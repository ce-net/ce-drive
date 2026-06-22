//! Golden wire-format vectors for the cross-crate (core <-> wasm) CRDT op encoding.
//!
//! The browser CRDT ([`ce-drive-wasm`]) is a separate, wasm-clean crate that cannot depend on
//! [`ce-drive-core`]; the two converge only because their serde JSON encodings of [`MoveOp`] and
//! [`StampedContentOp`] are **byte-identical**. This test pins core's encoding to exact golden
//! strings. The identical golden strings are asserted in `ce-drive-wasm`'s own tests
//! (`wasm_wire_golden_matches_core`), so any drift in EITHER crate's field names/shape fails a test.
//! These vectors are the contract; do not change them without changing both sides intentionally.

use ce_drive_core::content::ContentOp;
use ce_drive_core::sync::StampedContentOp;
use ce_drive_core::tree::{MoveOp, NodeKind, Timestamp};

/// The canonical golden JSON for a representative MoveOp.
pub const GOLDEN_MOVE_OP: &str =
    r#"{"ts":{"lamport":7,"replica":"node-abc"},"child":"c1","new_parent":"ROOT","new_name":"x","kind":"File"}"#;

/// The canonical golden JSON for a representative StampedContentOp (a Set).
pub const GOLDEN_CONTENT_OP: &str =
    r#"{"ts":{"lamport":3,"replica":"node-xyz"},"op":{"Set":{"id":"c1","cid":"cidA","size":10,"mode":420,"mtime_ms":99}}}"#;

#[test]
fn move_op_matches_golden() {
    let op = MoveOp {
        ts: Timestamp::new(7, "node-abc"),
        child: "c1".into(),
        new_parent: "ROOT".into(),
        new_name: "x".into(),
        kind: NodeKind::File,
    };
    assert_eq!(serde_json::to_string(&op).unwrap(), GOLDEN_MOVE_OP, "core MoveOp wire drifted");
    // And the golden decodes back to the same value.
    let back: MoveOp = serde_json::from_str(GOLDEN_MOVE_OP).unwrap();
    assert_eq!(back, op);
}

#[test]
fn content_op_matches_golden() {
    let op = StampedContentOp {
        ts: Timestamp::new(3, "node-xyz"),
        op: ContentOp::Set { id: "c1".into(), cid: "cidA".into(), size: 10, mode: 0o644, mtime_ms: 99 },
    };
    assert_eq!(serde_json::to_string(&op).unwrap(), GOLDEN_CONTENT_OP, "core ContentOp wire drifted");
    let back: StampedContentOp = serde_json::from_str(GOLDEN_CONTENT_OP).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), GOLDEN_CONTENT_OP);
}
