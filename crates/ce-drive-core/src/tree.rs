//! `DriveTree` — Kleppmann's highly-available move CRDT as a ce-coord [`StateMachine`].
//!
//! The directory tree is the *hard* half of a distributed filesystem: under concurrent edit,
//! naive path-keyed structures lose file identity across renames and (worse) concurrent **moves**
//! can create cycles. CE Drive uses the validated fix from the crdt-tree research: every node has a
//! **stable globally-unique [`NodeId`]**, and the tree is a set of `(child -> {parent, name, meta})`
//! edges. **Path is derived** by walking parent pointers — never stored as identity.
//!
//! ## One op type
//! Every structural mutation is a single [`MoveOp`]:
//! * **create** = move a fresh node out of limbo into a parent,
//! * **delete** = move into the reserved [`TRASH`] node,
//! * **rename** = move to the same parent with a new name,
//! * **move dir** = one O(1) edge flip (all descendants follow because path is derived).
//!
//! ## Cycle safety (the core invariant)
//! `apply(Move)` runs a **cycle check**: if `new_parent == child` or `child` is an ancestor of
//! `new_parent`, the move is **skipped** (deterministically, given op order). This keeps the tree
//! acyclic on every replica regardless of delivery order.
//!
//! ## Convergence: undo / do / redo
//! Out-of-order delivery means a late op can carry an earlier timestamp. Kleppmann's fix is to keep
//! the log sorted by `[Timestamp]`; on a late op we **undo** the ops after its insertion point (each
//! [`MoveOp`] records the `old_parent`/`old_name` it displaced = its exact inverse), **do** it, then
//! **redo** the rest (each re-runs its cycle check). Same op-set + same total order => identical
//! acyclic tree everywhere. In single-writer v1 the writer emits ops in timestamp order so the
//! undo/redo path is dormant (append-only) — but it is implemented and tested so promotion to
//! multi-writer v3 is a zero-call-site change.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ce_coord::replicated::StateMachine;
use serde::{Deserialize, Serialize};

/// A stable, globally-unique node identifier (16 hex bytes is plenty; collisions are negligible and
/// a collision only manifests as two writers reusing an id, which the conflict-rename surfaces).
pub type NodeId = String;

/// A replica identifier (the writer's NodeId hex) — the tiebreak half of a [`Timestamp`].
pub type ReplicaId = String;

/// A Lamport clock value.
pub type Lamport = u64;

/// The reserved root node id. Every drive has exactly one; `path(ROOT) == "/"`.
pub const ROOT: &str = "ROOT";

/// The reserved trash node id. Deleting a node moves it under `TRASH` (content kept until GC).
pub const TRASH: &str = "TRASH";

/// The reserved limbo node id. Hard-deleting a node (after `empty-trash`/GC) moves it under `LIMBO`,
/// a parent that is never listed, resolved, or path-derived — so the node leaves every live and trash
/// listing while its tombstone move op remains in the log (so peers converge on the removal). Keeping
/// it as an explicit detach (rather than dropping the edge) preserves the move-CRDT invariant that
/// every node's current location is defined by the latest move targeting it.
pub const LIMBO: &str = "LIMBO";

/// A total-order key for moves: a Lamport clock tie-broken by the replica id. Ordering is
/// `(lamport, replica)` lexicographically, which is a strict total order across all replicas.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp {
    pub lamport: Lamport,
    pub replica: ReplicaId,
}

impl Timestamp {
    pub fn new(lamport: Lamport, replica: impl Into<String>) -> Self {
        Timestamp { lamport, replica: replica.into() }
    }
}

/// Metadata carried on the edge to a node: whether it is a directory or a file, plus (for files)
/// the content pointer. The *bytes* of a file live in the content map ([`crate::content`]); this
/// `kind` only distinguishes dirs from files so `readdir` can render them and the cycle check can
/// reason about subtrees. (Files can never be a `new_parent`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// A directory (may have children).
    Dir,
    /// A regular file (a leaf; never a parent).
    File,
}

