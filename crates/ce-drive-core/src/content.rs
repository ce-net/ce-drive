//! The content map — `RMap<NodeId, FileContent>` keyed by **stable NodeId, never path**.
//!
//! This is collection (B) of the two-collection design (§4.4 of the design). It is orthogonal to the
//! [`DriveTree`](crate::tree) structure log (A): renaming/moving a file is a `MoveOp` in (A) and
//! never touches (B); editing a file's bytes is an `Insert(id, new_content)` in (B) and never
//! touches (A). Because both are keyed by the same stable id, the two intents never collide — the
//! decisive orthogonality of the design.
//!
//! Versioning is free from immutability: every save is a new file CID and the old CID stays valid,
//! so [`FileContent`] keeps a capped, ordered `versions` list of prior `(version_ts, cid, size)`.
//! "Restore version" = point `current` back at an old entry (a new content op).
//!
//! In single-writer v1 this is a thin newtype over ce-coord's [`RMap`]; we model the op type here so
//! the content map is its own [`StateMachine`] (the engine ships our `Op` opaque). LWW per key:
//! later `set_at` wins.

use ce_coord::StateMachine;
use serde::{Deserialize, Serialize};

use crate::tree::NodeId;

/// How many prior versions to retain per file before pruning the oldest. Pinning policy
/// ([`crate::durability`]) keeps the last K versions retrievable.
pub const MAX_VERSIONS: usize = 32;

/// One historical content pointer for a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Unix milliseconds the version was recorded (LWW tiebreak ordering key).
    pub set_at_ms: u64,
    /// The object CID of the file at this version (== `cid(file_bytes)`).
    pub cid: String,
    /// Size in bytes.
    pub size: u64,
}

/// The content of one file node: the current pointer plus the version history. Keyed by NodeId in
/// the map. A `.cedoc` file additionally carries an embedded ce-notes `doc_id` (gated to v3; the
/// field is reserved here so the wire format is stable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    /// The current object CID (== `cid(file_bytes)`). The whole-file content id.
    pub cid: String,
    /// Current size in bytes.
    pub size: u64,
    /// Unix mode bits (0 where unknown / on Windows).
    pub mode: u32,
    /// Last-modified time, unix milliseconds — the LWW ordering key for concurrent edits.
    pub mtime_ms: u64,
    /// Prior versions, oldest-first, capped at [`MAX_VERSIONS`].
    pub versions: Vec<Version>,
    /// Optional embedded ce-notes document id for `.cedoc` nodes (v3). `None` for ordinary files.
    pub doc_id: Option<String>,
}

impl FileContent {
    /// A fresh content record for a newly-stored file.
    pub fn new(cid: impl Into<String>, size: u64, mode: u32, mtime_ms: u64) -> Self {
        FileContent { cid: cid.into(), size, mode, mtime_ms, versions: Vec::new(), doc_id: None }
    }

    /// Record a new content version, pushing the *current* pointer onto the history first (so the
    /// old CID remains retrievable) and applying LWW: if `mtime_ms` is not newer than the current,
    /// and the cid differs, this is a concurrent edit — the caller surfaces a conflict copy; here we
    /// simply keep the later `mtime_ms` (cid as the final tiebreak). Returns `true` if `self` was
    /// updated to the new content (i.e. the new edit won LWW).
    pub fn record(&mut self, cid: impl Into<String>, size: u64, mtime_ms: u64) -> bool {
        let cid = cid.into();
        if cid == self.cid {
            return false; // no-op: identical content
        }
        let newer = mtime_ms > self.mtime_ms || (mtime_ms == self.mtime_ms && cid > self.cid);
        if !newer {
            // The incoming edit is older/loses LWW. Still record it in history so nothing is lost.
            self.push_version(Version { set_at_ms: mtime_ms, cid, size });
            return false;
        }
        // Push the current pointer to history, then advance to the new content.
        self.push_version(Version { set_at_ms: self.mtime_ms, cid: self.cid.clone(), size: self.size });
        self.cid = cid;
        self.size = size;
        self.mtime_ms = mtime_ms;
        true
    }

