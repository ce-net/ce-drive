//! Property / fuzz tests for the move-CRDT directory tree ([`DriveTree`]).
//!
//! These are the load-bearing convergence guarantees of the whole filesystem. We assert, over
//! thousands of randomly-generated op sequences, the four properties Kleppmann's move-CRDT must hold:
//!
//! 1. **Convergence (commutativity by total order).** Any permutation of the same op set folds to the
//!    *identical* acyclic tree — the derived (node -> path) map is byte-identical regardless of
//!    delivery order. This is the single most important property; a divergence here is data loss.
//! 2. **Idempotence.** Re-delivering ops (duplicates) never changes the converged state.
//! 3. **Acyclicity / no-cycle invariant.** After any op sequence, no live node is its own ancestor;
//!    every reachable node derives a finite path to ROOT (the cycle-skip held).
//! 4. **Determinism of conflict-rename.** Name collisions render identically on every replica
//!    (a pure function of the converged op set).
//!
//! The generator builds *valid-shaped* op streams (creates of fresh ids, then moves of existing ids
//! to existing parents, including deliberate cycle attempts) so the undo/redo + cycle-skip paths are
//! exercised under genuinely concurrent (out-of-timestamp-order) delivery.

use std::collections::BTreeMap;

use ce_drive_core::tree::{DriveTree, MoveOp, NodeKind, ROOT, TRASH, Timestamp};
use ce_coord::replicated::StateMachine;
use proptest::prelude::*;

/// A deterministic dump of (node -> derived path) — the convergence fingerprint.
fn paths(t: &DriveTree) -> BTreeMap<String, Option<String>> {
    t.edges().keys().map(|n| (n.clone(), t.path(n))).collect()
}

/// Fold an op set in a given delivery order into a fresh tree.
fn fold(ops: &[MoveOp], order: &[usize]) -> DriveTree {
    let mut t = DriveTree::new();
    for &i in order {
        t.apply(ops[i].clone());
    }
    t
}

/// Generate a structurally-plausible op set. We keep a small id/parent universe so collisions, moves
/// of existing nodes, and cycle attempts all actually occur. Timestamps are unique by construction
/// (a global counter for lamport, so every op has a distinct key — the CRDT's uniqueness precondition).
fn op_set_strategy() -> impl Strategy<Value = Vec<MoveOp>> {
    // Each op is (kind_is_dir, child_index, parent_choice, name_index, replica_index).
    let one_op = (
        any::<bool>(),
        0usize..8,    // child id index (small universe => reuse => moves & collisions)
        0usize..10,   // parent choice (0=ROOT, 1=TRASH, 2..=existing-id)
        0usize..4,    // name index (small => collisions)
        0usize..3,    // replica index
    );
    proptest::collection::vec(one_op, 1..40).prop_map(|raw| {
        let replicas = ["aaaaaaaa", "bbbbbbbb", "cccccccc"];
        let mut ops = Vec::with_capacity(raw.len());
        for (i, (is_dir, child_i, parent_c, name_i, rep_i)) in raw.into_iter().enumerate() {
            let child = format!("n{child_i}");
            let new_parent = match parent_c {
                0 => ROOT.to_string(),
                1 => TRASH.to_string(),
                other => format!("n{}", other - 2), // an id that may or may not exist yet
            };
            let kind = if is_dir { NodeKind::Dir } else { NodeKind::File };
            // Unique, monotone-ish lamport from the raw index but shuffled by replica so delivery
            // order != timestamp order in permutations. Distinct (lamport, replica) per op.
            let lamport = (i as u64) + 1;
            ops.push(MoveOp {
                ts: Timestamp::new(lamport, replicas[rep_i]),
                child,
                new_parent,
                new_name: format!("name{name_i}"),
                kind,
            });
        }
        ops
    })
}

