//! The `Drive` handle — the high-level API that composes the tree CRDT (A), the content map (B),
//! and the content store into the operations the CLI and (later) the mount/web faces call.
//!
//! In M1 a `Drive` is **single-writer**: this device owns the move log and the content map. The log
//! is applied through [`DriveTree`] (so the dormant undo/redo and cycle-skip are exercised exactly as
//! they will be once promoted to multi-writer), and persisted locally so the CLI is usable offline.
//! The networked path (publishing ops on a ce-coord `Replicated<DriveTree>` so readers converge) is a
//! thin wrapper over the same op stream — see [`Drive::ops`] / [`Drive::apply_remote`], which are the
//! integration seam to ce-coord. Keeping the op log explicit here means the same bytes drive both the
//! local single-writer CLI and a future `Replicated<DriveTree>` writer with no model change.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use ce_coord::StateMachine;
use serde::{Deserialize, Serialize};

use crate::content::{ContentMap, ContentOp, FileContent};
use crate::tree::{DriveTree, MoveOp, NodeKind, ROOT, TRASH, Timestamp};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A monotonic local id generator for fresh nodes: `<lamport-hex><rand-hex>`, globally unique enough
/// (lamport is per-replica-monotone, the random suffix avoids same-tick collisions).
fn fresh_node_id(lamport: u64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{lamport:012x}{c:08x}")
}

/// The persisted state of a single-writer drive: the ordered move log and the content ops. Replaying
/// both reconstructs the [`DriveTree`] and [`ContentMap`] deterministically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriveState {
    /// This drive's name/id.
    pub name: String,
    /// The writer (this device) NodeId hex — the replica tiebreak in every [`Timestamp`].
    pub replica: String,
    /// The next Lamport tick to assign.
    pub lamport: u64,
    /// The ordered move-op log (collection A).
    pub move_log: Vec<MoveOp>,
    /// The ordered content-op log (collection B).
    pub content_log: Vec<ContentOp>,
}

/// A live single-writer drive: the replayed tree + content map plus the persisted [`DriveState`].
pub struct Drive {
    state: DriveState,
    tree: DriveTree,
    content: ContentMap,
}

impl Drive {
    /// Initialize a fresh drive owned by `replica` (this device's NodeId hex).
    pub fn init(name: &str, replica: &str) -> Self {
        let state = DriveState {
            name: name.to_string(),
            replica: replica.to_string(),
            lamport: 0,
            move_log: Vec::new(),
            content_log: Vec::new(),
        };
        Drive { state, tree: DriveTree::new(), content: ContentMap::new() }
    }

    /// Rebuild a drive by replaying a persisted [`DriveState`].
    pub fn from_state(state: DriveState) -> Self {
        let mut tree = DriveTree::new();
        for op in &state.move_log {
            tree.apply(op.clone());
        }
        let mut content = ContentMap::new();
        for op in &state.content_log {
            content.apply(op.clone());
        }
        Drive { state, tree, content }
    }

    /// The persisted state (for saving).
    pub fn state(&self) -> &DriveState {
        &self.state
    }

    /// The live tree (read-only view).
    pub fn tree(&self) -> &DriveTree {
        &self.tree
    }

    /// The live content map (read-only view).
    pub fn content(&self) -> &ContentMap {
        &self.content
    }

    /// The full move-op stream (collection A) — the integration seam for a ce-coord
    /// `Replicated<DriveTree>` writer: publish each as it is appended.
    pub fn ops(&self) -> &[MoveOp] {
        &self.state.move_log
    }

    /// Allocate the next timestamp (advances the Lamport clock).
    fn tick(&mut self) -> Timestamp {
        self.state.lamport += 1;
        Timestamp::new(self.state.lamport, self.state.replica.clone())
    }

    /// Apply + persist a move op (writer side).
    fn push_move(&mut self, op: MoveOp) {
        self.tree.apply(op.clone());
        self.state.move_log.push(op);
    }

