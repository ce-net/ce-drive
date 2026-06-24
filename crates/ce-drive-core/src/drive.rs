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

use crate::changes::{Change, ChangeLog, Cursor};
use crate::content::{ContentMap, ContentOp, FileContent};
use crate::names::check_name;
use crate::sync::StampedContentOp;
use crate::tree::{DriveTree, LIMBO, MoveOp, NodeKind, ROOT, TRASH, Timestamp};

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

/// Fold a content-op log into a [`ContentMap`] in strict ascending-`ts` order. The log is normally
/// already sorted (writes append a monotonically increasing tick; remote merges insert in order), but
/// we sort a clone defensively so the fold is a pure function of the op *set* — the same set always
/// yields the same map regardless of how it was assembled. This is what makes replicas converge.
fn fold_content(log: &[StampedContentOp]) -> ContentMap {
    let mut ordered: Vec<&StampedContentOp> = log.iter().collect();
    ordered.sort_by(|a, b| a.ts.cmp(&b.ts));
    let mut content = ContentMap::new();
    for stamped in ordered {
        content.apply(stamped.op.clone());
    }
    content
}

/// The node id an op targets (every [`ContentOp`] variant keys by a single node).
fn op_target(op: &ContentOp) -> &str {
    match op {
        ContentOp::Set { id, .. }
        | ContentOp::Restore { id, .. }
        | ContentOp::Remove { id }
        | ContentOp::SetDoc { id, .. } => id,
    }
}

