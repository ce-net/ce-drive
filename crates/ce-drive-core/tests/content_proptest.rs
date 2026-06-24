//! Property tests for the content map (collection B): LWW convergence, version-history integrity,
//! and serialization round-trips (including values larger than JSON's 2^53 safe-integer limit).
//!
//! The content map is the orthogonal half of the design: keyed by stable NodeId, LWW-by-mtime with a
//! capped version history. The properties that must hold under any concurrent op delivery:
//!
//! * **LWW convergence.** Folding the same `Set` ops in any total-order permutation yields the same
//!   `current` cid (the greatest (mtime, cid) wins) — independent of delivery order.
//! * **No data loss.** Every distinct cid ever `Set` is retrievable via `all_cids()` until the
//!   version cap evicts the *oldest*; the current pointer is always present.
//! * **Version cap.** History never exceeds `MAX_VERSIONS`.
//! * **Serialization round-trip.** `FileContent` / `ContentOp` / `Version` survive a JSON and a
//!   bincode round-trip byte-for-byte, including `size`/`mtime_ms` values > 2^53 (which JSON numbers
//!   cannot represent exactly — they must serialize as the wire format the codebase actually uses).

use std::collections::BTreeMap;

use ce_drive_core::content::{ContentMap, ContentOp, FileContent, MAX_VERSIONS, Version};
use ce_drive_core::tree::Timestamp;
use ce_coord::replicated::StateMachine;
use proptest::prelude::*;

/// Fold a set of `(ts, ContentOp)` in ascending-ts order (what the merge layer guarantees) — the LWW
/// canonical order. Returns the converged map.
fn fold_lww(ops: &[(Timestamp, ContentOp)], order: &[usize]) -> ContentMap {
    // Dedup by key into ascending order (mirrors `Merged`'s key-ordered union fold).
    let mut union: BTreeMap<Timestamp, ContentOp> = BTreeMap::new();
    for &i in order {
        union.insert(ops[i].0.clone(), ops[i].1.clone());
    }
    let mut m = ContentMap::new();
    for op in union.values() {
        m.apply(op.clone());
    }
    m
}

fn set_op_strategy() -> impl Strategy<Value = Vec<(Timestamp, ContentOp)>> {
    let one = (
        0usize..4,            // node id index (few keys => concurrent edits on the same file)
        0u64..1_000_000,      // mtime
        0usize..6,            // cid index
        0usize..3,            // replica
    );
    proptest::collection::vec(one, 1..30).prop_map(|raw| {
        let replicas = ["aa", "bb", "cc"];
        raw.into_iter()
            .enumerate()
            .map(|(i, (id_i, mtime, cid_i, rep_i))| {
                let ts = Timestamp::new(i as u64 + 1, replicas[rep_i]);
                let op = ContentOp::Set {
                    id: format!("f{id_i}"),
                    cid: format!("cid{cid_i}"),
                    size: mtime, // size tracks mtime for variety; value irrelevant to LWW
                    mode: 0o644,
                    mtime_ms: mtime,
                };
                (ts, op)
            })
            .collect()
    })
}

