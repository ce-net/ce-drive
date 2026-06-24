//! The PURE, I/O-free CRDT state machine — the move-CRDT directory tree + content/version model +
//! path resolution + conflict info. This is a self-contained mirror of `ce-drive-core`'s `tree` and
//! `content` modules with **zero** dependency on ce-coord / ce-rs / tokio / rdev (none of which
//! target wasm). The serde wire format is **byte-identical** to core's, so a [`MoveOp`] /
//! [`StampedContentOp`] produced by the Rust core (and shipped over the mesh as JSON via ce-coord)
//! decodes and applies here unchanged — and an op produced *here* applies in the Rust core.
//!
//! File **bytes** never appear here. The browser stores/fetches blobs through `@ce-net/sdk` against
//! the in-browser CE node; this module manages only the tree structure + per-node content metadata
//! (CID / size / version), exactly the "manifest" half of the design's two-collection model.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

/// A stable, globally-unique node identifier (hex string). Path is derived from edges, never stored.
pub type NodeId = String;
/// A replica identifier (the writer's NodeId hex) — the tiebreak half of a [`Timestamp`].
pub type ReplicaId = String;
/// A Lamport clock value.
pub type Lamport = u64;

/// The reserved root node id. `path(ROOT) == "/"`.
pub const ROOT: &str = "ROOT";
/// The reserved trash node id. Delete = move under `TRASH`.
pub const TRASH: &str = "TRASH";

/// Total-order key: a Lamport clock tie-broken by replica id. Strict total order across replicas.
/// **Wire-identical** to `ce_drive_core::tree::Timestamp`.
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

/// Dir-vs-file marker. Wire-identical to core's `NodeKind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Dir,
    File,
}

/// The single structural-mutation op. Wire-identical to `ce_drive_core::tree::MoveOp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveOp {
    pub ts: Timestamp,
    pub child: NodeId,
    pub new_parent: NodeId,
    pub new_name: String,
    pub kind: NodeKind,
}

/// One edge in the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub parent: NodeId,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogEntry {
    op: MoveOp,
    old: Option<Edge>,
}

/// The move-CRDT directory tree (collection A). Holds the sorted move log, the live edge set, and a
/// derived children index. Identical algorithm to core's `DriveTree` (cycle-skip + undo/redo), so
/// folding the same op set in the same total order yields the identical acyclic tree.
#[derive(Debug, Clone, Default)]
pub struct DriveTree {
    log: Vec<LogEntry>,
    edges: HashMap<NodeId, Edge>,
    children: HashMap<NodeId, BTreeSet<(String, NodeId)>>,
}

impl DriveTree {
    pub fn new() -> Self {
        DriveTree::default()
    }

    fn do_move(&mut self, op: &MoveOp) -> Option<Edge> {
        if op.new_parent == op.child || self.is_ancestor(&op.child, &op.new_parent) {
            return self.edges.get(&op.child).cloned();
        }
        let old = self.edges.get(&op.child).cloned();
        if let Some(prev) = &old
            && let Some(set) = self.children.get_mut(&prev.parent)
        {
            set.remove(&(prev.name.clone(), op.child.clone()));
        }
        let edge = Edge {
            parent: op.new_parent.clone(),
            name: op.new_name.clone(),
            kind: op.kind.clone(),
        };
        self.children
            .entry(op.new_parent.clone())
            .or_default()
            .insert((op.new_name.clone(), op.child.clone()));
        self.edges.insert(op.child.clone(), edge);
        old
    }

