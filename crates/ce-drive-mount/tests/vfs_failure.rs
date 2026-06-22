//! Failure-injection + edge-case coverage for the VFS engine and the materialize fallback.
//!
//! The unit tests in `vfs.rs` cover the happy paths; here we assert the engine degrades **gracefully
//! (errors, never panics)** under the failure modes a distributed mount actually hits:
//!
//! * A **missing blob** on the read path (the CID the manifest points at is gone from the store).
//! * A **bad file handle** / **unknown inode** (kernel hands us a stale id).
//! * A **store error mid-flush** (the upload fails) — the model must not be corrupted.
//! * **Concurrent writers** to two handles on the same file (last flush wins; no panic, no deadlock).
//! * **Truncate / sparse write / past-EOF read** boundaries.
//! * **materialize** of a file whose blob is missing surfaces an error rather than writing garbage.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use ce_drive_core::content::FileContent;
use ce_drive_core::Drive;
use ce_drive_mount::store_iface::{BlockStore, PutResult};
use ce_drive_mount::vfs::{ROOT_INO, Vfs};
use ce_rs::data::cid;

/// A store that can be told to fail reads and/or writes, to inject I/O failures deterministically.
#[derive(Clone, Default)]
struct FaultStore {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fail_reads: Arc<AtomicBool>,
    fail_writes: Arc<AtomicBool>,
}

impl FaultStore {
    fn new() -> Self {
        FaultStore::default()
    }
    fn put_sync(&self, bytes: &[u8]) -> String {
        let id = cid(bytes);
        self.objects.lock().unwrap().insert(id.clone(), bytes.to_vec());
        id
    }
    fn set_fail_reads(&self, v: bool) {
        self.fail_reads.store(v, Ordering::SeqCst);
    }
    fn set_fail_writes(&self, v: bool) {
        self.fail_writes.store(v, Ordering::SeqCst);
    }
    /// Drop an object so its CID dangles (simulates a missing/garbage-collected blob).
    fn forget(&self, c: &str) {
        self.objects.lock().unwrap().remove(c);
    }
}

impl BlockStore for FaultStore {
    async fn get_object(&self, c: &str) -> Result<Vec<u8>> {
        if c.is_empty() {
            return Ok(Vec::new());
        }
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(anyhow!("injected read failure"));
        }
        self.objects
            .lock()
            .unwrap()
            .get(c)
            .cloned()
            .ok_or_else(|| anyhow!("no such object {c}"))
    }

    async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(anyhow!("injected write failure"));
        }
        let id = cid(bytes);
        self.objects.lock().unwrap().insert(id.clone(), bytes.to_vec());
        Ok(PutResult { content: FileContent::new(id, bytes.len() as u64, mode, mtime_ms), uploaded_chunks: 0 })
    }
}

fn drive_with(store: &FaultStore, name: &str, bytes: &[u8]) -> Drive {
    let mut d = Drive::init("fault", "replica-a");
    let c = store.put_sync(bytes);
    d.add_file("/", name, FileContent::new(c, bytes.len() as u64, 0o644, 1)).unwrap();
    d
}

#[tokio::test]
async fn read_of_missing_blob_errors_not_panics() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"hello");
    let vfs = Vfs::new(drive, store.clone());
    let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
    // Forget the blob: the content map still points at a CID the store no longer has.
    let cidv = cid(b"hello");
    store.forget(&cidv);
    let fh = vfs.open(ino).await.unwrap();
    let r = vfs.read(fh, 0, 5).await;
    assert!(r.is_err(), "reading a missing blob surfaces an error, not a panic");
}

#[tokio::test]
async fn injected_read_failure_is_graceful() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"data");
    let vfs = Vfs::new(drive, store.clone());
    let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
    store.set_fail_reads(true);
    let fh = vfs.open(ino).await.unwrap();
    assert!(vfs.read(fh, 0, 4).await.is_err(), "read failure propagates as Err");
    // Recovering the store lets a subsequent read succeed (no poisoned state).
    store.set_fail_reads(false);
    let fh2 = vfs.open(ino).await.unwrap();
    assert_eq!(vfs.read(fh2, 0, 4).await.unwrap(), b"data");
}

#[tokio::test]
async fn bad_handle_and_unknown_inode_error() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"x");
    let vfs = Vfs::new(drive, store.clone());
    // A handle id that was never opened.
    assert!(vfs.read(9999, 0, 1).await.is_err(), "unknown fh => Err");
    assert!(vfs.write(9999, 0, b"y").await.is_err());
    assert!(vfs.flush(9999).await.is_err());
    // An inode that does not exist.
    assert!(vfs.getattr(424242).await.is_err(), "unknown inode => Err");
    assert!(vfs.open(424242).await.is_err());
    assert!(vfs.lookup(ROOT_INO, "nope").await.is_err(), "missing name => Err");
}

