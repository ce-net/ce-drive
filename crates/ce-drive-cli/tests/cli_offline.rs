//! Offline CLI smoke + persistence tests for the `ce-drive` binary.
//!
//! These run the real compiled binary (`CARGO_BIN_EXE_ce-drive`) against a throwaway data dir and
//! identity dir, exercising the commands that need no live CE node: `init`, `ls`, `tree`, `trash`,
//! `search`, `changes`, `conflicts`, and `compact`. The key invariant under test is the persistence
//! layer: `init` must create a compact `.cedrive` (bincode+zstd) state file, and every subsequent
//! command must load it without error (a regression here is exactly the kind of half-finished
//! persistence breakage this crate had before).

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

/// Run `ce-drive <args>` with isolated data/identity dirs. Returns (success, stdout, stderr).
fn run(data: &TempDir, ident: &TempDir, args: &[&str]) -> (bool, String, String) {
    let bin = env!("CARGO_BIN_EXE_ce-drive");
    let out = Command::new(bin)
        .args(args)
        .env("CE_DRIVE_DIR", data.path())
        .env("CE_IDENTITY_DIR", ident.path())
        .output()
        .expect("spawn ce-drive");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn init_creates_compact_state_and_commands_load_it() {
    let data = TempDir::new().unwrap();
    let ident = TempDir::new().unwrap();

    // init creates a drive.
    let (ok, out, err) = run(&data, &ident, &["init"]);
    assert!(ok, "init failed: {err}");
    assert!(out.contains("initialized drive"), "unexpected init output: {out}");

    // The canonical state file is the compact `.cedrive`, NOT a legacy `.json`.
    let cedrive: PathBuf = data.path().join("default.cedrive");
    let legacy_json: PathBuf = data.path().join("default.json");
    assert!(cedrive.exists(), "init must write a .cedrive state file");
    assert!(!legacy_json.exists(), "init must not write a legacy .json state file");

    // The file is zstd-compressed bytes, not JSON text (sanity: not starting with '{').
    let bytes = std::fs::read(&cedrive).unwrap();
    assert!(!bytes.is_empty());
    assert_ne!(bytes[0], b'{', "state must be binary (bincode+zstd), not pretty JSON");

    // init is idempotent-guarded: a second init on the same drive errors.
    let (ok2, _o2, _e2) = run(&data, &ident, &["init"]);
    assert!(!ok2, "second init on an existing drive must fail");

    // Every read-only command loads the compact state without error.
    for args in [
        vec!["ls", "/"],
        vec!["tree"],
        vec!["trash"],
        vec!["conflicts"],
        vec!["search", "anything"],
        vec!["changes"],
    ] {
        let (ok, _out, err) = run(&data, &ident, &args);
        assert!(ok, "command {args:?} failed to load state: {err}");
    }
}

#[test]
fn empty_drive_reports_empty_surfaces() {
    let data = TempDir::new().unwrap();
    let ident = TempDir::new().unwrap();
    run(&data, &ident, &["init"]);

    let (_ok, out, _err) = run(&data, &ident, &["trash"]);
    assert!(out.contains("trash is empty"));

    let (_ok, out, _err) = run(&data, &ident, &["search", "nothing-here"]);
    assert!(out.contains("no matches"));

    let (_ok, out, _err) = run(&data, &ident, &["conflicts"]);
    assert!(out.contains("no content conflicts"));
}

#[test]
fn compact_on_empty_drive_is_a_noop_that_persists() {
    let data = TempDir::new().unwrap();
    let ident = TempDir::new().unwrap();
    run(&data, &ident, &["init"]);

    let (ok, out, err) = run(&data, &ident, &["compact"]);
    assert!(ok, "compact failed: {err}");
    assert!(out.contains("compacted op log"), "unexpected compact output: {out}");

    // State is still loadable after compaction.
    let (ok, _out, err) = run(&data, &ident, &["ls", "/"]);
    assert!(ok, "ls after compact failed: {err}");
}

#[test]
fn missing_drive_errors_clearly() {
    let data = TempDir::new().unwrap();
    let ident = TempDir::new().unwrap();
    // No init: ls must fail with a helpful "not found / run init" message.
    let (ok, _out, err) = run(&data, &ident, &["ls", "/"]);
    assert!(!ok, "ls on a non-existent drive must fail");
    assert!(err.contains("not found") || err.contains("init"), "error should hint at init: {err}");
}