    /// Apply + persist a content op (writer side).
    fn push_content(&mut self, op: ContentOp) {
        self.content.apply(op.clone());
        self.state.content_log.push(op);
    }

    /// Apply a remote move op (reader side / multi-writer merge): integrate it and record it. The
    /// Lamport clock advances to stay ahead of any seen op (standard Lamport merge).
    pub fn apply_remote_move(&mut self, op: MoveOp) {
        self.state.lamport = self.state.lamport.max(op.ts.lamport) + 1;
        self.tree.apply(op.clone());
        self.state.move_log.push(op);
    }

    /// Apply a remote content op.
    pub fn apply_remote_content(&mut self, op: ContentOp) {
        self.content.apply(op.clone());
        self.state.content_log.push(op);
    }

    // ---- structural operations (one MoveOp each) ----

    /// Create a directory under `parent_path`, returning its node id.
    pub fn mkdir(&mut self, parent_path: &str, name: &str) -> Result<String> {
        let parent = self.resolve_dir(parent_path)?;
        if self.child_named(&parent, name).is_some() {
            bail!("'{name}' already exists in '{parent_path}'");
        }
        let ts = self.tick();
        let id = fresh_node_id(ts.lamport);
        self.push_move(MoveOp { ts, child: id.clone(), new_parent: parent, new_name: name.to_string(), kind: NodeKind::Dir });
        Ok(id)
    }

    /// Add a file node under `parent_path` with the given content (already stored). Returns the new
    /// node id. The [`FileContent`] is recorded in the content map keyed by that id.
    pub fn add_file(&mut self, parent_path: &str, name: &str, content: FileContent) -> Result<String> {
        let parent = self.resolve_dir(parent_path)?;
        // If a file with this name already exists, treat it as a new version of that file (edit),
        // keyed by the SAME node id — so renames and edits never collide.
        if let Some(existing) = self.child_named(&parent, name) {
            if matches!(self.tree.edge(&existing).map(|e| &e.kind), Some(NodeKind::File)) {
                self.push_content(ContentOp::Set {
                    id: existing.clone(),
                    cid: content.cid,
                    size: content.size,
                    mode: content.mode,
                    mtime_ms: content.mtime_ms,
                });
                return Ok(existing);
            }
            bail!("'{name}' already exists as a directory in '{parent_path}'");
        }
        let ts = self.tick();
        let id = fresh_node_id(ts.lamport);
        self.push_move(MoveOp { ts, child: id.clone(), new_parent: parent, new_name: name.to_string(), kind: NodeKind::File });
        self.push_content(ContentOp::Set {
            id: id.clone(),
            cid: content.cid,
            size: content.size,
            mode: content.mode,
            mtime_ms: content.mtime_ms,
        });
        Ok(id)
    }

    /// Move/rename a node from `src_path` to `dst_parent_path`/`dst_name`. One `MoveOp`.
    pub fn mv(&mut self, src_path: &str, dst_parent_path: &str, dst_name: &str) -> Result<()> {
        let node = self.tree.resolve(src_path).ok_or_else(|| anyhow::anyhow!("no such path: {src_path}"))?;
        if node == ROOT {
            bail!("cannot move the root");
        }
        let dst_parent = self.resolve_dir(dst_parent_path)?;
        let kind = self.tree.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
        let ts = self.tick();
        self.push_move(MoveOp { ts, child: node, new_parent: dst_parent, new_name: dst_name.to_string(), kind });
        Ok(())
    }

    /// Delete a node (move to TRASH). Recoverable until GC. One `MoveOp`.
    pub fn rm(&mut self, path: &str) -> Result<()> {
        let node = self.tree.resolve(path).ok_or_else(|| anyhow::anyhow!("no such path: {path}"))?;
        if node == ROOT {
            bail!("cannot delete the root");
        }
        let name = self.tree.edge(&node).map(|e| e.name.clone()).unwrap_or_default();
        let kind = self.tree.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
        let ts = self.tick();
        self.push_move(MoveOp { ts, child: node, new_parent: TRASH.to_string(), new_name: name, kind });
        Ok(())
    }

