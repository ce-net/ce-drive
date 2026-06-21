//! The driverless `materialize` fallback — works everywhere, no kernel driver.
//!
//! For CI runners, containers without `/dev/fuse`, and no-admin machines the lazy kernel mount is
//! not an option. The fallback (design §7.5) is a plain directory tree:
//!
//! * [`materialize`] walks the [`Drive`] manifest and fetches every file by CID (verified) into a
//!   real local directory — native-speed, works everywhere, no driver. Default in CI/containers.
//! * [`push`] re-chunks changed files in that directory back into CE blobs and updates the tree
//!   (this is literally the `rdev syncd` engine pointed at a CE Drive), giving the
//!   "materialize -> edit/build natively -> sync back" loop.
//! * [`watch`] runs [`push`] on an interval (a poll-based watcher; a future enhancement swaps in
//!   `notify`/fsevents/inotify behind the same surface).
//!
//! All paths are store-generic over [`BlockStore`], so the fallback is unit-tested with the mock
//! store (a full materialize -> edit -> push -> re-materialize round-trip) with no live node.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ce_drive_core::Drive;

use crate::store_iface::BlockStore;

/// Outcome of a [`materialize`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    /// Number of files written to disk.
    pub files: usize,
    /// Number of directories created.
    pub dirs: usize,
    /// Total bytes written.
    pub bytes: u64,
}

/// Outcome of a [`push`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    /// Files that were new or changed and re-chunked into the drive.
    pub changed: usize,
    /// Files deleted from disk and removed (trashed) in the drive.
    pub removed: usize,
    /// Total chunks uploaded across all changed files (delta — only missing chunks).
    pub uploaded_chunks: usize,
}

/// Materialize a whole drive (from `/`) into `out`: every directory becomes a real directory and
/// every file is fetched by CID (verified, block-wise) and written. Native-speed; no driver.
pub async fn materialize<S: BlockStore>(drive: &Drive, store: &S, out: &Path) -> Result<MaterializeReport> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let mut report = MaterializeReport::default();
    materialize_dir(drive, store, "/", out, &mut report).await?;
    Ok(report)
}

/// Recursively materialize one subtree. Boxed recursion (async fn can't recurse directly).
fn materialize_dir<'a, S: BlockStore>(
    drive: &'a Drive,
    store: &'a S,
    path: &'a str,
    out: &'a Path,
    report: &'a mut MaterializeReport,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        for e in drive.ls(path)? {
            let target = out.join(&e.name);
            if e.is_dir {
                std::fs::create_dir_all(&target).with_context(|| format!("mkdir {}", target.display()))?;
                report.dirs += 1;
                let child = join(path, &e.name);
                materialize_dir(drive, store, &child, &target, report).await?;
            } else if let Some(content) = &e.content {
                let bytes = if content.cid.is_empty() {
                    Vec::new()
                } else {
                    store.get_object(&content.cid).await.with_context(|| format!("fetch {}", e.name))?
                };
                std::fs::write(&target, &bytes).with_context(|| format!("write {}", target.display()))?;
                report.files += 1;
                report.bytes += bytes.len() as u64;
            }
        }
        Ok(())
    })
}

/// Push local changes from a materialized directory `src` back into the drive: any file whose bytes
/// differ from the drive's recorded content (or that is new) is re-chunked + delta-uploaded and its
/// content updated; any drive file missing on disk is trashed. Mutates `drive` in place — the caller
/// persists it. Returns what changed.
pub async fn push<S: BlockStore>(drive: &mut Drive, store: &S, src: &Path) -> Result<PushReport> {
    let mut report = PushReport::default();
    // Snapshot the on-disk set so we can detect drive files that were deleted locally.
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    collect_disk_paths(src, "/", &mut on_disk)?;

    // 1) Add/update every on-disk file.
    push_dir(drive, store, src, "/", &mut report).await?;

    // 2) Trash drive files that no longer exist on disk.
    let drive_files = drive_file_paths(drive, "/");
    for p in drive_files {
        if !on_disk.contains(&p) {
            drive.rm(&p).with_context(|| format!("trash {p}"))?;
            report.removed += 1;
        }
    }
    Ok(report)
}

/// Run [`push`] on a fixed interval until `should_stop` returns true. A simple, dependency-free
/// poll-based watcher (the design notes a future swap to `notify`/fsevents/inotify behind the same
/// surface). Persistence between iterations is the caller's responsibility via `on_push`.
pub async fn watch<S, F, Stop>(
    drive: &mut Drive,
    store: &S,
    src: &Path,
    interval: Duration,
    mut on_push: F,
    mut should_stop: Stop,
) -> Result<()>
where
    S: BlockStore,
    F: FnMut(&Drive, &PushReport),
    Stop: FnMut() -> bool,
{
    loop {
        if should_stop() {
            return Ok(());
        }
        let report = push(drive, store, src).await?;
        on_push(drive, &report);
        tokio::time::sleep(interval).await;
    }
}

// ---- internals ----