/// A single move operation — the only structural mutation type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveOp {
    /// Total-order key. Must be unique across the system (Lamport + replica guarantees this).
    pub ts: Timestamp,
    /// The node being moved/created.
    pub child: NodeId,
    /// The new parent: [`ROOT`], [`TRASH`], or a directory node id.
    pub new_parent: NodeId,
    /// The new name within `new_parent`.
    pub new_name: String,
    /// The kind of `child` (set on create; carried so readers know dir-vs-file without the map).
    pub kind: NodeKind,
}

/// One edge in the tree: where a node currently lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub parent: NodeId,
    pub name: String,
    pub kind: NodeKind,
}

/// A log entry: a move plus the edge it displaced (`None` if the child had no prior edge). The
/// `old` field is the exact inverse used by undo.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogEntry {
    op: MoveOp,
    /// The edge `child` had immediately before this op was applied (for undo). `None` = the child
    /// did not exist yet (a create).
    old: Option<Edge>,
}

/// The replicated directory tree. Holds the sorted move log, the current edge set, and a derived
/// children index for O(k log k) `readdir`.
#[derive(Debug, Clone, Default)]
pub struct DriveTree {
    /// Move log, kept sorted by `op.ts` (ascending). The ordered log *is* the CRDT.
    log: Vec<LogEntry>,
    /// Current edge per node: `child -> Edge`. ROOT/TRASH are implicit (never in this map).
    edges: HashMap<NodeId, Edge>,
    /// Derived index: `parent -> {(name, child)}` for fast, name-sorted readdir.
    children: HashMap<NodeId, BTreeSet<(String, NodeId)>>,
}

impl DriveTree {
    /// A fresh, empty tree (only the implicit ROOT and TRASH exist).
    pub fn new() -> Self {
        DriveTree::default()
    }

    /// Apply a move to the live edge set + children index (no log bookkeeping). Returns the edge it
    /// displaced (`None` if the child had no edge), or — if the move is cycle-forming — leaves the
    /// tree untouched and returns the *current* edge unchanged. The cycle skip is the core safety
    /// property: the result is always acyclic.
    fn do_move(&mut self, op: &MoveOp) -> Option<Edge> {
        // Cycle check: a node may not become its own ancestor, and may not be moved under itself.
        if op.new_parent == op.child || self.is_ancestor(&op.child, &op.new_parent) {
            // Skip: return the child's current edge so undo restores exactly this state.
            return self.edges.get(&op.child).cloned();
        }
        let old = self.edges.get(&op.child).cloned();
        // Remove the old children-index entry.
        if let Some(prev) = &old
            && let Some(set) = self.children.get_mut(&prev.parent)
        {
            set.remove(&(prev.name.clone(), op.child.clone()));
        }
        let edge = Edge { parent: op.new_parent.clone(), name: op.new_name.clone(), kind: op.kind.clone() };
        self.children
            .entry(op.new_parent.clone())
            .or_default()
            .insert((op.new_name.clone(), op.child.clone()));
        self.edges.insert(op.child.clone(), edge);
        old
    }

    /// Reverse one applied entry: restore `child` to the `old` edge it displaced (or remove it if
    /// it had no prior edge). Exact inverse of [`do_move`].
    fn undo_move(&mut self, entry: &LogEntry) {
        let child = &entry.op.child;
        // Remove the current edge from the index.
        if let Some(cur) = self.edges.get(child).cloned()
            && let Some(set) = self.children.get_mut(&cur.parent)
        {
            set.remove(&(cur.name.clone(), child.clone()));
        }
        match &entry.old {
            Some(prev) => {
                self.children
                    .entry(prev.parent.clone())
                    .or_default()
                    .insert((prev.name.clone(), child.clone()));
                self.edges.insert(child.clone(), prev.clone());
            }
            None => {
                self.edges.remove(child);
            }
        }
    }