    /// Record new content for an existing file node by its **stable NodeId** (the write-back path the
    /// mount uses on flush). Unlike [`Drive::add_file`], which resolves by path+name, this keys
    /// directly by id — so a file being concurrently renamed never loses the write (collection (B)
    /// orthogonality). A `MoveOp` is never emitted; only a content op. No-op if `id` is unknown to
    /// the content map and tree.
    pub fn set_content(&mut self, id: &str, content: FileContent) {
        self.push_content(ContentOp::Set {
            id: id.to_string(),
            cid: content.cid,
            size: content.size,
            mode: content.mode,
            mtime_ms: content.mtime_ms,
        });
    }

    /// Restore a file node to a prior version CID.
    pub fn restore_version(&mut self, path: &str, cid: &str) -> Result<()> {
        let node = self.tree.resolve(path).ok_or_else(|| anyhow::anyhow!("no such path: {path}"))?;
        if self.content.get(&node).is_none() {
            bail!("'{path}' has no content history");
        }
        self.push_content(ContentOp::Restore { id: node, cid: cid.to_string(), now_ms: now_ms() });
        Ok(())
    }

    // ---- queries ----

    /// List a directory: `(rendered_name, is_dir, content)` sorted by name. `content` is `None` for
    /// directories and for files with no recorded content yet.
    pub fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        let dir = self.resolve_dir(path)?;
        Ok(self
            .tree
            .readdir(&dir)
            .into_iter()
            .map(|(name, id, kind)| DirEntry {
                name,
                is_dir: matches!(kind, NodeKind::Dir),
                content: self.content.get(&id).cloned(),
                node_id: id,
            })
            .collect())
    }

    /// The derived absolute path of a node id.
    pub fn path_of(&self, node_id: &str) -> Option<String> {
        self.tree.path(node_id)
    }

    /// Every distinct CID the drive references (durability roots).
    pub fn all_cids(&self) -> Vec<String> {
        self.content.all_cids()
    }

    /// Resolve a path that must be an existing directory (or ROOT). Errors if it is a file.
    fn resolve_dir(&self, path: &str) -> Result<String> {
        let node = self.tree.resolve(path).ok_or_else(|| anyhow::anyhow!("no such directory: {path}"))?;
        if node != ROOT && !matches!(self.tree.edge(&node).map(|e| &e.kind), Some(NodeKind::Dir)) {
            bail!("'{path}' is not a directory");
        }
        Ok(node)
    }

    /// The live child of `parent` with the given bare name, if any.
    fn child_named(&self, parent: &str, name: &str) -> Option<String> {
        self.tree
            .raw_children(parent)
            .into_iter()
            .find(|(n, id)| n == name && !self.tree.is_trashed(id))
            .map(|(_, id)| id)
    }
}