fn push_dir<'a, S: BlockStore>(
    drive: &'a mut Drive,
    store: &'a S,
    disk_dir: &'a Path,
    drive_path: &'a str,
    report: &'a mut PushReport,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let read = std::fs::read_dir(disk_dir).with_context(|| format!("read dir {}", disk_dir.display()))?;
        // Deterministic order so the op log is reproducible.
        let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().with_context(|| format!("stat {name}"))?;
            let child_drive = join(drive_path, &name);
            if meta.is_dir() {
                ensure_dir(drive, drive_path, &name)?;
                push_dir(drive, store, &entry.path(), &child_drive, report).await?;
            } else if meta.is_file() {
                let bytes = std::fs::read(entry.path()).with_context(|| format!("read {name}"))?;
                // Skip unchanged files: compare the would-be object CID to the recorded one.
                let existing_cid = drive
                    .tree()
                    .resolve(&child_drive)
                    .and_then(|id| drive.content().get(&id).map(|c| c.cid.clone()));
                let put = store.put_bytes(&bytes, mode_of(&meta), mtime_ms_of(&meta)).await?;
                if existing_cid.as_deref() == Some(put.content.cid.as_str()) {
                    continue; // identical content already in the drive
                }
                ensure_dir_path(drive, drive_path)?;
                drive
                    .add_file(drive_path, &name, put.content.clone())
                    .with_context(|| format!("record {child_drive}"))?;
                report.changed += 1;
                report.uploaded_chunks += put.uploaded_chunks;
            }
        }
        Ok(())
    })
}

/// Ensure a single child directory exists in the drive under `parent`.
fn ensure_dir(drive: &mut Drive, parent: &str, name: &str) -> Result<()> {
    let child = join(parent, name);
    if drive.tree().resolve(&child).is_none() {
        ensure_dir_path(drive, parent)?;
        drive.mkdir(parent, name)?;
    }
    Ok(())
}

/// Ensure every directory along an absolute path exists (mkdir -p).
fn ensure_dir_path(drive: &mut Drive, dir: &str) -> Result<()> {
    if dir == "/" || drive.tree().resolve(dir).is_some() {
        return Ok(());
    }
    let mut cur = String::from("/");
    for comp in dir.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
        let child = join(&cur, comp);
        if drive.tree().resolve(&child).is_none() {
            drive.mkdir(&cur, comp)?;
        }
        cur = child;
    }
    Ok(())
}

/// Collect every file path present under `src` on disk (drive-relative absolute paths).
fn collect_disk_paths(src: &Path, drive_path: &str, out: &mut BTreeSet<String>) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src).with_context(|| format!("read dir {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata()?;
        let child = join(drive_path, &name);
        if meta.is_dir() {
            collect_disk_paths(&entry.path(), &child, out)?;
        } else if meta.is_file() {
            out.insert(child);
        }
    }
    Ok(())
}

/// Every live file path in the drive (for deletion detection).
fn drive_file_paths(drive: &Drive, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = drive.ls(path) {
        for e in entries {
            let child = join(path, &e.name);
            if e.is_dir {
                out.extend(drive_file_paths(drive, &child));
            } else {
                out.push(child);
            }
        }
    }
    out
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convenience: the default local materialization root for a drive name (so the CLI and tests agree).
pub fn default_materialize_dir(base: &Path, drive: &str) -> PathBuf {
    base.join(drive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_iface::MockStore;
    use ce_drive_core::content::FileContent;

    fn seeded_drive(store: &MockStore) -> Drive {
        let mut d = Drive::init("t", "a");
        let cid = store.put_sync(b"hello world");
        d.add_file("/", "readme.txt", FileContent::new(cid, 11, 0o644, 1)).unwrap();
        d.mkdir("/", "src").unwrap();
        let cid2 = store.put_sync(b"fn main() {}");
        d.add_file("/src", "main.rs", FileContent::new(cid2, 12, 0o644, 1)).unwrap();
        d
    }

    #[tokio::test]
    async fn materialize_writes_the_tree() {
        let store = MockStore::new();
        let drive = seeded_drive(&store);
        let tmp = tempfile::tempdir().unwrap();
        let report = materialize(&drive, &store, tmp.path()).await.unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.dirs, 1);
        assert_eq!(std::fs::read(tmp.path().join("readme.txt")).unwrap(), b"hello world");
        assert_eq!(std::fs::read(tmp.path().join("src/main.rs")).unwrap(), b"fn main() {}");
    }

    #[tokio::test]
    async fn materialize_edit_push_roundtrip() {
        let store = MockStore::new();
        let mut drive = seeded_drive(&store);
        let tmp = tempfile::tempdir().unwrap();
        materialize(&drive, &store, tmp.path()).await.unwrap();

        // Edit a file natively, add a new one, delete another.
        std::fs::write(tmp.path().join("readme.txt"), b"hello CE drive").unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), b"pub fn f() {}").unwrap();
        std::fs::remove_file(tmp.path().join("src/main.rs")).unwrap();

        let report = push(&mut drive, &store, tmp.path()).await.unwrap();
        assert_eq!(report.changed, 2, "edited readme + new lib.rs");
        assert_eq!(report.removed, 1, "deleted main.rs trashed");

        // Re-materialize into a fresh dir and confirm the round-trip landed.
        let tmp2 = tempfile::tempdir().unwrap();
        materialize(&drive, &store, tmp2.path()).await.unwrap();
        assert_eq!(std::fs::read(tmp2.path().join("readme.txt")).unwrap(), b"hello CE drive");
        assert_eq!(std::fs::read(tmp2.path().join("src/lib.rs")).unwrap(), b"pub fn f() {}");
        assert!(!tmp2.path().join("src/main.rs").exists(), "deleted file gone after push");
    }

    #[tokio::test]
    async fn push_skips_unchanged_files() {
        let store = MockStore::new();
        let mut drive = seeded_drive(&store);
        let tmp = tempfile::tempdir().unwrap();
        materialize(&drive, &store, tmp.path()).await.unwrap();
        // No edits: a push changes nothing.
        let report = push(&mut drive, &store, tmp.path()).await.unwrap();
        assert_eq!(report.changed, 0, "unchanged files are skipped (CID compare)");
        assert_eq!(report.removed, 0);
    }
}