    fn undo_move(&mut self, entry: &LogEntry) {
        let child = &entry.op.child;
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

    /// Integrate one op (sorted insert + undo/redo). The core convergence routine — identical to
    /// core's. Duplicate timestamps are idempotent.
    pub fn apply(&mut self, op: MoveOp) {
        let pos = self.log.partition_point(|e| e.op.ts < op.ts);
        if pos < self.log.len() && self.log[pos].op.ts == op.ts {
            return;
        }
        let tail: Vec<LogEntry> = self.log.split_off(pos);
        for entry in tail.iter().rev() {
            self.undo_move(entry);
        }
        let old = self.do_move(&op);
        self.log.push(LogEntry { op, old });
        for mut entry in tail {
            let old = self.do_move(&entry.op);
            entry.old = old;
            self.log.push(entry);
        }
    }

    pub fn is_ancestor(&self, ancestor: &str, node: &str) -> bool {
        let mut cur = node.to_string();
        for _ in 0..=self.edges.len() {
            if cur == ancestor {
                return true;
            }
            match self.edges.get(&cur) {
                Some(edge) => cur = edge.parent.clone(),
                None => return false,
            }
        }
        false
    }

    pub fn edge(&self, node: &str) -> Option<&Edge> {
        self.edges.get(node)
    }

    #[allow(dead_code)] // parity with core's DriveTree API; used by mount/web adapters.
    pub fn is_live(&self, node: &str) -> bool {
        match self.edges.get(node) {
            Some(_) => !self.is_ancestor(TRASH, node),
            None => node == ROOT,
        }
    }

    pub fn is_trashed(&self, node: &str) -> bool {
        self.is_ancestor(TRASH, node)
    }

    pub fn raw_children(&self, parent: &str) -> Vec<(String, NodeId)> {
        self.children.get(parent).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// `readdir` with the deterministic conflict-rename policy. Identical render-time function to
    /// core's: on a name collision the greater-timestamp node keeps the bare name, losers become
    /// `"<name>.conflict-<replica>-<lamport>"`. Returns `(rendered_name, child, kind)` sorted by name.
    pub fn readdir(&self, parent: &str) -> Vec<(String, NodeId, NodeKind)> {
        let mut by_name: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for (name, child) in self.raw_children(parent) {
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
            let mut ranked: Vec<(Timestamp, NodeId)> =
                ids.into_iter().map(|c| (self.defining_ts(&c), c)).collect();
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

    fn defining_ts(&self, node: &str) -> Timestamp {
        self.log
            .iter()
            .rev()
            .find(|e| e.op.child == node)
            .map(|e| e.op.ts.clone())
            .unwrap_or(Timestamp { lamport: 0, replica: String::new() })
    }

    /// The set of (bare_name, child) pairs in a dir that lose a name collision — the surfaced
    /// conflicts. Empty when every child name is unique. A pure function of the converged tree.
    pub fn conflicts(&self, parent: &str) -> Vec<Conflict> {
        let mut by_name: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for (name, child) in self.raw_children(parent) {
            if parent != TRASH && self.is_trashed(&child) {
                continue;
            }
            by_name.entry(name).or_default().push(child);
        }
        let mut out = Vec::new();
        for (name, ids) in by_name {
            if ids.len() < 2 {
                continue;
            }
            let mut ranked: Vec<(Timestamp, NodeId)> =
                ids.into_iter().map(|c| (self.defining_ts(&c), c)).collect();
            ranked.sort_by(|a, b| b.0.cmp(&a.0));
            let winner = ranked[0].1.clone();
            for (ts, loser) in ranked.into_iter().skip(1) {
                out.push(Conflict {
                    name: name.clone(),
                    winner: winner.clone(),
                    loser: loser.clone(),
                    rendered: format!("{name}.conflict-{}-{}", short(&ts.replica), ts.lamport),
                });
            }
        }
        out
    }

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
                None => return None,
            }
        }
        None
    }

    pub fn resolve(&self, path: &str) -> Option<NodeId> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Some(ROOT.to_string());
        }
        let mut cur = ROOT.to_string();
        for comp in trimmed.split('/') {
            let entry = self.readdir(&cur).into_iter().find(|(name, _, _)| name == comp)?;
            cur = entry.1;
        }
        Some(cur)
    }

    #[allow(dead_code)] // used by tests + mount/web adapters for snapshot/debug.
    pub fn edges(&self) -> &HashMap<NodeId, Edge> {
        &self.edges
    }

    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    #[allow(dead_code)] // parity with core; used to advance a Lamport clock past the frontier.
    pub fn log_max_ts(&self) -> Option<Timestamp> {
        self.log.last().map(|e| e.op.ts.clone())
    }

    /// The live child of `parent` with the given bare name, if any (not trashed).
    pub fn child_named(&self, parent: &str, name: &str) -> Option<NodeId> {
        self.raw_children(parent)
            .into_iter()
            .find(|(n, id)| n == name && !self.is_trashed(id))
            .map(|(_, id)| id)
    }
}