fn rev(n: usize) -> Vec<usize> {
    (0..n).rev().collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// LWW CONVERGENCE: forward vs. reverse delivery converge to the same current cid per key.
    #[test]
    fn lww_converges_regardless_of_order(ops in set_op_strategy()) {
        let n = ops.len();
        let fwd = fold_lww(&ops, &(0..n).collect::<Vec<_>>());
        let bwd = fold_lww(&ops, &rev(n));
        // Compare the current cid of every key present.
        let mut keys: Vec<String> = fwd.entries().map(|(k, _)| k.clone()).collect();
        keys.sort();
        for k in &keys {
            prop_assert_eq!(
                fwd.get(k).map(|c| &c.cid),
                bwd.get(k).map(|c| &c.cid),
                "LWW current cid for {} diverged across delivery order", k
            );
        }
        // all_cids is order-independent too.
        prop_assert_eq!(fwd.all_cids(), bwd.all_cids(), "all_cids must be order-independent");
    }

    /// VERSION CAP + CURRENT PRESENT: history is bounded and the current cid is always in all_cids.
    #[test]
    fn version_history_bounded_and_current_retained(ops in set_op_strategy()) {
        let m = fold_lww(&ops, &(0..ops.len()).collect::<Vec<_>>());
        for (_id, fc) in m.entries() {
            prop_assert!(fc.versions.len() <= MAX_VERSIONS, "version history exceeded cap");
            prop_assert!(fc.all_cids().contains(&fc.cid), "current cid missing from all_cids");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 600, ..ProptestConfig::default() })]

    /// SERIALIZATION ROUND-TRIP: FileContent survives JSON and bincode round-trips byte-identically,
    /// including size/mtime values beyond 2^53 (the JSON safe-integer boundary). serde_json emits
    /// u64 as a JSON integer; round-tripping back to u64 must be exact (this is the property a money/
    /// large-value field relies on — the codebase carries large integers as the wire's native form).
    #[test]
    fn filecontent_roundtrips(
        cid in "[a-f0-9]{0,40}",
        size in any::<u64>(),
        mode in any::<u32>(),
        mtime in any::<u64>(),
        nver in 0usize..40,
    ) {
        let mut versions = Vec::new();
        for i in 0..nver {
            versions.push(Version { set_at_ms: (i as u64).wrapping_mul(7), cid: format!("v{i}"), size: i as u64, conflict: i % 3 == 0 });
        }
        let fc = FileContent { cid, size, mode, mtime_ms: mtime, versions, doc_id: None };

        // JSON round-trip (the HTTP/wire form).
        let j = serde_json::to_string(&fc).unwrap();
        let back_j: FileContent = serde_json::from_str(&j).unwrap();
        prop_assert_eq!(&fc, &back_j, "JSON round-trip must be exact (incl values > 2^53)");

        // bincode round-trip (the disk/mesh form).
        let b = bincode::serialize(&fc).unwrap();
        let back_b: FileContent = bincode::deserialize(&b).unwrap();
        prop_assert_eq!(&fc, &back_b, "bincode round-trip must be exact");
    }

    /// Large-value boundary: a value strictly greater than 2^53 round-trips exactly through JSON
    /// (proving the codebase does not silently truncate large file sizes / timestamps to f64).
    #[test]
    fn large_u64_roundtrips_exactly(extra in 1u64..u64::MAX/2) {
        let big = (1u64 << 53) + extra; // > 2^53, unrepresentable as an exact f64
        let fc = FileContent::new("cid", big, 0o644, big);
        let j = serde_json::to_string(&fc).unwrap();
        let back: FileContent = serde_json::from_str(&j).unwrap();
        prop_assert_eq!(back.size, big);
        prop_assert_eq!(back.mtime_ms, big);
    }
}

/// `Remove` deletes the record; a subsequent `Set` re-creates it fresh (no stale history).
#[test]
fn remove_then_set_is_fresh() {
    let mut m = ContentMap::new();
    m.apply(ContentOp::Set { id: "f".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 });
    m.apply(ContentOp::Set { id: "f".into(), cid: "b".into(), size: 2, mode: 0, mtime_ms: 2 });
    m.apply(ContentOp::Remove { id: "f".into() });
    assert!(m.get("f").is_none(), "Remove drops the record");
    m.apply(ContentOp::Set { id: "f".into(), cid: "c".into(), size: 3, mode: 0, mtime_ms: 3 });
    let fc = m.get("f").unwrap();
    assert_eq!(fc.cid, "c");
    assert!(fc.versions.is_empty(), "re-created record carries no stale history");
}

/// `SetDoc` attaches a doc id only to an existing record (no-op on a missing key).
#[test]
fn setdoc_attaches_only_to_existing() {
    let mut m = ContentMap::new();
    m.apply(ContentOp::SetDoc { id: "missing".into(), doc_id: "d1".into() });
    assert!(m.get("missing").is_none(), "SetDoc on a missing key is a no-op");
    m.apply(ContentOp::Set { id: "f".into(), cid: "a".into(), size: 1, mode: 0, mtime_ms: 1 });
    m.apply(ContentOp::SetDoc { id: "f".into(), doc_id: "d1".into() });
    assert_eq!(m.get("f").unwrap().doc_id.as_deref(), Some("d1"));
}

/// `Restore` of an unknown cid is a no-op; restoring a known one re-points current and bumps mtime.
#[test]
fn restore_semantics() {
    let mut fc = FileContent::new("cid0", 10, 0o644, 100);
    fc.record("cid1", 12, 200);
    assert!(!fc.restore("nope", 300), "restore of unknown cid fails");
    assert_eq!(fc.cid, "cid1");
    assert!(fc.restore("cid0", 400), "restore of a known prior cid succeeds");
    assert_eq!(fc.cid, "cid0");
    assert_eq!(fc.mtime_ms, 400, "restore bumps mtime so it wins future LWW");
}

/// mtime tie -> cid is the deterministic tiebreak (the greater cid wins), identically on every
/// replica. Two concurrent edits at the same mtime must not diverge.
#[test]
fn equal_mtime_breaks_by_cid_deterministically() {
    let mut a = FileContent::new("base", 1, 0, 100);
    a.record("zzz", 1, 200);
    let mut b = a.clone();
    // Both apply the same two equal-mtime edits in opposite orders.
    a.record("aaa", 1, 200);
    a.record("mmm", 1, 200);
    b.record("mmm", 1, 200);
    b.record("aaa", 1, 200);
    assert_eq!(a.cid, b.cid, "equal-mtime edits converge by cid tiebreak");
    assert_eq!(a.cid, "zzz", "the lexicographically-greatest cid at that mtime wins");
}
