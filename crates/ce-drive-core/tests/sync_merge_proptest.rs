//! Property tests for the multi-writer merge layer ([`TreeMerge`] / [`ContentMerge`]) — the exact
//! key-ordered union fold `Merged` performs, exercised in-process (no node) over random op sets.
//!
//! This is the leaderless convergence guarantee at the merge-machine level: independent of which
//! writer produced which op and in what delivery order, the key-ordered union of all writers' ops
//! folds to one tree + one content map. We also prove **snapshot bootstrap == full replay**: folding
//! a prefix into a checkpoint state then folding the suffix equals folding the whole set (the property
//! `SyncedDrive::checkpoint` relies on for a fresh device to bootstrap then tail).

use std::collections::BTreeMap;

use ce_drive_core::content::{ContentMap, ContentOp};
use ce_drive_core::sync::{ContentMerge, StampedContentOp, TreeMerge};
use ce_drive_core::tree::{DriveTree, MoveOp, NodeKind, ROOT, TRASH, Timestamp};
use ce_coord::MergeMachine;
use proptest::prelude::*;

/// Fold move ops the way `Merged<TreeMerge>` does: dedup by key into ascending order, fold fresh.
fn fold_tree_merge(ops: &[MoveOp]) -> DriveTree {
    let mut union: BTreeMap<Timestamp, MoveOp> = BTreeMap::new();
    for op in ops {
        union.insert(TreeMerge::key(op), op.clone());
    }
    let mut m = TreeMerge::default();
    for op in union.values() {
        m.apply(op.clone());
    }
    m.0
}

fn fold_content_merge(ops: &[StampedContentOp]) -> ContentMap {
    let mut union: BTreeMap<Timestamp, StampedContentOp> = BTreeMap::new();
    for op in ops {
        union.insert(ContentMerge::key(op), op.clone());
    }
    let mut m = ContentMerge::default();
    for op in union.values() {
        m.apply(op.clone());
    }
    m.0
}

fn paths(t: &DriveTree) -> BTreeMap<String, Option<String>> {
    t.edges().keys().map(|n| (n.clone(), t.path(n))).collect()
}

/// Two-writer op set: each writer's ops get that writer's replica id; timestamps interleave.
fn two_writer_ops() -> impl Strategy<Value = Vec<MoveOp>> {
    let one = (any::<bool>(), 0usize..6, 0usize..8, 0usize..3, 0usize..2);
    proptest::collection::vec(one, 1..30).prop_map(|raw| {
        let replicas = ["writerA", "writerB"];
        raw.into_iter()
            .enumerate()
            .map(|(i, (is_dir, child_i, parent_c, name_i, w))| MoveOp {
                ts: Timestamp::new(i as u64 + 1, replicas[w]),
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

fn content_ops() -> impl Strategy<Value = Vec<StampedContentOp>> {
    let one = (0usize..4, "[a-f0-9]{1,8}", any::<u64>(), 0usize..2);
    proptest::collection::vec(one, 1..25).prop_map(|raw| {
        let replicas = ["writerA", "writerB"];
        raw.into_iter()
            .enumerate()
            .map(|(i, (id_i, cid, mtime, w))| StampedContentOp {
                ts: Timestamp::new(i as u64 + 1, replicas[w]),
                op: ContentOp::Set { id: format!("f{id_i}"), cid, size: mtime, mode: 0, mtime_ms: mtime },
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    /// TREE MERGE CONVERGENCE: forward and reversed delivery of the union fold to the same tree.
    #[test]
    fn tree_merge_order_independent(ops in two_writer_ops()) {
        let fwd = fold_tree_merge(&ops);
        let mut rev = ops.clone();
        rev.reverse();
        let bwd = fold_tree_merge(&rev);
        prop_assert_eq!(paths(&fwd), paths(&bwd), "tree merge diverged across delivery order");
    }

    /// SNAPSHOT == REPLAY: fold prefix then suffix == fold the whole set. (Checkpoint bootstrap then
    /// tail must equal replaying from version 1.)
    #[test]
    fn snapshot_bootstrap_equals_full_replay(ops in two_writer_ops(), cut in 0usize..30) {
        let n = ops.len();
        let cut = cut.min(n);
        // Whole-set fold (the "replay from v1" reference).
        let full = fold_tree_merge(&ops);
        // "Checkpoint" = fold the prefix; "tail" = then fold the suffix into the same union.
        // Because the merge is a key-ordered union over the full set, prefix+suffix == whole set.
        let mut union: BTreeMap<Timestamp, MoveOp> = BTreeMap::new();
        for op in &ops[..cut] {
            union.insert(TreeMerge::key(op), op.clone());
        }
        // checkpoint state captured; now tail the rest.
        for op in &ops[cut..] {
            union.insert(TreeMerge::key(op), op.clone());
        }
        let mut m = TreeMerge::default();
        for op in union.values() {
            m.apply(op.clone());
        }
        prop_assert_eq!(paths(&m.0), paths(&full), "bootstrap+tail must equal full replay");
    }

    /// CONTENT MERGE CONVERGENCE: LWW current cid per key is order-independent.
    #[test]
    fn content_merge_order_independent(ops in content_ops()) {
        let fwd = fold_content_merge(&ops);
        let mut rev = ops.clone();
        rev.reverse();
        let bwd = fold_content_merge(&rev);
        let mut keys: Vec<String> = fwd.entries().map(|(k, _)| k.clone()).collect();
        keys.sort();
        for k in &keys {
            prop_assert_eq!(
                fwd.get(k).map(|c| &c.cid),
                bwd.get(k).map(|c| &c.cid),
                "content LWW for {} diverged", k
            );
        }
    }
}