/// One surfaced name collision in a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// The contested bare name.
    pub name: String,
    /// The node that keeps the bare name (greater timestamp).
    pub winner: NodeId,
    /// The node that is conflict-renamed.
    pub loser: NodeId,
    /// The deterministic rendered name for the loser.
    pub rendered: String,
}

fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

// =================================================================================================
// Content map (collection B) — NodeId -> FileContent, LWW per key by mtime.
// =================================================================================================

/// Retained-version cap. Mirrors core's `MAX_VERSIONS`.
pub const MAX_VERSIONS: usize = 32;

/// One historical content pointer. Wire-identical to core's `content::Version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub set_at_ms: u64,
    pub cid: String,
    pub size: u64,
}

/// A file node's content metadata + version history. Wire-identical to core's `FileContent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub cid: String,
    pub size: u64,
    pub mode: u32,
    pub mtime_ms: u64,
    pub versions: Vec<Version>,
    pub doc_id: Option<String>,
}

impl FileContent {
    pub fn new(cid: impl Into<String>, size: u64, mode: u32, mtime_ms: u64) -> Self {
        FileContent { cid: cid.into(), size, mode, mtime_ms, versions: Vec::new(), doc_id: None }
    }

    fn record(&mut self, cid: impl Into<String>, size: u64, mtime_ms: u64) -> bool {
        let cid = cid.into();
        if cid == self.cid {
            return false;
        }
        let newer = mtime_ms > self.mtime_ms || (mtime_ms == self.mtime_ms && cid > self.cid);
        if !newer {
            self.push_version(Version { set_at_ms: mtime_ms, cid, size });
            return false;
        }
        self.push_version(Version { set_at_ms: self.mtime_ms, cid: self.cid.clone(), size: self.size });
        self.cid = cid;
        self.size = size;
        self.mtime_ms = mtime_ms;
        true
    }

    fn restore(&mut self, cid: &str, now_ms: u64) -> bool {
        if let Some(v) = self.versions.iter().find(|v| v.cid == cid).cloned() {
            self.push_version(Version { set_at_ms: self.mtime_ms, cid: self.cid.clone(), size: self.size });
            self.cid = v.cid;
            self.size = v.size;
            self.mtime_ms = now_ms;
            true
        } else {
            false
        }
    }

    fn push_version(&mut self, v: Version) {
        if self.versions.last().map(|l| l.cid == v.cid).unwrap_or(false) {
            return;
        }
        self.versions.push(v);
        if self.versions.len() > MAX_VERSIONS {
            let overflow = self.versions.len() - MAX_VERSIONS;
            self.versions.drain(0..overflow);
        }
    }

    pub fn all_cids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::iter::once(self.cid.clone())
            .chain(self.versions.iter().map(|x| x.cid.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Content mutation type. Wire-identical to core's `content::ContentOp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentOp {
    Set { id: NodeId, cid: String, size: u64, mode: u32, mtime_ms: u64 },
    Restore { id: NodeId, cid: String, now_ms: u64 },
    Remove { id: NodeId },
    SetDoc { id: NodeId, doc_id: String },
}

/// A content op tagged with its total-order key. Wire-identical to
/// `ce_drive_core::sync::StampedContentOp` — the merged-log content payload the browser receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampedContentOp {
    pub ts: Timestamp,
    pub op: ContentOp,
}

/// The content map (collection B): `NodeId -> FileContent`.
#[derive(Debug, Clone, Default)]
pub struct ContentMap {
    map: HashMap<NodeId, FileContent>,
}

impl ContentMap {
    pub fn new() -> Self {
        ContentMap::default()
    }

