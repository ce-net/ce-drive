//! Error-path coverage for the single-writer [`Drive`] API (the CLI's model layer).
//!
//! The happy paths are in `drive.rs`'s unit tests; here we pin the *error* contract every CLI command
//! and mount callback depends on: duplicate names, moving/deleting the root, treating a file as a
//! directory, restoring a non-existent version, and resolving non-existent paths. All must return a
//! descriptive `Err`, never panic.

use ce_drive_core::content::FileContent;
use ce_drive_core::Drive;

fn fc(cid: &str, size: u64) -> FileContent {
    FileContent::new(cid, size, 0o644, 1)
}

#[test]
fn mkdir_duplicate_errors() {
    let mut d = Drive::init("w", "a");
    d.mkdir("/", "docs").unwrap();
    let e = d.mkdir("/", "docs").unwrap_err();
    assert!(e.to_string().contains("already exists"), "duplicate dir errors: {e}");
}

#[test]
fn mkdir_under_missing_parent_errors() {
    let mut d = Drive::init("w", "a");
    let e = d.mkdir("/nope", "x").unwrap_err();
    assert!(e.to_string().contains("no such directory"), "missing parent errors: {e}");
}

#[test]
fn mkdir_under_a_file_errors() {
    let mut d = Drive::init("w", "a");
    d.add_file("/", "f.txt", fc("c", 1)).unwrap();
    let e = d.mkdir("/f.txt", "x").unwrap_err();
    assert!(e.to_string().contains("is not a directory"), "file-as-dir errors: {e}");
}

#[test]
fn add_file_over_existing_dir_errors() {
    let mut d = Drive::init("w", "a");
    d.mkdir("/", "name").unwrap();
    let e = d.add_file("/", "name", fc("c", 1)).unwrap_err();
    assert!(e.to_string().contains("already exists as a directory"), "file-over-dir errors: {e}");
}

#[test]
fn mv_and_rm_root_error() {
    let mut d = Drive::init("w", "a");
    assert!(d.mv("/", "/", "x").unwrap_err().to_string().contains("cannot move the root"));
    assert!(d.rm("/").unwrap_err().to_string().contains("cannot delete the root"));
}

#[test]
fn mv_missing_source_errors() {
    let mut d = Drive::init("w", "a");
    let e = d.mv("/ghost", "/", "x").unwrap_err();
    assert!(e.to_string().contains("no such path"), "missing source errors: {e}");
}

#[test]
fn rm_missing_errors() {
    let mut d = Drive::init("w", "a");
    let e = d.rm("/ghost").unwrap_err();
    assert!(e.to_string().contains("no such path"));
}

#[test]
fn restore_version_without_history_errors() {
    let mut d = Drive::init("w", "a");
    d.add_file("/", "a.md", fc("cid1", 1)).unwrap();
    // A path with no content at all.
    d.mkdir("/", "dir").unwrap();
    assert!(d.restore_version("/dir", "x").unwrap_err().to_string().contains("no content history"));
    // A missing path.
    assert!(d.restore_version("/ghost", "x").unwrap_err().to_string().contains("no such path"));
}

#[test]
fn ls_on_file_errors() {
    let d = {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "f.txt", fc("c", 1)).unwrap();
        d
    };
    let e = d.ls("/f.txt").unwrap_err();
    assert!(e.to_string().contains("is not a directory"), "ls of a file errors: {e}");
}

#[test]
fn set_content_by_id_does_not_emit_moveop() {
    // The mount write-back path: set_content keys by stable id, never a MoveOp (so a concurrent
    // rename never loses the write).
    let mut d = Drive::init("w", "a");
    let id = d.add_file("/", "a.md", fc("cid1", 10)).unwrap();
    let before_moves = d.ops().len();
    d.set_content(&id, fc("cid2", 20));
    assert_eq!(d.ops().len(), before_moves, "set_content emits no MoveOp");
    assert_eq!(d.content().get(&id).unwrap().cid, "cid2");
    assert!(d.content().get(&id).unwrap().versions.iter().any(|v| v.cid == "cid1"), "old version kept");
}

#[test]
fn apply_remote_move_advances_lamport_past_seen() {
    // Reader-side merge: applying a remote op with a high lamport bumps the local clock so the next
    // local op strictly post-dates it (Lamport merge invariant).
    let mut d = Drive::init("w", "local");
    d.mkdir("/", "x").unwrap(); // lamport now 1
    // A remote op with lamport 100.
    use ce_drive_core::tree::{MoveOp, NodeKind, ROOT, Timestamp};
    d.apply_remote_move(MoveOp {
        ts: Timestamp::new(100, "remote"),
        child: "rn".into(),
        new_parent: ROOT.into(),
        new_name: "remote_dir".into(),
        kind: NodeKind::Dir,
    });
    // The next local op must carry lamport > 100.
    let id = d.mkdir("/", "y").unwrap();
    let ts = d.ops().iter().find(|o| o.child == id).unwrap().ts.lamport;
    assert!(ts > 100, "local op {ts} must post-date the observed remote op (100)");
}