    /// Point `current` back at a prior version (undo / restore). Returns `false` if no such version.
    pub fn restore(&mut self, cid: &str, now_ms: u64) -> bool {
        if let Some(v) = self.versions.iter().find(|v| v.cid == cid).cloned() {
            // Archive the (current) pointer, then make the restored one current with a fresh mtime.
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
        // Avoid duplicate consecutive entries.
        if self.versions.last().map(|l| l.cid == v.cid).unwrap_or(false) {
            return;
        }
        self.versions.push(v);
        if self.versions.len() > MAX_VERSIONS {
            let overflow = self.versions.len() - MAX_VERSIONS;
            self.versions.drain(0..overflow);
        }
    }

    /// All distinct CIDs this file references (current + history) — the durability/GC roots.
    pub fn all_cids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::iter::once(self.cid.clone())
            .chain(self.versions.iter().map(|x| x.cid.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// The mutation type for the content map. Modeled as its own [`StateMachine`] so the content map is
/// a first-class replicated collection (the engine ships these ops opaque, in version order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentOp {
    /// Set/replace a file's content (a save). Carries the full new [`FileContent`]-defining fields;
    /// `apply` folds it into the existing record via [`FileContent::record`] (LWW + history).
    Set { id: NodeId, cid: String, size: u64, mode: u32, mtime_ms: u64 },
    /// Restore a file to a prior CID (a new op, so it converges like any other).
    Restore { id: NodeId, cid: String, now_ms: u64 },
    /// Remove a file's content record entirely (after GC / hard-delete).
    Remove { id: NodeId },
    /// Attach an embedded ce-notes document id to a node (`.cedoc`). Reserved for v3.
    SetDoc { id: NodeId, doc_id: String },
}

/// The replicated content map: `NodeId -> FileContent`.
#[derive(Debug, Clone, Default)]
pub struct ContentMap {
    map: std::collections::HashMap<NodeId, FileContent>,
}

impl ContentMap {
    pub fn new() -> Self {
        ContentMap::default()
    }

    /// The content record for a node, if present.
    pub fn get(&self, id: &str) -> Option<&FileContent> {
        self.map.get(id)
    }

    /// Number of content records.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every (id, content) pair (for snapshot / durability roots).
    pub fn entries(&self) -> impl Iterator<Item = (&NodeId, &FileContent)> {
        self.map.iter()
    }

    /// Every distinct CID referenced by every file (durability/GC roots across the whole drive).
    pub fn all_cids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.values().flat_map(|c| c.all_cids()).collect();
        v.sort();
        v.dedup();
        v
    }
}

impl StateMachine for ContentMap {
    type Op = ContentOp;

    fn apply(&mut self, op: Self::Op) {
        match op {
            ContentOp::Set { id, cid, size, mode, mtime_ms } => {
                match self.map.get_mut(&id) {
                    Some(existing) => {
                        existing.mode = mode;
                        existing.record(cid, size, mtime_ms);
                    }
                    None => {
                        self.map.insert(id, FileContent::new(cid, size, mode, mtime_ms));
                    }
                }
            }
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

    #[test]
    fn record_advances_and_keeps_history() {
        let mut c = FileContent::new("cid0", 10, 0o644, 100);
        assert!(c.record("cid1", 12, 200));
        assert_eq!(c.cid, "cid1");
        assert_eq!(c.versions.len(), 1);
        assert_eq!(c.versions[0].cid, "cid0");
    }

    #[test]
    fn record_lww_older_edit_loses_but_is_retained() {
        let mut c = FileContent::new("cid1", 12, 0o644, 200);
        // An older (mtime 100) concurrent edit arrives: it loses LWW but is kept in history.
        assert!(!c.record("cidX", 5, 100));
        assert_eq!(c.cid, "cid1", "current pointer unchanged (newer mtime wins)");
        assert!(c.versions.iter().any(|v| v.cid == "cidX"), "loser retained, no data lost");
    }

    #[test]
    fn record_identical_cid_is_noop() {
        let mut c = FileContent::new("cid1", 12, 0o644, 200);
        assert!(!c.record("cid1", 12, 999));
        assert!(c.versions.is_empty());
    }

    #[test]
    fn restore_points_back_at_old_cid() {
        let mut c = FileContent::new("cid0", 10, 0o644, 100);
        c.record("cid1", 12, 200);
        c.record("cid2", 14, 300);
        assert!(c.restore("cid0", 400));
        assert_eq!(c.cid, "cid0");
        assert_eq!(c.mtime_ms, 400);
        assert!(!c.restore("missing", 500));
    }

    #[test]
    fn version_cap_prunes_oldest() {
        let mut c = FileContent::new("cid-init", 1, 0, 0);
        for i in 1..=(MAX_VERSIONS as u64 + 5) {
            c.record(format!("cid{i}"), i, i);
        }
        assert!(c.versions.len() <= MAX_VERSIONS);
    }

    #[test]
    fn map_apply_set_and_all_cids() {
        let mut m = ContentMap::new();
        m.apply(ContentOp::Set { id: "f1".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 });
        m.apply(ContentOp::Set { id: "f1".into(), cid: "b".into(), size: 2, mode: 0, mtime_ms: 2 });
        m.apply(ContentOp::Set { id: "f2".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 });
        assert_eq!(m.get("f1").unwrap().cid, "b");
        // Dedup across files: a, b only.
        assert_eq!(m.all_cids(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn map_apply_is_order_independent_for_distinct_keys() {
        let ops = || {
            vec![
                ContentOp::Set { id: "f1".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 },
                ContentOp::Set { id: "f2".into(), cid: "c".into(), size: 1, mode: 0, mtime_ms: 1 },
            ]
        };
        let mut m1 = ContentMap::new();
        for o in ops() {
            m1.apply(o);
        }
        let mut m2 = ContentMap::new();
        for o in ops().into_iter().rev() {
            m2.apply(o);
        }
        assert_eq!(m1.get("f1").unwrap().cid, m2.get("f1").unwrap().cid);
        assert_eq!(m1.get("f2").unwrap().cid, m2.get("f2").unwrap().cid);
    }
}