    pub fn get(&self, id: &str) -> Option<&FileContent> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn all_cids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.values().flat_map(|c| c.all_cids()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Apply one content op (LWW per key). Identical to core's `ContentMap::apply`.
    pub fn apply(&mut self, op: ContentOp) {
        match op {
            ContentOp::Set { id, cid, size, mode, mtime_ms } => match self.map.get_mut(&id) {
                Some(existing) => {
                    existing.mode = mode;
                    existing.record(cid, size, mtime_ms);
                }
                None => {
                    self.map.insert(id, FileContent::new(cid, size, mode, mtime_ms));
                }
            },
            ContentOp::Restore { id, cid, now_ms } => {
                if let Some(c) = self.map.get_mut(&id) {
                    c.restore(&cid, now_ms);
                }
            }
            ContentOp::Remove { id } => {
                self.map.remove(&id);
            }
            ContentOp::SetDoc { id, doc_id } => {
                if let Some(c) = self.map.get_mut(&id) {
                    c.doc_id = Some(doc_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn mv(l: u64, r: &str, child: &str, parent: &str, name: &str, kind: NodeKind) -> MoveOp {
        MoveOp { ts: Timestamp::new(l, r), child: child.into(), new_parent: parent.into(), new_name: name.into(), kind }
    }

    fn fold_tree(ops: &[MoveOp]) -> DriveTree {
        let mut union: BTreeMap<Timestamp, MoveOp> = BTreeMap::new();
        for op in ops {
            union.insert(op.ts.clone(), op.clone());
        }
        let mut t = DriveTree::new();
        for op in union.values() {
            t.apply(op.clone());
        }
        t
    }

    fn fold_order(ops: &[MoveOp], order: &[usize]) -> DriveTree {
        let mut t = DriveTree::new();
        for &i in order {
            t.apply(ops[i].clone());
        }
        t
    }

    fn paths(t: &DriveTree) -> BTreeMap<String, Option<String>> {
        t.edges().keys().map(|n| (n.clone(), t.path(n))).collect()
    }

    /// True if `start` is part of a cycle: walking parent pointers revisits `start`. A bounded walk
    /// (by edge count) catches any cycle the cycle-skip should have prevented.
    fn walk_cycles(t: &DriveTree, start: &str) -> bool {
        let mut cur = match t.edge(start) {
            Some(e) => e.parent.clone(),
            None => return false,
        };
        for _ in 0..=t.edges().len() {
            if cur == start {
                return true; // returned to start => cycle
            }
            match t.edge(&cur) {
                Some(e) => cur = e.parent.clone(),
                None => return false, // reached ROOT/TRASH/limbo without revisiting start
            }
        }
        false
    }

    // -- property generators (identical shape to core's tree_proptest, so the two crates' CRDTs are
    //    proven to converge under the same families of op streams) --
    fn op_set_strategy() -> impl Strategy<Value = Vec<MoveOp>> {
        let one = (any::<bool>(), 0usize..8, 0usize..10, 0usize..4, 0usize..3);
        proptest::collection::vec(one, 1..40).prop_map(|raw| {
            let replicas = ["aaaaaaaa", "bbbbbbbb", "cccccccc"];
            raw.into_iter()
                .enumerate()
                .map(|(i, (is_dir, child_i, parent_c, name_i, rep_i))| MoveOp {
                    ts: Timestamp::new(i as u64 + 1, replicas[rep_i]),
                    child: format!("n{child_i}"),
                    new_parent: match parent_c {
                        0 => ROOT.to_string(),
                        1 => TRASH.to_string(),
                        o => format!("n{}", o - 2),
                    },
                    new_name: format!("name{name_i}"),
                    kind: if is_dir { NodeKind::Dir } else { NodeKind::File },
                })
                .collect()
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

        /// The wasm CRDT converges identically across delivery orders (same guarantee as core).
        #[test]
        fn wasm_tree_converges_regardless_of_order(ops in op_set_strategy()) {
            let n = ops.len();
            let reference = paths(&fold_order(&ops, &(0..n).collect::<Vec<_>>()));
            let rev: Vec<usize> = (0..n).rev().collect();
            prop_assert_eq!(paths(&fold_order(&ops, &rev)), reference.clone(), "reverse delivery diverged");
            // A rotation too.
            let mut rot: Vec<usize> = (0..n).collect();
            rot.rotate_left(n / 2);
            prop_assert_eq!(paths(&fold_order(&ops, &rot)), reference, "rotated delivery diverged");
        }

        /// Idempotent + acyclic, same as core. The true invariant is ACYCLICITY: walking parent
        /// pointers from any node must terminate without revisiting that node. (A node may legitimately
        /// be parented under a never-created id — "limbo" — which the random generator can produce but
        /// the real Drive API cannot; such a node has no path to ROOT yet is still acyclic, so we test
        /// no-cycle directly rather than reachability-to-ROOT.)
        #[test]
        fn wasm_tree_idempotent_and_acyclic(ops in op_set_strategy()) {
            let once = fold_tree(&ops);
            let mut twice = once.clone();
            for op in &ops {
                twice.apply(op.clone());
            }
            prop_assert_eq!(paths(&twice), paths(&once), "duplicate delivery changed state");
            for node in once.edges().keys() {
                prop_assert!(!walk_cycles(&once, node), "node {} is part of a parent-pointer cycle", node);
            }
        }

        /// WIRE PARITY: a MoveOp serialized here decodes back here identically (byte-stable JSON), the
        /// property the design's golden-vector parity with core depends on.
        #[test]
        fn wasm_moveop_wire_stable(ops in op_set_strategy()) {
            for op in &ops {
                let bytes = serde_json::to_vec(op).unwrap();
                let back: MoveOp = serde_json::from_slice(&bytes).unwrap();
                prop_assert_eq!(&back, op);
                prop_assert_eq!(serde_json::to_vec(&back).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn pure_tree_converges_like_core() {
        let a = vec![
            mv(1, "a", "X", ROOT, "X", NodeKind::Dir),
            mv(5, "a", "X", "Z", "X", NodeKind::Dir),
        ];
        let b = vec![mv(2, "b", "Z", ROOT, "Z", NodeKind::Dir)];
        let mut all = a.clone();
        all.extend(b.clone());
        let r = paths(&fold_tree(&all));
        assert_eq!(paths(&fold_tree(&all.iter().rev().cloned().collect::<Vec<_>>())), r);
    }

    #[test]
    fn conflict_surfacing() {
        let mut t = DriveTree::new();
        t.apply(mv(1, "a", "d", ROOT, "d", NodeKind::Dir));
        t.apply(mv(5, "aaaaaaaa", "f1", "d", "note.md", NodeKind::File));
        t.apply(mv(9, "bbbbbbbb", "f2", "d", "note.md", NodeKind::File));
        let c = t.conflicts("d");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].winner, "f2");
        assert_eq!(c[0].loser, "f1");
        assert_eq!(c[0].rendered, "note.md.conflict-aaaaaaaa-5");
    }

    #[test]
    fn content_lww() {
        let mut m = ContentMap::new();
        m.apply(ContentOp::Set { id: "f".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 });
        m.apply(ContentOp::Set { id: "f".into(), cid: "b".into(), size: 2, mode: 0, mtime_ms: 2 });
        assert_eq!(m.get("f").unwrap().cid, "b");
        assert!(m.get("f").unwrap().versions.iter().any(|v| v.cid == "a"));
    }
}