    /// Insert `op` into the sorted log and apply with undo/redo so the final state equals applying
    /// the whole log in timestamp order. Duplicate timestamps are idempotent (ignored).
    fn integrate(&mut self, op: MoveOp) {
        // Find the insertion point by timestamp (the log is sorted ascending).
        let pos = self.log.partition_point(|e| e.op.ts < op.ts);
        // Idempotency: an identical timestamp already present is a duplicate — ignore it.
        if pos < self.log.len() && self.log[pos].op.ts == op.ts {
            return;
        }
        // Detach the suspended tail (so the undo/redo does not alias the log borrow).
        let tail: Vec<LogEntry> = self.log.split_off(pos);
        // Undo every detached entry (newest first).
        for entry in tail.iter().rev() {
            self.undo_move(entry);
        }
        // Do the new op, recording the edge it displaces.
        let old = self.do_move(&op);
        let new_entry = LogEntry { op, old };
        // Redo the suspended tail (oldest first), recomputing each displaced edge (cycle checks may
        // now resolve differently — that is exactly the point).
        self.log.push(new_entry);
        for mut entry in tail {
            let old = self.do_move(&entry.op);
            entry.old = old;
            self.log.push(entry);
        }
    }

    /// Is `ancestor` an ancestor of (or equal to) `node`? Walks parent pointers to ROOT. O(depth).
    /// Used by the cycle check (`child` must not be an ancestor of `new_parent`).
    pub fn is_ancestor(&self, ancestor: &str, node: &str) -> bool {
        let mut cur = node.to_string();
        // Bound the walk by the edge count to defend against any pathological state.
        for _ in 0..=self.edges.len() {
            if cur == ancestor {
                return true;
            }
            match self.edges.get(&cur) {
                Some(edge) => cur = edge.parent.clone(),
                None => return false, // reached ROOT/TRASH/limbo
            }
        }
        false
    }

    /// The current edge of a node, if it has one.
    pub fn edge(&self, node: &str) -> Option<&Edge> {
        self.edges.get(node)
    }

    /// True if the node is live (exists and is not under TRASH or LIMBO).
    pub fn is_live(&self, node: &str) -> bool {
        match self.edges.get(node) {
            Some(_) => !self.is_ancestor(TRASH, node) && !self.is_ancestor(LIMBO, node),
            None => node == ROOT, // ROOT is implicitly live; everything else needs an edge
        }
    }

    /// True if the node is in the trash subtree.
    pub fn is_trashed(&self, node: &str) -> bool {
        self.is_ancestor(TRASH, node)
    }

    /// True if the node has been hard-deleted (detached under [`LIMBO`]); it derives no path and is
    /// never listed or resolved.
    pub fn is_limbo(&self, node: &str) -> bool {
        self.is_ancestor(LIMBO, node)
    }

