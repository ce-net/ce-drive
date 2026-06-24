//! Multi-writer sync — the leaderless, Raft-free convergence layer that promotes [`DriveTree`] and
//! the content map off the single-writer [`Drive`](crate::drive::Drive) onto ce-coord's
//! [`Merged`](ce_coord::Merged).
//!
//! ## The headline gap this closes
//! In M1 a [`Drive`](crate::drive::Drive) is single-writer: one device owns the move log and the
//! content map, everyone else is a reader. That stalls structural edits whenever the primary is
//! offline — exactly the local-first case CE targets. The genuine CRDT path (design §4.6, §13.2) is
//! **per-writer logs merged by Lamport timestamp**: every device appends to *its own* writer log and
//! readers take the key-ordered set union, so N devices converge with no leader and no quorum.
//!
//! ## How it maps onto [`Merged`]
//! [`Merged<M>`](ce_coord::Merged) folds a deduped, **key-ordered** union of every writer's ops into a
//! fresh `M::default()`, in ascending [`MergeMachine::key`] order. That is *precisely* the order
//! [`DriveTree::apply`] wants: fed strictly ascending, its dormant undo/redo never triggers (it is
//! append-only), and the cycle-skip resolves identically on every replica because the op *set* and
//! its total order are identical everywhere. So the move-CRDT is a [`MergeMachine`] for free — its
//! key is `MoveOp.ts` (Lamport + replica), a strict total order across all writers.
//!
//! The content map is made convergent the same way: each [`ContentOp`] is paired with a total-order
//! [`Timestamp`] key, and [`ContentMerge`] folds the key-ordered union — LWW-by-timestamp, identical
//! on every replica regardless of delivery order. Renaming a file (a `MoveOp` in the tree) while
//! editing its bytes (a content op keyed by the same stable NodeId) never collide: the two merged
//! collections are orthogonal.
//!
//! ## Snapshot / bootstrap
//! Each per-writer log under [`Merged`] is a `Replicated<WriterLog<Op>>`, and [`WriterLog`] is
//! `Snapshot`-able — so a fresh device bootstraps from a content-addressed checkpoint then tails,
//! rather than replaying every op from version 1 ([`SyncedDrive::checkpoint`]).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use ce_coord::snapshot::Checkpoint;
use ce_coord::{Coord, MergeMachine, Merged, StateMachine};
use serde::{Deserialize, Serialize};

use crate::content::{ContentMap, ContentOp, FileContent};
use crate::drive::DirEntry;
use crate::tree::{DriveTree, MoveOp, NodeId, NodeKind, ROOT, TRASH, Timestamp};

// =================================================================================================
// Merge machines: the deterministic, order-independent state machines `Merged` folds.
// =================================================================================================

/// The directory tree as a [`MergeMachine`]. Identical to [`DriveTree`] as a [`StateMachine`] — the
/// merge layer simply guarantees ops are folded in ascending `ts` order, which is exactly the
/// canonical order [`DriveTree::apply`] converges under (append-only fast-path, undo/redo dormant).
#[derive(Default)]
pub struct TreeMerge(pub DriveTree);

impl MergeMachine for TreeMerge {
    type Op = MoveOp;
    type Key = Timestamp;

    fn key(op: &MoveOp) -> Timestamp {
        op.ts.clone()
    }

    fn apply(&mut self, op: MoveOp) {
        self.0.apply(op);
    }
}

/// A content op tagged with its total-order key. The bare [`ContentOp`] is convergent under LWW only
/// once it carries a strict total order; pairing it with a [`Timestamp`] (Lamport + replica) gives
/// the same leaderless guarantee the tree enjoys. Distinct ops always have distinct keys, so the
/// merged union dedups cleanly and folds deterministically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampedContentOp {
    /// The strict total-order key (Lamport + replica). The merge fold visits ops in this order, so
    /// later-timestamp content wins LWW identically on every replica.
    pub ts: Timestamp,
    /// The underlying content mutation.
    pub op: ContentOp,
}

