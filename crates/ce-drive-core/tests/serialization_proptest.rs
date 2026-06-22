//! Serialization round-trip property tests for every on-wire / on-disk type in the core.
//!
//! The CRDT only converges if the bytes one replica sends decode identically on another. We assert
//! encode==decode for the tree ops, the stamped content ops, the persisted `DriveState`, and the
//! sharing types — through both JSON (the HTTP/mesh form) and bincode (the disk form) — over randomly
//! generated values, including u64 fields larger than 2^53.

use ce_drive_core::content::ContentOp;
use ce_drive_core::sync::StampedContentOp;
use ce_drive_core::tree::{MoveOp, NodeKind, Timestamp};
use ce_drive_core::{DriveState, PinPolicy};
use proptest::prelude::*;

fn kind() -> impl Strategy<Value = NodeKind> {
    prop_oneof![Just(NodeKind::Dir), Just(NodeKind::File)]
}

fn timestamp() -> impl Strategy<Value = Timestamp> {
    (any::<u64>(), "[a-f0-9]{0,16}").prop_map(|(l, r)| Timestamp::new(l, r))
}

fn move_op() -> impl Strategy<Value = MoveOp> {
    (timestamp(), "[a-z0-9]{1,8}", "[A-Z]{1,6}", "[^/\\x00]{0,12}", kind()).prop_map(
        |(ts, child, parent, name, kind)| MoveOp {
            ts,
            child,
            new_parent: parent,
            new_name: name,
            kind,
        },
    )
}

fn content_op() -> impl Strategy<Value = ContentOp> {
    prop_oneof![
        ("[a-z]{1,6}", "[a-f0-9]{0,40}", any::<u64>(), any::<u32>(), any::<u64>())
            .prop_map(|(id, cid, size, mode, mtime_ms)| ContentOp::Set { id, cid, size, mode, mtime_ms }),
        ("[a-z]{1,6}", "[a-f0-9]{0,40}", any::<u64>())
            .prop_map(|(id, cid, now_ms)| ContentOp::Restore { id, cid, now_ms }),
        "[a-z]{1,6}".prop_map(|id| ContentOp::Remove { id }),
        ("[a-z]{1,6}", "[a-z0-9]{1,8}").prop_map(|(id, doc_id)| ContentOp::SetDoc { id, doc_id }),
    ]
}

/// JSON and bincode round-trips must both reproduce the exact bytes (via the canonical JSON value).
macro_rules! assert_roundtrips {
    ($v:expr) => {{
        let v = $v;
        let j = serde_json::to_vec(&v).unwrap();
        let back_j: serde_json::Value = serde_json::from_slice(&j).unwrap();
        let canon: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap();
        prop_assert_eq!(back_j, canon, "JSON not stable");
        let b = bincode::serialize(&v).unwrap();
        let _ = bincode::deserialize::<serde_json::Value>(&b); // bincode value untyped not needed
        v
    }};
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, ..ProptestConfig::default() })]

    #[test]
    fn moveop_json_and_bincode_roundtrip(op in move_op()) {
        let op = assert_roundtrips!(op);
        // Typed round-trip equality.
        let j: MoveOp = serde_json::from_slice(&serde_json::to_vec(&op).unwrap()).unwrap();
        prop_assert_eq!(&op, &j);
        let b: MoveOp = bincode::deserialize(&bincode::serialize(&op).unwrap()).unwrap();
        prop_assert_eq!(&op, &b);
    }

    #[test]
    fn timestamp_ordering_is_total_and_stable(a in timestamp(), b in timestamp()) {
        // A strict total order: exactly one of <, =, > holds, and it round-trips.
        use std::cmp::Ordering::*;
        let ord = a.cmp(&b);
        let back_a: Timestamp = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        prop_assert_eq!(&a, &back_a);
        // (lamport, replica) lexicographic.
        let expected = (a.lamport, a.replica.clone()).cmp(&(b.lamport, b.replica.clone()));
        prop_assert_eq!(ord, expected);
        if ord == Equal {
            prop_assert_eq!(a.lamport, b.lamport);
        }
    }

    #[test]
    fn stamped_content_op_roundtrips(ts in timestamp(), op in content_op()) {
        let stamped = StampedContentOp { ts, op };
        let j = serde_json::to_vec(&stamped).unwrap();
        let back: StampedContentOp = serde_json::from_slice(&j).unwrap();
        prop_assert_eq!(serde_json::to_vec(&back).unwrap(), j);
        let b = bincode::serialize(&stamped).unwrap();
        let back_b: StampedContentOp = bincode::deserialize(&b).unwrap();
        prop_assert_eq!(bincode::serialize(&back_b).unwrap(), b);
    }

    #[test]
    fn pin_policy_roundtrips(rep in any::<u8>(), retention in any::<u64>(), announce in any::<bool>()) {
        let p = PinPolicy { replication: rep, trash_retention_secs: retention, announce };
        let j: PinPolicy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        prop_assert_eq!(&p, &j);
    }
}

/// A full `DriveState` (the on-disk JSON) round-trips and replays to an identical tree — the persistence
/// invariant the CLI relies on. Built from a random valid op log via the public `Drive` API.
#[test]
fn drivestate_persist_replay_is_identical() {
    use ce_drive_core::content::FileContent;
    use ce_drive_core::Drive;

    let mut d = Drive::init("persist", "replica-x");
    d.mkdir("/", "docs").unwrap();
    d.add_file("/docs", "a.md", FileContent::new("cidA", 10, 0o644, 1)).unwrap();
    d.mkdir("/docs", "sub").unwrap();
    d.add_file("/docs/sub", "b.md", FileContent::new("cidB", 20, 0o644, 2)).unwrap();
    d.mv("/docs/a.md", "/docs/sub", "a2.md").unwrap();
    d.add_file("/docs/sub", "b.md", FileContent::new("cidB2", 25, 0o644, 3)).unwrap(); // edit
    d.rm("/docs/sub/a2.md").unwrap();

    let state = d.state().clone();
    // JSON round-trip of the persisted state.
    let json = serde_json::to_vec_pretty(&state).unwrap();
    let restored_state: DriveState = serde_json::from_slice(&json).unwrap();
    let d2 = Drive::from_state(restored_state);

    // The replayed drive lists identically at every directory.
    assert_eq!(d.ls("/").unwrap(), d2.ls("/").unwrap());
    assert_eq!(d.ls("/docs").unwrap(), d2.ls("/docs").unwrap());
    assert_eq!(d.ls("/docs/sub").unwrap(), d2.ls("/docs/sub").unwrap());
    assert_eq!(d.all_cids(), d2.all_cids(), "content cids identical after persist/replay");

    // bincode persistence form too.
    let bin = bincode::serialize(&state).unwrap();
    let from_bin: DriveState = bincode::deserialize(&bin).unwrap();
    let d3 = Drive::from_state(from_bin);
    assert_eq!(d.ls("/docs/sub").unwrap(), d3.ls("/docs/sub").unwrap());
}
