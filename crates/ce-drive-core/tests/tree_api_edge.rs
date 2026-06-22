//! Edge-case / boundary unit coverage for the public [`DriveTree`] API surface not exercised by the
//! in-`lib.rs` tests: `is_live`, `live_count`, `log_max_ts`, `is_trashed`, `path`/`resolve` of
//! missing/trashed/root nodes, deep nesting, and empty-string / unusual names.

use ce_drive_core::tree::{DriveTree, MoveOp, NodeKind, ROOT, TRASH, Timestamp};
use ce_coord::replicated::StateMachine;

fn dir(l: u64, child: &str, parent: &str, name: &str) -> MoveOp {
    MoveOp { ts: Timestamp::new(l, "r"), child: child.into(), new_parent: parent.into(), new_name: name.into(), kind: NodeKind::Dir }
}
fn file(l: u64, child: &str, parent: &str, name: &str) -> MoveOp {
    MoveOp { ts: Timestamp::new(l, "r"), child: child.into(), new_parent: parent.into(), new_name: name.into(), kind: NodeKind::File }
}

#[test]
fn is_live_and_live_count() {
    let mut t = DriveTree::new();
    // ROOT is implicitly live; nothing else exists yet.
    assert!(t.is_live(ROOT));
    assert!(!t.is_live("ghost"), "a node with no edge (not root) is not live");
    assert_eq!(t.live_count(), 0);

    t.apply(dir(1, "d", ROOT, "d"));
    t.apply(file(2, "f", "d", "f.txt"));
    assert!(t.is_live("d"));
    assert!(t.is_live("f"));
    assert_eq!(t.live_count(), 2);

    // Trash the file: not live, not counted.
    t.apply(file(3, "f", TRASH, "f.txt"));
    assert!(!t.is_live("f"));
    assert!(t.is_trashed("f"));
    assert_eq!(t.live_count(), 1, "trashed node drops out of live_count");

    // Trash the whole dir: its (already-trashed) child stays trashed; dir not live.
    t.apply(dir(4, "d", TRASH, "d"));
    assert!(!t.is_live("d"));
    assert_eq!(t.live_count(), 0);
}

#[test]
fn log_max_ts_tracks_frontier() {
    let mut t = DriveTree::new();
    assert!(t.log_max_ts().is_none(), "empty log has no frontier");
    t.apply(dir(5, "a", ROOT, "a"));
    t.apply(dir(2, "b", ROOT, "b")); // out-of-order (earlier ts) — log stays sorted
    t.apply(dir(9, "c", ROOT, "c"));
    let max = t.log_max_ts().unwrap();
    assert_eq!(max.lamport, 9, "frontier is the greatest timestamp regardless of apply order");
}

#[test]
fn path_of_missing_trashed_and_root() {
    let mut t = DriveTree::new();
    assert_eq!(t.path(ROOT).as_deref(), Some("/"));
    assert_eq!(t.path("nope"), None, "missing node has no path");

    t.apply(dir(1, "d", ROOT, "d"));
    t.apply(file(2, "f", "d", "f.txt"));
    assert_eq!(t.path("f").as_deref(), Some("/d/f.txt"));
    // Once trashed, path() returns None (the walk doesn't reach ROOT).
    t.apply(file(3, "f", TRASH, "f.txt"));
    assert_eq!(t.path("f"), None, "trashed node derives no live path");
    assert!(t.is_trashed("f"));
}

#[test]
fn resolve_edge_cases() {
    let mut t = DriveTree::new();
    // Root resolves for "/", "", and trailing slashes.
    assert_eq!(t.resolve("/").as_deref(), Some(ROOT));
    assert_eq!(t.resolve("").as_deref(), Some(ROOT));
    assert_eq!(t.resolve("///").as_deref(), Some(ROOT));

    t.apply(dir(1, "d", ROOT, "docs"));
    t.apply(file(2, "f", "d", "spec.md"));
    assert_eq!(t.resolve("/docs").as_deref(), Some("d"));
    assert_eq!(t.resolve("/docs/spec.md").as_deref(), Some("f"));
    assert_eq!(t.resolve("docs/spec.md").as_deref(), Some("f"), "leading slash optional");
    assert_eq!(t.resolve("/docs/missing"), None);
    assert_eq!(t.resolve("/missing/deep"), None);
}

#[test]
fn deeply_nested_path_derivation() {
    let mut t = DriveTree::new();
    let mut parent = ROOT.to_string();
    let depth = 50;
    for i in 0..depth {
        let id = format!("n{i}");
        t.apply(dir(i as u64 + 1, &id, &parent, &format!("d{i}")));
        parent = id;
    }
    // The deepest node derives the full /d0/d1/.../d49 path.
    let leaf = format!("n{}", depth - 1);
    let expected: String = (0..depth).map(|i| format!("/d{i}")).collect();
    assert_eq!(t.path(&leaf).as_deref(), Some(expected.as_str()));
    assert_eq!(t.resolve(&expected).as_deref(), Some(leaf.as_str()));
    assert_eq!(t.live_count(), depth);
}

#[test]
fn raw_children_vs_readdir_on_collision() {
    let mut t = DriveTree::new();
    t.apply(dir(1, "d", ROOT, "d"));
    // Two distinct ids with the same name under d.
    t.apply(file(5, "f1", "d", "dup"));
    t.apply(file(9, "f2", "d", "dup"));
    // raw_children shows both with the bare name; readdir surfaces the conflict rename.
    let raw = t.raw_children("d");
    assert_eq!(raw.len(), 2);
    assert!(raw.iter().all(|(n, _)| n == "dup"));
    let rendered = t.readdir("d");
    assert_eq!(rendered.len(), 2);
    let names: Vec<_> = rendered.iter().map(|(n, _, _)| n.clone()).collect();
    assert!(names.contains(&"dup".to_string()), "winner keeps the bare name");
    assert!(names.iter().any(|n| n.starts_with("dup.conflict-")), "loser is conflict-renamed");
}

#[test]
fn move_into_self_is_skipped() {
    let mut t = DriveTree::new();
    t.apply(dir(1, "a", ROOT, "a"));
    // Move a under itself => cycle => skipped, a stays at root.
    t.apply(dir(2, "a", "a", "a"));
    assert_eq!(t.path("a").as_deref(), Some("/a"), "self-move skipped, node unchanged");
}

#[test]
fn empty_name_resolves_and_is_addressable() {
    // A node created with an empty name is still addressable by raw_children/path (the engine does
    // not forbid it; the CLI/mount layer is responsible for name policy). path joins it as an empty
    // segment — we assert it does not panic and is consistent.
    let mut t = DriveTree::new();
    t.apply(file(1, "f", ROOT, ""));
    assert_eq!(t.path("f").as_deref(), Some("/"), "empty-name child path joins to '/'");
    // readdir still lists it (rendered name is empty).
    let r = t.readdir(ROOT);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "");
}