/// The content map as a [`MergeMachine`]. Folds the key-ordered union of [`StampedContentOp`]s into a
/// [`ContentMap`]; because [`ContentMap::apply`] is LWW-by-mtime and the fold visits ops in ascending
/// `ts`, the result is a pure function of the op set — convergent regardless of delivery order.
#[derive(Default)]
pub struct ContentMerge(pub ContentMap);

impl MergeMachine for ContentMerge {
    type Op = StampedContentOp;
    type Key = Timestamp;

    fn key(op: &StampedContentOp) -> Timestamp {
        op.ts.clone()
    }

    fn apply(&mut self, op: StampedContentOp) {
        self.0.apply(op.op);
    }
}

// =================================================================================================
// The synced drive handle.
// =================================================================================================

/// A live, multi-writer drive: this device's view of the tree + content map, converged leaderless
/// over ce-coord's [`Merged`]. Every device opens its own [`SyncedDrive`] with its NodeId and the
/// NodeIds of its peer writers; each `mkdir`/`mv`/`add`/`rm` appends to *this* device's writer log
/// and is broadcast, and [`refresh`](Self::refresh) folds the merged union of all writers into the
/// live tree + content map. Convergence is order-independent and needs no leader or quorum.
pub struct SyncedDrive {
    name: String,
    /// This device's NodeId hex — the replica tiebreak in every [`Timestamp`].
    replica: String,
    /// Lamport clock, advanced past every op this device sees (local or remote).
    lamport: Arc<Mutex<u64>>,
    /// The move-op merged log (collection A).
    tree: Merged<TreeMerge>,
    /// The content-op merged log (collection B).
    content: Merged<ContentMerge>,
}

impl SyncedDrive {
    /// Open the multi-writer drive `name` on this device. `peers` are the *other* member devices'
    /// NodeId hexes whose writer logs to follow. New members can be added later with
    /// [`add_writer`](Self::add_writer).
    pub async fn open(coord: &Coord, name: &str, peers: &[String]) -> Result<SyncedDrive> {
        let replica = coord.node_id().to_string();
        let tree =
            Merged::<TreeMerge>::open(coord, &format!("drive-{name}-tree"), &replica, peers).await?;
        let content =
            Merged::<ContentMerge>::open(coord, &format!("drive-{name}-content"), &replica, peers)
                .await?;
        Ok(SyncedDrive {
            name: name.to_string(),
            replica,
            lamport: Arc::new(Mutex::new(0)),
            tree,
            content,
        })
    }

    /// This drive's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// This device's replica id (NodeId hex).
    pub fn replica(&self) -> &str {
        &self.replica
    }

    /// Start following a newly-added peer writer's logs. Idempotent.
    pub async fn add_writer(&self, peer: &str) -> Result<()> {
        self.tree.add_writer(peer).await?;
        self.content.add_writer(peer).await?;
        Ok(())
    }

    /// Pull every op delivered so far (local + every peer) into the merged union — call before a read
    /// when you want to see the latest from peers without waiting on the watch. Cheap; idempotent.
    pub fn refresh(&self) {
        self.tree.pull();
        self.content.pull();
    }

    /// Allocate the next timestamp. The Lamport clock is bumped past everything seen so far, then by
    /// one — so this device's new op strictly post-dates every op it has observed.
    fn tick(&self) -> Timestamp {
        let mut l = self.lamport.lock().unwrap();
        *l += 1;
        Timestamp::new(*l, self.replica.clone())
    }

    /// Observe a remote timestamp so the next [`tick`](Self::tick) post-dates it (Lamport merge).
    fn observe(&self, ts: &Timestamp) {
        let mut l = self.lamport.lock().unwrap();
        *l = (*l).max(ts.lamport);
    }

    /// Fold the merged tree, advancing the Lamport clock past every op seen, and hand the converged
    /// [`DriveTree`] to `f`. Pulls first, so peers' ops are reflected.
    pub fn with_tree<R>(&self, f: impl FnOnce(&DriveTree) -> R) -> R {
        // Advance the clock past the highest ts in the union before reading, so subsequent local ops
        // are causally after everything observed.
        let max_ts = self.tree.read(|m| {
            // The union is key-ordered; the highest op key is the causal frontier.
            m.0.log_max_ts()
        });
        if let Some(ts) = max_ts {
            self.observe(&ts);
        }
        self.tree.read(|m| f(&m.0))
    }