#[tokio::test]
async fn flush_write_failure_leaves_handle_dirty() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"orig");
    let vfs = Vfs::new(drive, store.clone());
    let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
    let fh = vfs.open(ino).await.unwrap();
    vfs.write(fh, 0, b"NEWDATA").await.unwrap();
    // Injected upload failure: flush errors, the handle stays dirty (write not lost), model unchanged.
    store.set_fail_writes(true);
    assert!(vfs.flush(fh).await.is_err(), "flush surfaces the upload failure");
    // Size in the model is unchanged (the failed flush did not record a bad version).
    assert_eq!(vfs.getattr(ino).await.unwrap().size, 4, "model not corrupted by a failed flush");
    // Once the store recovers, a retry flush succeeds and records the new content.
    store.set_fail_writes(false);
    let res = vfs.flush(fh).await.unwrap().expect("dirty handle still flushes on retry");
    assert!(!res.cid.is_empty());
    assert_eq!(vfs.getattr(ino).await.unwrap().size, 7, "retry recorded NEWDATA");
}

#[tokio::test]
async fn concurrent_handles_last_flush_wins_no_deadlock() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"start");
    let vfs = Vfs::new(drive, store.clone());
    let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;

    // Two open handles to the same file write different content concurrently.
    let h1 = vfs.open(ino).await.unwrap();
    let h2 = vfs.open(ino).await.unwrap();
    let v1 = vfs.clone();
    let v2 = vfs.clone();
    // Each handle truncates to its own content length first, so the flush is a full overwrite (a
    // deterministic 8 or 4 bytes, never a partial-overwrite tail of the hydrated original).
    let t1 = tokio::spawn(async move {
        v1.truncate(h1, 0).await.unwrap();
        v1.write(h1, 0, b"AAAAAAAA").await.unwrap();
        v1.flush(h1).await.unwrap()
    });
    let t2 = tokio::spawn(async move {
        v2.truncate(h2, 0).await.unwrap();
        v2.write(h2, 0, b"BBBB").await.unwrap();
        v2.flush(h2).await.unwrap()
    });
    let _ = t1.await.unwrap();
    let _ = t2.await.unwrap();
    // The file converged to one of the two writes (no panic, no deadlock); the model is consistent.
    let attr = vfs.getattr(ino).await.unwrap();
    assert!(attr.size == 8 || attr.size == 4, "size reflects one of the concurrent flushes, got {}", attr.size);
    // And the recorded content is retrievable end-to-end.
    let fh = vfs.open(ino).await.unwrap();
    let back = vfs.read(fh, 0, 100).await.unwrap();
    assert!(back == b"AAAAAAAA" || back == b"BBBB", "content is one consistent write");
}

#[tokio::test]
async fn truncate_extend_and_sparse_write_boundaries() {
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"0123456789");
    let vfs = Vfs::new(drive, store.clone());
    let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
    let fh = vfs.open(ino).await.unwrap();

    // Truncate down to 4.
    vfs.truncate(fh, 4).await.unwrap();
    assert_eq!(vfs.read(fh, 0, 100).await.unwrap(), b"0123");
    // Extend (zero-fill) to 8.
    vfs.truncate(fh, 8).await.unwrap();
    assert_eq!(vfs.read(fh, 0, 100).await.unwrap(), b"0123\0\0\0\0");
    // Sparse write past current end auto-extends with zeros.
    vfs.write(fh, 10, b"Z").await.unwrap();
    let out = vfs.read(fh, 0, 100).await.unwrap();
    assert_eq!(out.len(), 11);
    assert_eq!(out[10], b'Z');
    assert_eq!(&out[8..10], b"\0\0", "gap zero-filled");
    // Past-EOF read clamps to available bytes.
    assert_eq!(vfs.read(fh, 100, 50).await.unwrap(), b"", "read entirely past EOF is empty");
    vfs.release(fh).await.unwrap();
}

#[tokio::test]
async fn materialize_missing_blob_errors() {
    use ce_drive_mount::materialize;
    let store = FaultStore::new();
    let drive = drive_with(&store, "a.txt", b"present");
    // Forget the blob so materialize cannot fetch it.
    store.forget(&cid(b"present"));
    let out = tempfile::tempdir().unwrap();
    let r = materialize(&drive, &store, out.path()).await;
    assert!(r.is_err(), "materialize of a missing blob errors rather than writing garbage");
}

#[tokio::test]
async fn create_empty_file_hydrates_to_empty_not_error() {
    // A freshly-created file has an empty CID; hydrating it must yield empty bytes, not a fetch error.
    let store = FaultStore::new();
    let drive = Drive::init("t", "a");
    let vfs = Vfs::new(drive, store.clone());
    let (attr, fh) = vfs.create(ROOT_INO, "new.txt").await.unwrap();
    // Read before any write: empty, no store fetch (empty cid path).
    let fh_read = vfs.open(attr.ino).await.unwrap();
    assert_eq!(vfs.read(fh_read, 0, 10).await.unwrap(), b"");
    let _ = fh;
}