/// One listing entry from [`Drive::ls`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub node_id: String,
    pub is_dir: bool,
    pub content: Option<FileContent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(cid: &str, size: u64) -> FileContent {
        FileContent::new(cid, size, 0o644, now_ms())
    }

    #[test]
    fn init_mkdir_add_ls() {
        let mut d = Drive::init("work", "replica-a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "spec.md", fc("cid1", 100)).unwrap();
        let entries = d.ls("/docs").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "spec.md");
        assert_eq!(entries[0].content.as_ref().unwrap().cid, "cid1");
        assert_eq!(d.ls("/").unwrap()[0].name, "docs");
    }

    #[test]
    fn rename_keeps_content_identity() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("cid1", 10)).unwrap();
        let id = d.tree.resolve("/a.md").unwrap();
        d.mv("/a.md", "/", "b.md").unwrap();
        // Same node id after rename; content unchanged.
        assert_eq!(d.tree.resolve("/b.md").as_deref(), Some(id.as_str()));
        assert_eq!(d.content.get(&id).unwrap().cid, "cid1");
    }

    #[test]
    fn dir_move_is_single_op() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "x.txt", fc("c", 1)).unwrap();
        d.mkdir("/", "archive").unwrap();
        let before = d.ops().len();
        d.mv("/docs", "/archive", "docs").unwrap();
        assert_eq!(d.ops().len(), before + 1, "dir move is exactly one op");
        assert_eq!(d.tree.path(&d.tree.resolve("/archive/docs/x.txt").unwrap()).as_deref(), Some("/archive/docs/x.txt"));
    }

    #[test]
    fn edit_is_new_version_same_id() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("cid1", 10)).unwrap();
        let id = d.tree.resolve("/a.md").unwrap();
        // "Adding" the same name again = an edit (new content version) on the same id.
        d.add_file("/", "a.md", fc("cid2", 20)).unwrap();
        assert_eq!(d.tree.resolve("/a.md").as_deref(), Some(id.as_str()));
        let c = d.content.get(&id).unwrap();
        assert_eq!(c.cid, "cid2");
        assert!(c.versions.iter().any(|v| v.cid == "cid1"), "old version retained");
    }

    #[test]
    fn rm_moves_to_trash_and_hides() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("c", 1)).unwrap();
        d.rm("/a.md").unwrap();
        assert!(d.ls("/").unwrap().is_empty());
    }

    #[test]
    fn from_state_replays_identically() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "x", fc("c1", 1)).unwrap();
        d.mv("/docs/x", "/", "y").unwrap();
        let state = d.state().clone();
        let d2 = Drive::from_state(state);
        assert_eq!(d.ls("/").unwrap(), d2.ls("/").unwrap());
        assert_eq!(d.ls("/docs").unwrap(), d2.ls("/docs").unwrap());
    }

    #[test]
    fn restore_version_points_back() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("cid1", 10)).unwrap();
        d.add_file("/", "a.md", fc("cid2", 20)).unwrap();
        d.restore_version("/a.md", "cid1").unwrap();
        let id = d.tree.resolve("/a.md").unwrap();
        assert_eq!(d.content.get(&id).unwrap().cid, "cid1");
    }

    #[test]
    fn two_writers_converge_on_concurrent_moves() {
        // Writer A and writer B each build the same base then issue a concurrent structural op; both
        // converge to the same tree after merging each other's ops (apply_remote_move).
        let mut a = Drive::init("w", "aaaa");
        a.mkdir("/", "X").unwrap();
        a.mkdir("/", "Y").unwrap();
        // Snapshot the base ops and replay them into B so both share identical history.
        let base_ops = a.ops().to_vec();
        let mut b = Drive::init("w", "bbbb");
        for op in &base_ops {
            b.apply_remote_move(op.clone());
        }
        // Resolve ids on both (same ids, since ops carry them).
        let x = a.tree.resolve("/X").unwrap();
        // A moves Y under X; B moves X under Y — a classic concurrent swap.
        a.mv("/Y", "/X", "Y").unwrap();
        b.mv("/X", "/Y", "X").unwrap();
        let a_op = a.ops().last().unwrap().clone();
        let b_op = b.ops().last().unwrap().clone();
        // Exchange the concurrent ops.
        a.apply_remote_move(b_op);
        b.apply_remote_move(a_op);
        // Both trees converge identically and remain acyclic.
        let ap: std::collections::BTreeMap<_, _> = a.tree.edges().keys().map(|n| (n.clone(), a.tree.path(n))).collect();
        let bp: std::collections::BTreeMap<_, _> = b.tree.edges().keys().map(|n| (n.clone(), b.tree.path(n))).collect();
        assert_eq!(ap, bp, "concurrent-move writers converge identically");
        let _ = x;
    }
}