    /// Fold the merged content map and hand it to `f`. Pulls first.
    pub fn with_content<R>(&self, f: impl FnOnce(&ContentMap) -> R) -> R {
        self.content.read(|m| f(&m.0))
    }

    /// Propose one move op on this device's writer log (and broadcast it). Folds locally immediately.
    async fn propose_move(&self, op: MoveOp) -> Result<()> {
        self.tree.propose(op).await?;
        Ok(())
    }

    /// Propose one content op, stamped with a fresh total-order key.
    async fn propose_content(&self, op: ContentOp) -> Result<()> {
        let ts = self.tick();
        self.content.propose(StampedContentOp { ts, op }).await?;
        Ok(())
    }

    // ---- structural operations (mirror the single-writer `Drive` API, but multi-writer) ----

    /// Create a directory under `parent_path`, returning its node id.
    pub async fn mkdir(&self, parent_path: &str, name: &str) -> Result<NodeId> {
        let parent = self.with_tree(|t| resolve_dir(t, parent_path))?;
        let ts = self.tick();
        let id = fresh_node_id(&ts);
        self.propose_move(MoveOp {
            ts,
            child: id.clone(),
            new_parent: parent,
            new_name: name.to_string(),
            kind: NodeKind::Dir,
        })
        .await?;
        Ok(id)
    }

    /// Add a file under `parent_path` with already-stored content. If a live file of that name exists
    /// it is treated as a new version (edit) on the *same* stable id. Returns the file's node id.
    pub async fn add_file(
        &self,
        parent_path: &str,
        name: &str,
        content: FileContent,
    ) -> Result<NodeId> {
        let (parent, existing) = self.with_tree(|t| {
            let parent = resolve_dir(t, parent_path)?;
            let existing = child_named(t, &parent, name);
            Ok::<_, anyhow::Error>((parent, existing))
        })?;
        if let Some(existing) = existing {
            // Edit the existing file: a content op keyed by its stable id, no MoveOp.
            self.propose_content(ContentOp::Set {
                id: existing.clone(),
                cid: content.cid,
                size: content.size,
                mode: content.mode,
                mtime_ms: content.mtime_ms,
            })
            .await?;
            return Ok(existing);
        }
        let ts = self.tick();
        let id = fresh_node_id(&ts);
        self.propose_move(MoveOp {
            ts,
            child: id.clone(),
            new_parent: parent,
            new_name: name.to_string(),
            kind: NodeKind::File,
        })
        .await?;
        self.propose_content(ContentOp::Set {
            id: id.clone(),
            cid: content.cid,
            size: content.size,
            mode: content.mode,
            mtime_ms: content.mtime_ms,
        })
        .await?;
        Ok(id)
    }

    /// Move/rename a node. One `MoveOp`.
    pub async fn mv(&self, src_path: &str, dst_parent_path: &str, dst_name: &str) -> Result<()> {
        let (node, dst_parent, kind) = self.with_tree(|t| {
            let node = t
                .resolve(src_path)
                .ok_or_else(|| anyhow::anyhow!("no such path: {src_path}"))?;
            if node == ROOT {
                anyhow::bail!("cannot move the root");
            }
            let dst_parent = resolve_dir(t, dst_parent_path)?;
            let kind = t.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
            Ok::<_, anyhow::Error>((node, dst_parent, kind))
        })?;
        let ts = self.tick();
        self.propose_move(MoveOp {
            ts,
            child: node,
            new_parent: dst_parent,
            new_name: dst_name.to_string(),
            kind,
        })
        .await
    }

    /// Delete a node (move to TRASH). One `MoveOp`.
    pub async fn rm(&self, path: &str) -> Result<()> {
        let (node, name, kind) = self.with_tree(|t| {
            let node =
                t.resolve(path).ok_or_else(|| anyhow::anyhow!("no such path: {path}"))?;
            if node == ROOT {
                anyhow::bail!("cannot delete the root");
            }
            let name = t.edge(&node).map(|e| e.name.clone()).unwrap_or_default();
            let kind = t.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
            Ok::<_, anyhow::Error>((node, name, kind))
        })?;
        let ts = self.tick();
        self.propose_move(MoveOp {
            ts,
            child: node,
            new_parent: TRASH.to_string(),
            new_name: name,
            kind,
        })
        .await
    }