    /// The children of `parent`, as `(name, child)` pairs sorted by name. After applying the
    /// conflict-rename policy ([`readdir`]), names are filesystem-unique; this raw view may contain
    /// duplicate names (two distinct ids), which `readdir` surfaces.
    pub fn raw_children(&self, parent: &str) -> Vec<(String, NodeId)> {
        self.children.get(parent).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// `readdir` with the deterministic conflict-rename policy applied. When two live children of a
    /// directory share a name, the one whose creating/last move has the **greater** timestamp keeps
    /// the bare name; every other loser is rendered as `"<name>.conflict-<replica>-<lamport>"`.
    /// This is a pure render-time function of the converged tree (no extra ops), identical on every
    /// replica. Returns `(rendered_name, child, kind)`, sorted by rendered name.
    pub fn readdir(&self, parent: &str) -> Vec<(String, NodeId, NodeKind)> {
        // Group live children by bare name.
        let mut by_name: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for (name, child) in self.raw_children(parent) {
            // Hide trashed children from a live listing.
            if parent != TRASH && self.is_trashed(&child) {
                continue;
            }
            by_name.entry(name).or_default().push(child);
        }
        let mut out = Vec::new();
        for (name, mut ids) in by_name {
            if ids.len() == 1 {
                let child = ids.pop().unwrap();
                let kind = self.edges.get(&child).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
                out.push((name, child, kind));
                continue;
            }
            // Collision: the greatest-timestamp node keeps the name. We need each child's defining
            // timestamp = the timestamp of its current edge (the last move that placed it here).
            let mut ranked: Vec<(Timestamp, NodeId)> =
                ids.into_iter().map(|c| (self.defining_ts(&c), c)).collect();
            // Sort descending by timestamp: winner first.
            ranked.sort_by(|a, b| b.0.cmp(&a.0));
            for (i, (ts, child)) in ranked.into_iter().enumerate() {
                let kind = self.edges.get(&child).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
                if i == 0 {
                    out.push((name.clone(), child, kind));
                } else {
                    let rendered = format!("{name}.conflict-{}-{}", short(&ts.replica), ts.lamport);
                    out.push((rendered, child, kind));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The timestamp of the move that currently defines `node`'s edge (the last move whose `child`
    /// is `node`). Used for conflict-rename ranking. Falls back to a minimal timestamp if absent.
    fn defining_ts(&self, node: &str) -> Timestamp {
        self.log
            .iter()
            .rev()
            .find(|e| e.op.child == node)
            .map(|e| e.op.ts.clone())
            .unwrap_or(Timestamp { lamport: 0, replica: String::new() })
    }

    /// Derive the absolute path of `node` by walking parent pointers to ROOT and joining the bare
    /// (un-conflict-renamed) names with `/`. Returns `"/"` for ROOT. `None` if the node has no edge
    /// (limbo) or the walk does not reach ROOT (e.g. it is under TRASH — callers can check with
    /// [`is_trashed`]). Path is a pure function of the tree, never stored as identity.
    pub fn path(&self, node: &str) -> Option<String> {
        if node == ROOT {
            return Some("/".to_string());
        }
        let mut parts = Vec::new();
        let mut cur = node.to_string();
        for _ in 0..=self.edges.len() {
            match self.edges.get(&cur) {
                Some(edge) => {
                    parts.push(edge.name.clone());
                    if edge.parent == ROOT {
                        parts.reverse();
                        return Some(format!("/{}", parts.join("/")));
                    }
                    cur = edge.parent.clone();
                }
                None => return None, // hit limbo/TRASH without reaching ROOT
            }
        }
        None
    }

    /// Resolve an absolute path (e.g. `"/docs/spec.md"`) to a node id, using bare names. Returns the
    /// matching live node, or `None`. On a name collision the winner (bare name) is matched.
    pub fn resolve(&self, path: &str) -> Option<NodeId> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Some(ROOT.to_string());
        }
        let mut cur = ROOT.to_string();
        for comp in trimmed.split('/') {
            let entry = self
                .readdir(&cur)
                .into_iter()
                .find(|(name, _, _)| name == comp)?;
            cur = entry.1;
        }
        Some(cur)
    }

    /// The number of live (non-trashed) nodes with an edge. Useful for tests/metrics.
    pub fn live_count(&self) -> usize {
        self.edges.keys().filter(|n| self.is_live(n)).count()
    }

    /// The full set of edges (for snapshotting/debug). `child -> Edge`.
    pub fn edges(&self) -> &HashMap<NodeId, Edge> {
        &self.edges
    }

    /// The number of entries in the move log.
    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    /// The greatest [`Timestamp`] in the move log (the causal frontier), or `None` if empty. Used by
    /// the multi-writer layer to advance its Lamport clock past everything observed before minting a
    /// new local op (so its op strictly post-dates every op it has seen).
    pub fn log_max_ts(&self) -> Option<Timestamp> {
        // The log is kept sorted ascending by ts, so the last entry carries the maximum.
        self.log.last().map(|e| e.op.ts.clone())
    }
}

/// Short (8-hex) form of a replica id for conflict-rename rendering.
fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

impl StateMachine for DriveTree {
    type Op = MoveOp;

    fn apply(&mut self, op: Self::Op) {
        self.integrate(op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(lamport: u64, replica: &str, child: &str, parent: &str, name: &str, kind: NodeKind) -> MoveOp {
        MoveOp { ts: Timestamp::new(lamport, replica), child: child.into(), new_parent: parent.into(), new_name: name.into(), kind }
    }

    fn dir(lamport: u64, replica: &str, child: &str, parent: &str, name: &str) -> MoveOp {
        op(lamport, replica, child, parent, name, NodeKind::Dir)
    }
    fn file(lamport: u64, replica: &str, child: &str, parent: &str, name: &str) -> MoveOp {
        op(lamport, replica, child, parent, name, NodeKind::File)
    }

    #[test]
    fn create_and_path_derivation() {
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "docs", ROOT, "docs"));
        t.apply(file(2, "a", "f1", "docs", "spec.md"));
        assert_eq!(t.path("docs").as_deref(), Some("/docs"));
        assert_eq!(t.path("f1").as_deref(), Some("/docs/spec.md"));
        assert_eq!(t.path(ROOT).as_deref(), Some("/"));
        assert_eq!(t.resolve("/docs/spec.md").as_deref(), Some("f1"));
    }

    #[test]
    fn dir_move_is_one_op_and_descendants_follow() {
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "docs", ROOT, "docs"));
        t.apply(dir(2, "a", "sub", "docs", "sub"));
        t.apply(file(3, "a", "f", "sub", "x.txt"));
        assert_eq!(t.path("f").as_deref(), Some("/docs/sub/x.txt"));
        // Move the whole `docs` dir under a new `archive` dir with ONE op.
        t.apply(dir(4, "a", "archive", ROOT, "archive"));
        t.apply(dir(5, "a", "docs", "archive", "docs"));
        // The deep file's path followed automatically — no per-descendant op.
        assert_eq!(t.path("f").as_deref(), Some("/archive/docs/sub/x.txt"));
    }

    #[test]
    fn delete_moves_to_trash_and_hides_from_readdir() {
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "docs", ROOT, "docs"));
        t.apply(file(2, "a", "f", "docs", "x.txt"));
        assert_eq!(t.readdir("docs").len(), 1);
        // Delete = move to TRASH.
        t.apply(file(3, "a", "f", TRASH, "x.txt"));
        assert!(t.is_trashed("f"));
        assert!(t.readdir("docs").is_empty(), "trashed file hidden from live listing");
    }

    #[test]
    fn cycle_skip_keeps_tree_acyclic() {
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "A", ROOT, "A"));
        t.apply(dir(2, "a", "B", "A", "B"));
        // Try to move A under B (its own descendant) -> cycle -> skipped.
        t.apply(dir(3, "a", "A", "B", "A"));
        // A stays at root; tree remains acyclic and paths still derive.
        assert_eq!(t.path("A").as_deref(), Some("/A"));
        assert_eq!(t.path("B").as_deref(), Some("/A/B"));
    }

    #[test]
    fn concurrent_swap_resolves_acyclically_later_wins() {
        // Two replicas concurrently move A->under-B and B->under-A. The later-timestamp move wins;
        // the earlier is superseded so the tree never cycles.
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "A", ROOT, "A"));
        t.apply(dir(1, "b", "B", ROOT, "B"));
        // op X (ts (5,a)): move A under B. op Y (ts (6,b)): move B under A.
        let x = dir(5, "a", "A", "B", "A");
        let y = dir(6, "b", "B", "A", "B");
        // Apply in one order.
        let mut t1 = t.clone();
        t1.apply(x.clone());
        t1.apply(y.clone());
        // Apply in the reverse order (out-of-order delivery triggers undo/redo).
        let mut t2 = t.clone();
        t2.apply(y);
        t2.apply(x);
        // Both converge to the SAME acyclic tree.
        assert_eq!(paths(&t1), paths(&t2), "order-independent convergence");
        // And the tree is acyclic: every node still derives a path.
        for n in ["A", "B"] {
            assert!(t1.path(n).is_some() || t1.is_trashed(n));
        }
    }

    /// A deterministic dump of (node -> derived path) for equality comparison.
    fn paths(t: &DriveTree) -> BTreeMap<String, Option<String>> {
        t.edges().keys().map(|n| (n.clone(), t.path(n))).collect()
    }

    #[test]
    fn random_op_orders_converge_identically() {
        // Property-style: a fixed op set applied in many permutations converges to one tree.
        let ops = vec![
            dir(1, "a", "A", ROOT, "A"),
            dir(2, "b", "B", ROOT, "B"),
            dir(3, "a", "C", "A", "C"),
            file(4, "b", "f", "C", "f.txt"),
            dir(5, "a", "A", "B", "A"),   // move A under B
            file(6, "b", "f", "B", "f.txt"), // move f directly under B
            dir(7, "a", "B", "C", "B"),   // would cycle (B under C under A under B) on some orders
        ];
        let reference = apply_all(&ops, &[6, 5, 4, 3, 2, 1, 0]);
        let ref_paths = paths(&reference);
        // Several distinct permutations (each a valid out-of-order delivery).
        for perm in [
            vec![0, 1, 2, 3, 4, 5, 6],
            vec![6, 5, 4, 3, 2, 1, 0],
            vec![3, 0, 6, 1, 5, 2, 4],
            vec![4, 6, 0, 5, 1, 3, 2],
            vec![1, 0, 2, 4, 3, 6, 5],
        ] {
            let t = apply_all(&ops, &perm);
            assert_eq!(paths(&t), ref_paths, "perm {perm:?} diverged");
            // Acyclicity: is_ancestor is irreflexive-ish — no node is strictly under itself.
            for n in t.edges().keys() {
                let parent = &t.edges()[n].parent;
                assert!(!t.is_ancestor(n, parent) || parent == n.as_str() || t.path(n).is_some());
            }
        }
    }

    fn apply_all(ops: &[MoveOp], order: &[usize]) -> DriveTree {
        let mut t = DriveTree::new();
        for &i in order {
            t.apply(ops[i].clone());
        }
        t
    }

    #[test]
    fn name_collision_renders_deterministic_conflict() {
        // Two replicas create a file with the same name in the same dir (distinct ids).
        let mut t = DriveTree::new();
        t.apply(dir(1, "a", "d", ROOT, "d"));
        t.apply(file(5, "aaaaaaaa", "f1", "d", "note.md"));
        t.apply(file(9, "bbbbbbbb", "f2", "d", "note.md"));
        let entries = t.readdir("d");
        assert_eq!(entries.len(), 2);
        // The greater-timestamp (lamport 9) node keeps the bare name; the other is conflict-renamed.
        let winner = entries.iter().find(|(n, _, _)| n == "note.md").unwrap();
        assert_eq!(winner.1, "f2");
        let loser = entries.iter().find(|(n, _, _)| n != "note.md").unwrap();
        assert_eq!(loser.0, "note.md.conflict-aaaaaaaa-5");
        assert_eq!(loser.1, "f1");
    }

    #[test]
    fn conflict_rename_is_replica_order_independent() {
        let mk = |order: &[(u64, &str, &str)]| {
            let mut t = DriveTree::new();
            t.apply(dir(1, "a", "d", ROOT, "d"));
            for (l, r, id) in order {
                t.apply(file(*l, r, id, "d", "x"));
            }
            t.readdir("d")
        };
        let a = mk(&[(5, "aaaaaaaa", "f1"), (9, "bbbbbbbb", "f2")]);
        let b = mk(&[(9, "bbbbbbbb", "f2"), (5, "aaaaaaaa", "f1")]);
        assert_eq!(a, b, "conflict rendering is independent of insertion order");
    }

    #[test]
    fn duplicate_op_is_idempotent() {
        let mut t = DriveTree::new();
        let o = dir(1, "a", "A", ROOT, "A");
        t.apply(o.clone());
        t.apply(o.clone());
        t.apply(o);
        assert_eq!(t.log_len(), 1, "identical timestamp applied once");
        assert_eq!(t.readdir(ROOT).len(), 1);
    }
}