/// Re-fold the content record for a **single** node id from the ts-ordered subset of the log that
/// targets it, and splice the result into `content`. This is the incremental counterpart of
/// [`fold_content`]: applying a remote op only re-derives the one affected key (O(ops-for-that-key))
/// instead of re-folding the entire map (O(total-ops)) on every merge. Convergence is preserved
/// because the per-key result is a pure function of that key's op subset in canonical ts order — and
/// no [`ContentOp`] variant ever reads or writes a *different* key, so keys are independent.
fn refold_key(content: &mut ContentMap, log: &[StampedContentOp], id: &str) {
    let mut subset: Vec<&StampedContentOp> =
        log.iter().filter(|s| op_target(&s.op) == id).collect();
    subset.sort_by(|a, b| a.ts.cmp(&b.ts));
    content.remove(id);
    for s in subset {
        content.apply(s.op.clone());
    }
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
    /// The content-op log (collection B), each op tagged with its total-order [`Timestamp`]. Folding
    /// it in ascending-`ts` order makes the content map LWW-by-timestamp and therefore *order
    /// independent*: two replicas that receive the same content ops in different arrival orders
    /// converge to identical state.
    pub content_log: Vec<StampedContentOp>,
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
        let content = fold_content(&state.content_log);
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

    /// Apply + persist a content op (writer side). The op is stamped with a fresh total-order
    /// [`Timestamp`] (advancing the Lamport clock) so the content log stays a strictly increasing,
    /// order-independent sequence — local writes are LWW-comparable with remote ops by the same key.
    fn push_content(&mut self, op: ContentOp) {
        let ts = self.tick();
        let stamped = StampedContentOp { ts, op };
        self.content.apply(stamped.op.clone());
        self.state.content_log.push(stamped);
    }

    /// Apply a remote move op (reader side / multi-writer merge): integrate it and record it. The
    /// Lamport clock advances to stay ahead of any seen op (standard Lamport merge).
    pub fn apply_remote_move(&mut self, op: MoveOp) {
        self.state.lamport = self.state.lamport.max(op.ts.lamport) + 1;
        self.tree.apply(op.clone());
        self.state.move_log.push(op);
    }

    /// Apply a remote content op (reader side / multi-writer merge). The op carries its own total-order
    /// [`Timestamp`]; it is inserted into the content log in ascending-`ts` order and the content map
    /// is re-folded from the ts-ordered log. Because [`ContentMap`] is LWW-by-`(mtime, cid)` and the
    /// fold visits ops in a single canonical order on *every* replica, two replicas that receive the
    /// same content ops in different arrival orders converge to identical state. The Lamport clock
    /// advances past the op so this device's next write strictly post-dates it. Duplicate ops (same
    /// `ts`) are ignored — the log stays a deduped, ts-ordered set.
    pub fn apply_remote_content(&mut self, op: StampedContentOp) {
        self.state.lamport = self.state.lamport.max(op.ts.lamport) + 1;
        // Locate the insertion point by total order; ignore exact duplicates (idempotent merge).
        let pos = self.state.content_log.partition_point(|existing| existing.ts < op.ts);
        if self.state.content_log.get(pos).map(|e| e.ts == op.ts).unwrap_or(false) {
            return;
        }
        let id = op_target(&op.op).to_string();
        self.state.content_log.insert(pos, op);
        // Re-fold only the affected key from its ts-ordered op subset (the other keys are unchanged,
        // since no ContentOp variant ever touches a different node id). Convergent and O(ops-for-key)
        // rather than O(total-ops): a long-lived drive merging remote ops stays linear, not quadratic.
        refold_key(&mut self.content, &self.state.content_log, &id);
    }

    // ---- structural operations (one MoveOp each) ----

    /// Create a directory under `parent_path`, returning its node id.
    pub fn mkdir(&mut self, parent_path: &str, name: &str) -> Result<String> {
        check_name(name)?;
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
        check_name(name)?;
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
        check_name(dst_name)?;
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

    /// List the trash: every node that currently lives directly under [`TRASH`], as [`DirEntry`]s
    /// (with their original bare names). These are the user-visible "deleted but recoverable" items.
    pub fn trash(&self) -> Vec<DirEntry> {
        self.tree
            .readdir(TRASH)
            .into_iter()
            .map(|(name, id, kind)| DirEntry {
                name,
                is_dir: matches!(kind, NodeKind::Dir),
                content: self.content.get(&id).cloned(),
                node_id: id,
            })
            .collect()
    }

    /// Restore a trashed node back to a live directory. `dst_parent_path` defaults to `/` when `None`;
    /// `dst_name` defaults to the node's current (trashed) name when `None`. Errors if the node is not
    /// in the trash, or the destination is not a directory, or the name collides with a live child.
    pub fn restore(
        &mut self,
        node_id: &str,
        dst_parent_path: Option<&str>,
        dst_name: Option<&str>,
    ) -> Result<()> {
        if !self.tree.is_trashed(node_id) {
            bail!("node '{node_id}' is not in the trash");
        }
        let cur_name = self.tree.edge(node_id).map(|e| e.name.clone()).unwrap_or_default();
        let name = dst_name.map(|s| s.to_string()).unwrap_or(cur_name);
        check_name(&name)?;
        let parent_path = dst_parent_path.unwrap_or("/");
        let parent = self.resolve_dir(parent_path)?;
        if self.child_named(&parent, &name).is_some() {
            bail!("'{name}' already exists in '{parent_path}'");
        }
        let kind = self.tree.edge(node_id).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
        let ts = self.tick();
        self.push_move(MoveOp {
            ts,
            child: node_id.to_string(),
            new_parent: parent,
            new_name: name,
            kind,
        });
        Ok(())
    }

    /// Permanently remove every trashed node and emit a [`ContentOp::Remove`] for each so the content
    /// record (and thus its CIDs) becomes a GC candidate. Returns the node ids hard-deleted. The
    /// node's edge is dropped from the tree by moving it to a private limbo parent (it stops being a
    /// trash child and is no longer reachable), and its content record is removed. Idempotent.
    pub fn empty_trash(&mut self) -> Vec<String> {
        let ids: Vec<String> = self.tree.readdir(TRASH).into_iter().map(|(_, id, _)| id).collect();
        self.hard_delete(&ids);
        ids
    }

    /// Garbage-collect trashed nodes whose deletion is older than `retention_secs` relative to `now`
    /// (unix seconds). A node's "deletion time" is the wall-clock derived from the timestamp of the
    /// move that put it in the trash — but Lamport ticks are not wall-clock, so callers that need a
    /// true wall-clock retention should pass content-record mtimes via [`Drive::gc_trash_by_mtime`].
    /// This variant GCs every trashed node (retention 0) or, when `retention_secs > 0`, only those
    /// whose newest content mtime is older than the cutoff. Returns the hard-deleted node ids.
    pub fn gc_trash(&mut self, now_secs: u64, retention_secs: u64) -> Vec<String> {
        if retention_secs == 0 {
            return self.empty_trash();
        }
        let cutoff_ms = now_secs.saturating_sub(retention_secs).saturating_mul(1000);
        let victims: Vec<String> = self
            .tree
            .readdir(TRASH)
            .into_iter()
            .filter(|(_, id, _)| {
                // A node with no content (e.g. a dir) GCs immediately; a file GCs once its newest
                // content mtime predates the cutoff.
                match self.content.get(id) {
                    Some(c) => c.mtime_ms <= cutoff_ms,
                    None => true,
                }
            })
            .map(|(_, id, _)| id)
            .collect();
        self.hard_delete(&victims);
        victims
    }

    /// Hard-delete the given node ids: detach each from the tree (move to a private, unreachable
    /// limbo id so it leaves the trash listing and derives no path) and remove its content record.
    fn hard_delete(&mut self, ids: &[String]) {
        for id in ids {
            // Detach: a move to a reserved limbo parent the tree never lists or resolves.
            let kind = self.tree.edge(id).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
            let ts = self.tick();
            self.push_move(MoveOp {
                ts,
                child: id.clone(),
                new_parent: LIMBO.to_string(),
                new_name: id.clone(),
                kind,
            });
            // Drop the content record (and emit the op so peers converge on the removal).
            if self.content.get(id).is_some() {
                self.push_content(ContentOp::Remove { id: id.clone() });
            }
        }
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

    /// Every live file that currently carries an unresolved concurrent-edit conflict, as
    /// `(path, current_cid, conflict_cids)`. The UX renders each `conflict_cid` as a sibling
    /// `<name>.conflict-<short-cid>` copy so a user sees both sides of a concurrent edit (Dropbox
    /// style) instead of one silently winning LWW. Resolving = picking one (a normal save) — the
    /// loser stays in version history.
    pub fn content_conflicts(&self) -> Vec<ContentConflict> {
        let mut out = Vec::new();
        for (id, content) in self.content.entries() {
            if !content.has_conflict() {
                continue;
            }
            // Only surface conflicts for nodes that are live (not trashed/limbo).
            let Some(path) = self.tree.path(id) else { continue };
            out.push(ContentConflict {
                node_id: id.clone(),
                path,
                current_cid: content.cid.clone(),
                conflict_cids: content.conflict_versions().iter().map(|v| v.cid.clone()).collect(),
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
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

    /// The change feed since `cursor`: up to `limit` changes (0 = unbounded) in ascending timestamp
    /// order plus the cursor to poll with next. Powers incremental client sync and the activity feed.
    pub fn changes_since(&self, cursor: &Cursor, limit: usize) -> (Vec<Change>, Cursor) {
        ChangeLog::new(&self.state, &self.tree).changes_since(cursor, limit)
    }

    /// The current head cursor — open here to receive only *future* changes (skip history).
    pub fn changes_head(&self) -> Cursor {
        ChangeLog::new(&self.state, &self.tree).head()
    }

    /// Build a fresh filename/metadata [`SearchIndex`](crate::search::SearchIndex) over the live tree.
    pub fn search_index(&self) -> crate::search::SearchIndex {
        crate::search::SearchIndex::build(self)
    }

    /// Compact the persisted state: replace the full move/content op history with a **minimal
    /// equivalent log** that reconstructs the current live tree + content map. This bounds the state
    /// file size and replay time for a long-lived single-writer drive (the multi-writer
    /// [`SyncedDrive`](crate::sync::SyncedDrive) has its own ce-coord checkpoint path).
    ///
    /// The compacted log is a deterministic, ts-ordered sequence: for every live node (BFS from ROOT,
    /// parents before children so each move's parent already exists) one create [`MoveOp`], and for
    /// every content record one [`ContentOp::Set`] plus its retained version history as ordered
    /// `Set`/`Restore` ops. Trashed and limbo nodes are dropped (their history is intentionally
    /// discarded — compaction is the GC boundary). Lamport is reset to the new log length. The
    /// reconstructed drive lists identically to the original for all live content.
    pub fn compact(&mut self) {
        let mut new_moves: Vec<MoveOp> = Vec::new();
        let mut new_content: Vec<StampedContentOp> = Vec::new();
        let mut lamport: u64 = 0;
        let replica = self.state.replica.clone();
        let next_ts = |lamport: &mut u64| {
            *lamport += 1;
            Timestamp::new(*lamport, replica.clone())
        };

        // BFS the live tree from ROOT so parents are created before children.
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::from([ROOT.to_string()]);
        while let Some(parent) = queue.pop_front() {
            // raw_children gives bare (name, id); skip trashed/limbo.
            let mut kids = self.tree.raw_children(&parent);
            kids.sort();
            for (name, id) in kids {
                if self.tree.is_trashed(&id) || self.tree.is_limbo(&id) {
                    continue;
                }
                let kind = self.tree.edge(&id).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
                new_moves.push(MoveOp {
                    ts: next_ts(&mut lamport),
                    child: id.clone(),
                    new_parent: parent.clone(),
                    new_name: name,
                    kind: kind.clone(),
                });
                if matches!(kind, NodeKind::Dir) {
                    queue.push_back(id.clone());
                }
                // Content for this node (with version history, oldest version first then current).
                if let Some(c) = self.content.get(&id) {
                    for v in &c.versions {
                        new_content.push(StampedContentOp {
                            ts: next_ts(&mut lamport),
                            op: ContentOp::Set {
                                id: id.clone(),
                                cid: v.cid.clone(),
                                size: v.size,
                                mode: c.mode,
                                mtime_ms: v.set_at_ms,
                            },
                        });
                    }
                    new_content.push(StampedContentOp {
                        ts: next_ts(&mut lamport),
                        op: ContentOp::Set {
                            id: id.clone(),
                            cid: c.cid.clone(),
                            size: c.size,
                            mode: c.mode,
                            mtime_ms: c.mtime_ms,
                        },
                    });
                    if let Some(doc_id) = &c.doc_id {
                        new_content.push(StampedContentOp {
                            ts: next_ts(&mut lamport),
                            op: ContentOp::SetDoc { id: id.clone(), doc_id: doc_id.clone() },
                        });
                    }
                }
            }
        }

        self.state.move_log = new_moves;
        self.state.content_log = new_content;
        self.state.lamport = lamport;
        // Rebuild the live views from the compacted log (identical live state, smaller history).
        let mut tree = DriveTree::new();
        for op in &self.state.move_log {
            tree.apply(op.clone());
        }
        self.tree = tree;
        self.content = fold_content(&self.state.content_log);
    }

    /// The total number of ops (move + content) currently persisted — the compaction trigger metric.
    pub fn op_count(&self) -> usize {
        self.state.move_log.len() + self.state.content_log.len()
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

/// A live file with an unresolved concurrent-edit conflict (from [`Drive::content_conflicts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentConflict {
    /// The conflicted file's node id.
    pub node_id: String,
    /// Its derived path.
    pub path: String,
    /// The CID that currently won LWW.
    pub current_cid: String,
    /// The CIDs of the concurrent edits that lost (rendered as `*.conflict` copies), newest-first.
    pub conflict_cids: Vec<String>,
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
    fn two_replicas_converge_on_content_ops_in_different_orders() {
        // Theme E regression: two replicas that apply the SAME set of content ops in DIFFERENT
        // arrival orders must converge to byte-identical content state. Before the fix,
        // `apply_remote_content` applied ops in arrival order with no timestamp, so the "current"
        // pointer (and mode) was last-applied — order dependent — and replicas DIVERGED.

        // Build a set of stamped content ops for one file id "f", produced by two writers with
        // interleaved Lamport ticks (a classic concurrent-edit history), plus a second file "g".
        fn cop(l: u64, r: &str, id: &str, cid: &str, mode: u32, mtime: u64) -> StampedContentOp {
            StampedContentOp {
                ts: Timestamp::new(l, r),
                op: ContentOp::Set { id: id.into(), cid: cid.into(), size: 1, mode, mtime_ms: mtime },
            }
        }
        let ops = [
            cop(1, "a", "f", "cid-a1", 0o644, 100),
            cop(2, "b", "f", "cid-b1", 0o600, 250), // newest mtime => should win LWW for "f"
            cop(3, "a", "f", "cid-a2", 0o640, 150), // older mtime than b1 => loses, but retained
            cop(2, "a", "g", "cid-g1", 0o644, 90),
            cop(4, "b", "g", "cid-g2", 0o755, 90),  // same mtime, cid tiebreak (cid-g2 > cid-g1)
        ];

        // Replica 1 receives them in forward order; replica 2 in reverse order. Neither is the
        // local writer for these ops — both arrive via apply_remote_content.
        let mut r1 = Drive::init("w", "r1");
        for op in ops.iter() {
            r1.apply_remote_content(op.clone());
        }
        let mut r2 = Drive::init("w", "r2");
        for op in ops.iter().rev() {
            r2.apply_remote_content(op.clone());
        }
        // A third permutation: shuffled-ish interleave.
        let mut r3 = Drive::init("w", "r3");
        for i in [3usize, 0, 4, 2, 1] {
            r3.apply_remote_content(ops[i].clone());
        }

        // All three converge: same current cid, size, mode, mtime, and full version history per file.
        for id in ["f", "g"] {
            let c1 = r1.content.get(id).unwrap();
            let c2 = r2.content.get(id).unwrap();
            let c3 = r3.content.get(id).unwrap();
            assert_eq!(c1, c2, "replicas 1 and 2 diverged on '{id}'");
            assert_eq!(c1, c3, "replicas 1 and 3 diverged on '{id}'");
        }
        // And LWW resolved as expected (newest mtime wins for f; cid tiebreak for g).
        assert_eq!(r1.content.get("f").unwrap().cid, "cid-b1");
        assert_eq!(r1.content.get("g").unwrap().cid, "cid-g2");
        // Idempotent: re-applying the same ops changes nothing (deduped by ts).
        let before = r1.content.get("f").unwrap().clone();
        for op in ops.iter() {
            r1.apply_remote_content(op.clone());
        }
        assert_eq!(r1.content.get("f").unwrap(), &before, "duplicate merge must be idempotent");
    }

    #[test]
    fn name_validation_rejects_bad_names() {
        let mut d = Drive::init("w", "a");
        assert!(d.mkdir("/", "").is_err(), "empty name rejected");
        assert!(d.mkdir("/", "..").is_err(), "dotdot rejected");
        assert!(d.mkdir("/", "a/b").is_err(), "separator rejected");
        assert!(d.add_file("/", "x\0y", fc("c", 1)).is_err(), "NUL rejected");
        assert!(d.add_file("/", &"z".repeat(300), fc("c", 1)).is_err(), "overlong rejected");
        // A valid name still works after the rejections (no partial state corruption).
        assert!(d.mkdir("/", "good").is_ok());
        // mv to a bad name is rejected too.
        d.add_file("/", "ok.txt", fc("c", 1)).unwrap();
        assert!(d.mv("/ok.txt", "/", "bad/name").is_err());
    }

    #[test]
    fn trash_restore_roundtrip() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "a.md", fc("cid1", 10)).unwrap();
        let id = d.tree.resolve("/docs/a.md").unwrap();
        d.rm("/docs/a.md").unwrap();
        assert!(d.ls("/docs").unwrap().is_empty(), "trashed file hidden");
        // It shows up in the trash listing.
        let trash = d.trash();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].node_id, id);
        // Restore to a different folder + name.
        d.restore(&id, Some("/"), Some("restored.md")).unwrap();
        assert_eq!(d.tree.resolve("/restored.md").as_deref(), Some(id.as_str()));
        assert!(d.trash().is_empty(), "trash now empty");
        // Content survived the round-trip.
        assert_eq!(d.content.get(&id).unwrap().cid, "cid1");
    }

    #[test]
    fn restore_rejects_collision_and_non_trashed() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("c", 1)).unwrap();
        let id = d.tree.resolve("/a.md").unwrap();
        // Not in trash yet.
        assert!(d.restore(&id, None, None).is_err());
        d.rm("/a.md").unwrap();
        // Re-create a live file with the same name, then restoring should collide.
        d.add_file("/", "a.md", fc("c2", 2)).unwrap();
        assert!(d.restore(&id, Some("/"), Some("a.md")).is_err(), "name collision rejected");
        // Restoring under a free name works.
        assert!(d.restore(&id, Some("/"), Some("a-old.md")).is_ok());
    }

    #[test]
    fn empty_trash_hard_deletes_and_unpins() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.md", fc("cid1", 10)).unwrap();
        let id = d.tree.resolve("/a.md").unwrap();
        assert!(d.all_cids().contains(&"cid1".to_string()));
        d.rm("/a.md").unwrap();
        let deleted = d.empty_trash();
        assert_eq!(deleted, vec![id.clone()]);
        // Gone from trash, content record removed, CID no longer referenced (GC candidate).
        assert!(d.trash().is_empty());
        assert!(d.content.get(&id).is_none(), "content record removed");
        assert!(!d.all_cids().contains(&"cid1".to_string()), "CID unpinned");
        assert!(d.tree.is_limbo(&id), "node detached to limbo");
        // Idempotent.
        assert!(d.empty_trash().is_empty());
    }

    #[test]
    fn gc_trash_honors_retention() {
        let mut d = Drive::init("w", "a");
        // A file whose content mtime is t=1000ms.
        d.add_file("/", "old.md", FileContent::new("cid-old", 1, 0o644, 1_000)).unwrap();
        let old_id = d.tree.resolve("/old.md").unwrap();
        d.rm("/old.md").unwrap();
        // now=10 s, retention=5 s => cutoff = 5_000 ms. old.md (mtime 1000ms) is older => GC'd.
        let gced = d.gc_trash(10, 5);
        assert_eq!(gced, vec![old_id]);
        // A freshly-trashed file with a recent mtime is retained.
        d.add_file("/", "new.md", FileContent::new("cid-new", 1, 0o644, 9_000)).unwrap();
        d.rm("/new.md").unwrap();
        let kept = d.gc_trash(10, 5); // cutoff 5_000ms; new.md mtime 9_000ms > cutoff => kept
        assert!(kept.is_empty(), "recent deletion retained within window");
        assert_eq!(d.trash().len(), 1);
    }

    #[test]
    fn incremental_refold_matches_full_fold() {
        // Build a content history for several keys via apply_remote_content (incremental path), then
        // assert the resulting map equals a from-scratch full fold of the same op set.
        fn cop(l: u64, r: &str, id: &str, cid: &str, mtime: u64) -> StampedContentOp {
            StampedContentOp {
                ts: Timestamp::new(l, r),
                op: ContentOp::Set { id: id.into(), cid: cid.into(), size: 1, mode: 0o644, mtime_ms: mtime },
            }
        }
        let ops = [
            cop(1, "a", "f", "c1", 100),
            cop(3, "b", "f", "c3", 300),
            cop(2, "a", "f", "c2", 200), // arrives out of order (middle insert)
            cop(2, "b", "g", "cg1", 100),
            cop(4, "a", "g", "cg2", 50), // older mtime, loses LWW but retained
        ];
        let mut d = Drive::init("w", "z");
        for op in ops.iter().rev() {
            d.apply_remote_content(op.clone());
        }
        let full = fold_content(&d.state.content_log);
        for id in ["f", "g"] {
            assert_eq!(d.content.get(id), full.get(id), "incremental refold diverged on '{id}'");
        }
        assert_eq!(d.content.get("f").unwrap().cid, "c3", "newest mtime wins");
    }

    #[test]
    fn concurrent_content_edit_surfaces_conflict() {
        // Two writers edit the SAME file id with the SAME mtime but different cids — a genuine
        // concurrent edit. One wins LWW (cid tiebreak); the loser is surfaced as a conflict copy.
        fn cop(l: u64, r: &str, id: &str, cid: &str, mtime: u64) -> StampedContentOp {
            StampedContentOp {
                ts: Timestamp::new(l, r),
                op: ContentOp::Set { id: id.into(), cid: cid.into(), size: 1, mode: 0o644, mtime_ms: mtime },
            }
        }
        let mut d = Drive::init("w", "a");
        // Base content at mtime 1000 (older than the concurrent edits), so the two later edits at the
        // SAME mtime (2000) are a genuine concurrent edit against each other (not stale history).
        d.add_file("/", "doc.md", FileContent::new("base", 1, 0o644, 1000)).unwrap();
        let id = d.tree.resolve("/doc.md").unwrap();
        // Two concurrent edits with the same mtime (=2000) but different cids.
        d.apply_remote_content(cop(10, "x", &id, "cid-aaa", 2000));
        d.apply_remote_content(cop(11, "y", &id, "cid-zzz", 2000));
        // The conflict surface lists the file with the losing cid as a conflict copy.
        let conflicts = d.content_conflicts();
        assert_eq!(conflicts.len(), 1, "one conflicted file");
        assert_eq!(conflicts[0].path, "/doc.md");
        // cid-zzz > cid-aaa wins the tiebreak; cid-aaa is the conflict copy.
        assert_eq!(conflicts[0].current_cid, "cid-zzz");
        assert!(conflicts[0].conflict_cids.contains(&"cid-aaa".to_string()));
    }

    #[test]
    fn sequential_edits_are_not_conflicts() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "doc.md", FileContent::new("v0", 1, 0o644, 100)).unwrap();
        // Newer mtime each time — ordinary history, no conflict.
        d.add_file("/", "doc.md", FileContent::new("v1", 1, 0o644, 200)).unwrap();
        d.add_file("/", "doc.md", FileContent::new("v2", 1, 0o644, 300)).unwrap();
        assert!(d.content_conflicts().is_empty(), "sequential edits are not conflicts");
    }

    #[test]
    fn compact_preserves_live_state_and_shrinks_log() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "a.md", fc("cid1", 10)).unwrap();
        d.add_file("/docs", "a.md", fc("cid2", 20)).unwrap(); // version churn
        d.add_file("/docs", "a.md", fc("cid3", 30)).unwrap();
        d.mkdir("/", "tmp").unwrap();
        d.add_file("/tmp", "junk.txt", fc("junk", 5)).unwrap();
        d.rm("/tmp/junk.txt").unwrap();
        d.mv("/docs/a.md", "/", "moved.md").unwrap(); // moves create more ops
        let before = d.op_count();
        let live_before = d.ls("/").unwrap();
        let docs_before = d.ls("/docs").unwrap();
        let content_before = {
            let id = d.tree.resolve("/moved.md").unwrap();
            d.content.get(&id).unwrap().clone()
        };

        d.compact();
        // Live listings are identical after compaction.
        assert_eq!(d.ls("/").unwrap(), live_before);
        assert_eq!(d.ls("/docs").unwrap(), docs_before);
        // Current content + full version history preserved for the live file.
        let id = d.tree.resolve("/moved.md").unwrap();
        assert_eq!(d.content.get(&id).unwrap().cid, content_before.cid);
        assert_eq!(d.content.get(&id).unwrap().cid, "cid3");
        // Trashed junk is gone (compaction is the GC boundary).
        assert!(d.trash().is_empty());
        assert!(!d.all_cids().contains(&"junk".to_string()));
        // The log got smaller (history churn collapsed).
        assert!(d.op_count() < before, "compaction shrinks the op log: {} < {}", d.op_count(), before);
        // And the compacted state replays identically from scratch.
        let replayed = Drive::from_state(d.state().clone());
        assert_eq!(replayed.ls("/").unwrap(), d.ls("/").unwrap());
    }

    #[test]
    fn changes_since_reports_deltas() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.txt", fc("c1", 1)).unwrap();
        let head = d.changes_head();
        d.mkdir("/", "docs").unwrap();
        let (changes, _) = d.changes_since(&head, 0);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].kind, crate::changes::ChangeKind::Created { is_dir: true }));
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