    /// Record new content for an existing file by its stable NodeId (the mount write-back path).
    pub async fn set_content(&self, id: &str, content: FileContent) -> Result<()> {
        self.propose_content(ContentOp::Set {
            id: id.to_string(),
            cid: content.cid,
            size: content.size,
            mode: content.mode,
            mtime_ms: content.mtime_ms,
        })
        .await
    }

    // ---- queries ----

    /// List a directory: `(rendered_name, is_dir, content)` sorted by name.
    pub fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        let content = self.content.read(|m| m.0.clone());
        self.with_tree(|t| {
            let dir = resolve_dir(t, path)?;
            Ok(t.readdir(&dir)
                .into_iter()
                .map(|(name, id, kind)| DirEntry {
                    name,
                    is_dir: matches!(kind, NodeKind::Dir),
                    content: content.get(&id).cloned(),
                    node_id: id,
                })
                .collect())
        })
    }

    /// Resolve a path to its node id (merged tree).
    pub fn resolve(&self, path: &str) -> Option<NodeId> {
        self.with_tree(|t| t.resolve(path))
    }

    /// The content record for a node id (merged content map).
    pub fn content_of(&self, id: &str) -> Option<FileContent> {
        self.with_content(|m| m.get(id).cloned())
    }

    /// The derived absolute path of a node id.
    pub fn path_of(&self, id: &str) -> Option<String> {
        self.with_tree(|t| t.path(id))
    }

    /// Every distinct CID the drive references (durability roots), from the merged content map.
    pub fn all_cids(&self) -> Vec<String> {
        self.with_content(|m| m.all_cids())
    }

    /// The number of move ops in the merged union (across all writers).
    pub fn move_op_count(&self) -> usize {
        self.tree.op_count()
    }

    /// The number of content ops in the merged union (across all writers).
    pub fn content_op_count(&self) -> usize {
        self.content.op_count()
    }

    /// Per-writer applied versions for a sync-status display, for both collections.
    pub fn sync_status(&self) -> Vec<(String, ce_coord::Version, ce_coord::Version)> {
        let tree = self.tree.sync_status();
        let content: std::collections::HashMap<String, ce_coord::Version> =
            self.content.sync_status().into_iter().collect();
        tree.into_iter()
            .map(|(id, tv)| {
                let cv = content.get(&id).copied().unwrap_or(0);
                (id, tv, cv)
            })
            .collect()
    }

    // ---- snapshot / compaction ----

    /// Checkpoint this device's own writer logs (tree + content): serialize each to a
    /// content-addressed object and compact the local log past it. A fresh device that opens this
    /// drive then bootstraps from the checkpoint instead of replaying from version 1, then tails.
    /// Returns `(tree_checkpoint, content_checkpoint)`.
    pub async fn checkpoint(&self) -> Result<(Checkpoint, Checkpoint)> {
        let tree_cp = self.tree.writer_log().checkpoint().await?;
        let content_cp = self.content.writer_log().checkpoint().await?;
        Ok((tree_cp, content_cp))
    }
}

/// A globally-unique fresh node id derived from a timestamp: `<lamport-hex><replica-prefix><ctr>`.
/// Embedding the replica id makes ids unique across concurrent writers (no two devices mint the same
/// id even on the same Lamport tick), which the conflict-rename policy then surfaces if a name
/// collides.
fn fresh_node_id(ts: &Timestamp) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let rep: String = ts.replica.chars().take(8).collect();
    format!("{:012x}-{rep}-{c:08x}", ts.lamport)
}

/// Resolve a path that must be an existing directory (or ROOT) in the merged tree.
fn resolve_dir(t: &DriveTree, path: &str) -> Result<NodeId> {
    let node =
        t.resolve(path).ok_or_else(|| anyhow::anyhow!("no such directory: {path}"))?;
    if node != ROOT && !matches!(t.edge(&node).map(|e| &e.kind), Some(NodeKind::Dir)) {
        anyhow::bail!("'{path}' is not a directory");
    }
    Ok(node)
}

