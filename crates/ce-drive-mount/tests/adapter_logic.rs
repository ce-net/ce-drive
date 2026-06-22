//! Pure adapter-logic coverage that runs on every host: ProjFS path/attr mapping edge cases, the
//! macOS backend/mountpoint helpers (macOS only), and the materialize `watch` loop stop condition.

use ce_drive_mount::projfs_logic::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, basic_info_fields, ms_to_filetime,
    win_path_to_engine,
};
use ce_drive_mount::vfs::Attr;

#[test]
fn win_path_mapping_edge_cases() {
    // Root forms.
    assert_eq!(win_path_to_engine("\\"), Some("/".to_string()));
    assert_eq!(win_path_to_engine(""), Some("/".to_string()));
    // Repeated / leading / trailing separators collapse (empty components are filtered).
    assert_eq!(win_path_to_engine("\\\\src\\\\main.rs"), Some("/src/main.rs".to_string()));
    assert_eq!(win_path_to_engine("\\src\\"), Some("/src".to_string()));
    // Reserved device names and forbidden chars reject anywhere in the path.
    assert_eq!(win_path_to_engine("\\a\\PRN"), None);
    assert_eq!(win_path_to_engine("\\a\\b?c"), None);
    assert_eq!(win_path_to_engine("\\a\\b "), None, "trailing space component rejected");
    assert_eq!(win_path_to_engine("\\a\\.."), None, "dotdot component rejected");
}

#[test]
fn filetime_conversion_is_saturating_and_correct() {
    // Unix epoch (0 ms) maps to the 1601 epoch difference exactly.
    assert_eq!(ms_to_filetime(0), 116_444_736_000_000_000);
    // 1 ms = 10_000 100ns ticks.
    assert_eq!(ms_to_filetime(1), 116_444_736_000_000_000 + 10_000);
    // u64::MAX never panics (saturates).
    assert_eq!(ms_to_filetime(u64::MAX), u64::MAX);
}

#[test]
fn basic_info_fields_for_dir_and_file() {
    let dir = Attr { ino: 1, size: 0, mode: 0o755, mtime_ms: 0, is_dir: true };
    let file = Attr { ino: 2, size: 12345, mode: 0o644, mtime_ms: 5, is_dir: false };
    let d = basic_info_fields(&dir);
    assert!(d.is_directory);
    assert_eq!(d.file_attributes, FILE_ATTRIBUTE_DIRECTORY);
    assert_eq!(d.file_size, 0);
    let f = basic_info_fields(&file);
    assert!(!f.is_directory);
    assert_eq!(f.file_attributes, FILE_ATTRIBUTE_NORMAL);
    assert_eq!(f.file_size, 12345);
    assert_eq!(f.filetime, ms_to_filetime(5));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_backend_and_mountpoint_helpers() {
    use ce_drive_mount::macos_fskit::{Backend, default_mountpoint, normalize_name};
    assert_eq!(Backend::FsKit.mount_token(), "backend=fskit");
    assert_eq!(Backend::Kext.mount_token(), "backend=kext");
    assert_eq!(Backend::default(), Backend::FsKit);
    assert_eq!(default_mountpoint("team"), std::path::PathBuf::from("/Volumes/CEDrive/team"));
    // macOS name normalization: permissive (colon legal), but structural names rejected.
    assert_eq!(normalize_name("a:b"), Some("a:b".to_string()));
    assert!(normalize_name("a/b").is_none());
    assert!(normalize_name("..").is_none());
}

/// The `watch` loop honors its stop predicate immediately (no infinite loop) and runs at least one
/// push when told to continue once. Exercises the materialize watcher surface with the mock store.
#[tokio::test]
async fn watch_stops_on_predicate() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use ce_drive_core::Drive;
    use ce_drive_mount::store_iface::{BlockStore, PutResult};
    use ce_drive_core::content::FileContent;

    #[derive(Clone, Default)]
    struct NoopStore;
    impl BlockStore for NoopStore {
        async fn get_object(&self, _cid: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
            Ok(PutResult {
                content: FileContent::new(ce_rs::data::cid(bytes), bytes.len() as u64, mode, mtime_ms),
                uploaded_chunks: 0,
            })
        }
    }

    let mut drive = Drive::init("w", "a");
    let store = NoopStore;
    let dir = tempfile::tempdir().unwrap();
    // Stop after the first iteration: the predicate flips to true once it has been polled twice
    // (once at loop top before push, once after). Count invocations.
    let polls = Arc::new(AtomicUsize::new(0));
    let polls2 = polls.clone();
    let pushes = Arc::new(AtomicUsize::new(0));
    let pushes2 = pushes.clone();
    ce_drive_mount::watch(
        &mut drive,
        &store,
        dir.path(),
        Duration::from_millis(1),
        move |_d, _r| {
            pushes2.fetch_add(1, Ordering::SeqCst);
        },
        move || polls2.fetch_add(1, Ordering::SeqCst) >= 1, // stop on the 2nd poll
    )
    .await
    .unwrap();
    // First poll returns false (0 < 1) -> one push; second poll returns true -> stop.
    assert_eq!(pushes.load(Ordering::SeqCst), 1, "exactly one push before stop");
}