/// A few fixed permutations of `n` indices, covering forward, reverse, and rotations — each a valid
/// out-of-order delivery. (We avoid a full shuffle strategy to keep the property fast and the failure
/// reproducible; these orders already trigger undo/redo because timestamps are not delivery order.)
fn permutations(n: usize) -> Vec<Vec<usize>> {
    let forward: Vec<usize> = (0..n).collect();
    let reverse: Vec<usize> = (0..n).rev().collect();
    let mut rot = forward.clone();
    rot.rotate_left(n / 2);
    let mut interleave = Vec::with_capacity(n);
    let mut lo = 0;
    let mut hi = n - 1;
    while lo <= hi {
        interleave.push(lo);
        if hi != lo {
            interleave.push(hi);
        }
        lo += 1;
        if hi == 0 {
            break;
        }
        hi -= 1;
    }
    vec![forward, reverse, rot, interleave]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// CONVERGENCE: every delivery permutation of the same op set yields the identical tree.
    #[test]
    fn random_op_orders_converge_identically(ops in op_set_strategy()) {
        let n = ops.len();
        let reference = paths(&fold(&ops, &(0..n).collect::<Vec<_>>()));
        for order in permutations(n) {
            let got = paths(&fold(&ops, &order));
            prop_assert_eq!(&got, &reference, "delivery order {:?} diverged", order);
        }
    }

    /// IDEMPOTENCE: applying the whole set, then applying it all again, changes nothing.
    #[test]
    fn duplicate_delivery_is_idempotent(ops in op_set_strategy()) {
        let n = ops.len();
        let once = fold(&ops, &(0..n).collect::<Vec<_>>());
        let mut twice = once.clone();
        // Re-deliver every op (some out of order) — all are duplicates by timestamp.
        for order in permutations(n) {
            for &i in &order {
                twice.apply(ops[i].clone());
            }
        }
        prop_assert_eq!(paths(&twice), paths(&once), "re-delivery must be a no-op");
        prop_assert_eq!(twice.log_len(), once.log_len(), "log must not grow on duplicates");
    }

    /// ACYCLICITY (the cycle-skip invariant): walking parent pointers from any node terminates
    /// without revisiting it — no node is part of a cycle. We test the no-cycle property directly
    /// rather than reachability-to-ROOT: the random generator can legitimately parent a node under a
    /// never-created ("limbo") id, which has no path to ROOT and is not trashed yet is still acyclic.
    /// (The real Drive/SyncedDrive API always resolves the parent first, so limbo cannot occur there;
    /// the CRDT must nonetheless stay acyclic for ANY op set, which is what this asserts.)
    #[test]
    fn tree_is_always_acyclic(ops in op_set_strategy()) {
        let t = fold(&ops, &(0..ops.len()).collect::<Vec<_>>());
        for node in t.edges().keys() {
            // A node is never strictly its own ancestor via its parent chain.
            if let Some(edge) = t.edge(node) {
                prop_assert!(
                    !t.is_ancestor(node, &edge.parent) || &edge.parent == node,
                    "node {node} became its own ancestor (cycle)"
                );
            }
            // Direct cycle walk: following parents from `node` must never return to `node`.
            let mut cur = match t.edge(node) {
                Some(e) => e.parent.clone(),
                None => continue,
            };
            let mut cycled = false;
            for _ in 0..=t.edges().len() {
                if cur == *node {
                    cycled = true;
                    break;
                }
                match t.edge(&cur) {
                    Some(e) => cur = e.parent.clone(),
                    None => break, // reached ROOT/TRASH/limbo
                }
            }
            prop_assert!(!cycled, "node {node} is part of a parent-pointer cycle");
        }
    }

    /// CONFLICT-RENAME DETERMINISM: the rendered readdir of every directory is identical across
    /// delivery permutations (a pure function of the converged op set), and contains no duplicate
    /// rendered names (collisions are always surfaced, never hidden).
    #[test]
    fn conflict_render_is_order_independent_and_unique(ops in op_set_strategy()) {
        let n = ops.len();
        let reference = fold(&ops, &(0..n).collect::<Vec<_>>());
        // Every parent that appears in the converged tree (plus ROOT/TRASH).
        let mut parents: Vec<String> = reference.edges().values().map(|e| e.parent.clone()).collect();
        parents.push(ROOT.to_string());
        parents.push(TRASH.to_string());
        parents.sort();
        parents.dedup();

        for order in permutations(n) {
            let t = fold(&ops, &order);
            for p in &parents {
                let a: Vec<_> = reference.readdir(p).into_iter().map(|(n, _, _)| n).collect();
                let b: Vec<_> = t.readdir(p).into_iter().map(|(n, _, _)| n).collect();
                prop_assert_eq!(&a, &b, "readdir({}) differs across delivery order", p);
                // No duplicate rendered names in a single listing.
                let mut sorted = b.clone();
                sorted.sort();
                let mut deduped = sorted.clone();
                deduped.dedup();
                prop_assert_eq!(sorted.len(), deduped.len(), "duplicate rendered name in readdir({})", p);
            }
        }
    }
}

/// A focused, non-random regression: two concurrent dir moves that would form a cycle must converge
/// to a single acyclic outcome no matter which arrives first (the undo/redo swap). This complements
/// the in-`lib.rs` unit test with a deeper 3-level subtree so descendant path-rederivation is tested.
#[test]
fn deep_concurrent_swap_converges() {
    let base = [
        MoveOp { ts: Timestamp::new(1, "a"), child: "A".into(), new_parent: ROOT.into(), new_name: "A".into(), kind: NodeKind::Dir },
        MoveOp { ts: Timestamp::new(2, "a"), child: "B".into(), new_parent: ROOT.into(), new_name: "B".into(), kind: NodeKind::Dir },
        MoveOp { ts: Timestamp::new(3, "a"), child: "A1".into(), new_parent: "A".into(), new_name: "A1".into(), kind: NodeKind::Dir },
        MoveOp { ts: Timestamp::new(4, "a"), child: "B1".into(), new_parent: "B".into(), new_name: "B1".into(), kind: NodeKind::Dir },
    ];
    // Concurrent: move A under B1 (deep), and B under A1 (deep) — a cycle if both applied.
    let x = MoveOp { ts: Timestamp::new(10, "a"), child: "A".into(), new_parent: "B1".into(), new_name: "A".into(), kind: NodeKind::Dir };
    let y = MoveOp { ts: Timestamp::new(11, "b"), child: "B".into(), new_parent: "A1".into(), new_name: "B".into(), kind: NodeKind::Dir };

    let mut t1 = DriveTree::new();
    for op in base.iter().chain([&x, &y]) {
        t1.apply(op.clone());
    }
    let mut t2 = DriveTree::new();
    // Reverse the concurrent pair (out-of-order delivery triggers undo/redo).
    for op in base.iter().chain([&y, &x]) {
        t2.apply(op.clone());
    }
    assert_eq!(paths(&t1), paths(&t2), "deep concurrent swap must converge");
    // Acyclic: every node reaches ROOT or is trashed.
    for n in t1.edges().keys() {
        assert!(t1.path(n).is_some() || t1.is_trashed(n), "node {n} orphaned");
    }
}