/// The live child of `parent` with the given bare name, if any.
fn child_named(t: &DriveTree, parent: &str, name: &str) -> Option<NodeId> {
    t.raw_children(parent)
        .into_iter()
        .find(|(n, id)| n == name && !t.is_trashed(id))
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Offline / in-process convergence: prove the two merge machines converge regardless of order,
    // mirroring exactly what `Merged` does (key-ordered union fold). The live 2-node mesh test lives
    // in tests/two_node_sync.rs.

    fn mv(l: u64, r: &str, child: &str, parent: &str, name: &str, kind: NodeKind) -> MoveOp {
        MoveOp { ts: Timestamp::new(l, r), child: child.into(), new_parent: parent.into(), new_name: name.into(), kind }
    }

    fn fold_tree(ops: &[MoveOp]) -> DriveTree {
        // Replicate Merged's fold: dedup by key into ascending order, fold on a fresh machine.
        let mut union: std::collections::BTreeMap<Timestamp, MoveOp> = std::collections::BTreeMap::new();
        for op in ops {
            union.insert(TreeMerge::key(op), op.clone());
        }
        let mut m = TreeMerge::default();
        for op in union.values() {
            m.apply(op.clone());
        }
        m.0
    }

    fn paths(t: &DriveTree) -> std::collections::BTreeMap<String, Option<String>> {
        t.edges().keys().map(|n| (n.clone(), t.path(n))).collect()
    }

    #[test]
    fn tree_merge_converges_regardless_of_order() {
        // Two writers concurrently build and mutate the same tree.
        let a = vec![
            mv(1, "a", "X", ROOT, "X", NodeKind::Dir),
            mv(3, "a", "fa", "X", "shared.txt", NodeKind::File),
            mv(5, "a", "Y", "X", "Y", NodeKind::Dir),
        ];
        let b = vec![
            mv(2, "b", "Z", ROOT, "Z", NodeKind::Dir),
            mv(4, "b", "fb", "Z", "shared.txt", NodeKind::File),
            mv(6, "b", "X", "Z", "X", NodeKind::Dir), // move X under Z
        ];
        let mut all = a.clone();
        all.extend(b.clone());
        let reference = paths(&fold_tree(&all));

        // Several delivery permutations all converge to the same tree.
        let perms: Vec<Vec<MoveOp>> = vec![
            a.iter().cloned().chain(b.iter().cloned()).collect(),
            b.iter().cloned().chain(a.iter().cloned()).collect(),
            vec![a[2].clone(), b[2].clone(), a[0].clone(), b[1].clone(), a[1].clone(), b[0].clone()],
            all.iter().rev().cloned().collect(),
        ];
        for (i, perm) in perms.iter().enumerate() {
            assert_eq!(paths(&fold_tree(perm)), reference, "tree perm {i} diverged");
        }
    }

    #[test]
    fn content_merge_converges_lww_regardless_of_order() {
        fn cop(l: u64, r: &str, id: &str, cid: &str, mtime: u64) -> StampedContentOp {
            StampedContentOp {
                ts: Timestamp::new(l, r),
                op: ContentOp::Set { id: id.into(), cid: cid.into(), size: 1, mode: 0, mtime_ms: mtime },
            }
        }
        let ops = [
            cop(1, "a", "f1", "cidA", 100),
            cop(2, "b", "f1", "cidB", 200), // later mtime wins
            cop(3, "a", "f2", "cidC", 50),
        ];
        let fold = |order: &[usize]| {
            let mut union: std::collections::BTreeMap<Timestamp, StampedContentOp> = std::collections::BTreeMap::new();
            for &i in order {
                union.insert(ContentMerge::key(&ops[i]), ops[i].clone());
            }
            let mut m = ContentMerge::default();
            for op in union.values() {
                m.apply(op.clone());
            }
            m.0
        };
        let r = fold(&[0, 1, 2]);
        assert_eq!(r.get("f1").unwrap().cid, "cidB", "later-mtime content wins LWW");
        assert_eq!(fold(&[2, 1, 0]).get("f1").unwrap().cid, "cidB");
        assert_eq!(fold(&[1, 0, 2]).get("f1").unwrap().cid, "cidB");
    }
}
