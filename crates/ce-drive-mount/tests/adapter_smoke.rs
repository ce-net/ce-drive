//! Mount-adapter contract smoke — the per-OS adapter ↔ [`Vfs`] engine wiring, exercised on **any**
//! host with no kernel driver.
//!
//! Every OS adapter (fuser on Linux, macFUSE-FSKit on macOS, WinFsp/ProjFS on Windows) is the same
//! thin translation: a kernel callback resolves a request against the shared [`Vfs`] engine and fills
//! the platform's reply struct. The kernel callbacks themselves need a real FUSE/macFUSE/ProjFS
//! provider to drive, so they can't run on this Mac — but the **contract they depend on** (the exact
//! sequence of engine calls each callback makes, and the path/attr mapping the Windows adapters use)
//! is OS-independent and is what actually has to be correct. This test drives that contract directly:
//!
//! 1. **The FUSE/WinFsp lookup→open→read→write→flush→release call sequence** (what `Filesystem::*` /
//!    `FileSystemContext::*` translate into) against a real drive, asserting lazy hydration and
//!    write-back behave as the adapters assume.
//! 2. **`readdirplus` + `readdirplus_path`** — the manifest-only projection both the FUSE
//!    `readdirplus` callback and the ProjFS `GetDirectoryEnumeration` callback rely on (listing never
//!    downloads bytes).
//! 3. **The ProjFS path/attr mapping** ([`ce_drive_mount::projfs_logic`]) end-to-end against the same
//!    drive: a Win32 backslash path maps to the engine path, and the engine attr → `PRJ_FILE_BASIC_INFO`
//!    fields are correct — the exact logic the `cfg(windows)` ProjFS callbacks call into, verified
//!    here on a non-Windows host (the whole point of factoring it out of the `cfg(windows)` module).
//! 4. **Atomic rename preserves the inode** — the stable-identity invariant every adapter needs so
//!    `make`/`cargo` don't spuriously rebuild.
//!
//! The store is a faithful in-memory mirror of the real [`Store`](ce_drive_core::Store): it chunks via
//! the genuine `ce_rs::data` engine and keys by sha256 CIDs, so the read/write paths move real
//! content-addressed bytes (not mocks of them).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use ce_drive_core::content::FileContent;
use ce_drive_core::{Drive, DriveState};
use ce_drive_mount::projfs_logic;
use ce_drive_mount::store_iface::{BlockStore, PutResult};
use ce_drive_mount::vfs::{ROOT_INO, Vfs};
use ce_rs::data::{Manifest, chunk_object, reassemble};

const CHUNK_SIZE: usize = 1024 * 1024;

/// In-memory content-addressed store mirroring the real [`Store`]: chunk via `ce_rs::data`, key by
/// CID, record the manifest's own hash as the object CID. Counts object fetches so we can assert lazy
/// hydration ("listing never downloads; only touched files hydrate").
#[derive(Clone, Default)]
struct MemStore {
    chunks: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    manifests: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    object_fetches: Arc<AtomicUsize>,
}

impl MemStore {
    fn new() -> Self {
        MemStore::default()
    }
    fn object_fetches(&self) -> usize {
        self.object_fetches.load(Ordering::SeqCst)
    }
    /// Store bytes the way the real store does; returns the object CID.
    fn store(&self, bytes: &[u8]) -> String {
        let (manifest, chunks) = chunk_object(bytes, CHUNK_SIZE);
        {
            let mut held = self.chunks.lock().expect("lock");
            for (cid, data) in &chunks {
                held.insert(cid.clone(), data.clone());
            }
        }
        let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
        let object_cid = ce_rs::data::cid(&manifest_bytes);
        self.manifests.lock().expect("lock").insert(object_cid.clone(), manifest_bytes);
        object_cid
    }
    fn load_manifest(&self, object_cid: &str) -> Result<Manifest> {
        let raw = self
            .manifests
            .lock()
            .expect("lock")
            .get(object_cid)
            .cloned()
            .ok_or_else(|| anyhow!("no manifest for {object_cid}"))?;
        serde_json::from_slice(&raw).context("deserialize manifest")
    }
}

impl BlockStore for MemStore {
    async fn get_object(&self, cid: &str) -> Result<Vec<u8>> {
        if cid.is_empty() {
            return Ok(Vec::new());
        }
        self.object_fetches.fetch_add(1, Ordering::SeqCst);
        let manifest = self.load_manifest(cid)?;
        let chunks = self.chunks.clone();
        reassemble(&manifest, |chunk_cid| {
            chunks
                .lock()
                .expect("lock")
                .get(chunk_cid)
                .cloned()
                .ok_or_else(|| anyhow!("missing chunk {chunk_cid}"))
        })
    }

    async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
        let object_cid = self.store(bytes);
        Ok(PutResult {
            content: FileContent::new(object_cid, bytes.len() as u64, mode, mtime_ms),
            uploaded_chunks: 0,
        })
    }
}

/// A small drive: `/README.md`, `/src/main.rs`, `/src/big.bin` (multi-chunk).
fn seed_drive(store: &MemStore) -> Drive {
    let mut d = Drive::init("adapter-smoke", "replica-a");
    let readme = store.store(b"# adapter smoke\n");
    d.add_file("/", "README.md", FileContent::new(readme, 16, 0o644, 10)).unwrap();
    d.mkdir("/", "src").unwrap();
    let main = store.store(b"fn main() { println!(\"hi\"); }\n");
    d.add_file("/src", "main.rs", FileContent::new(main, 30, 0o644, 20)).unwrap();
    let big = store.store(&vec![0x5Au8; 2 * CHUNK_SIZE + 7]);
    d.add_file("/src", "big.bin", FileContent::new(big, (2 * CHUNK_SIZE + 7) as u64, 0o644, 30)).unwrap();
    d
}

/// The FUSE/WinFsp read-path callback sequence: lookup → open → read. Lazy hydration: listing pulls
/// nothing; reading one small file pulls exactly that one object, never `big.bin`.
#[tokio::test]
async fn read_path_call_sequence_is_lazy() -> Result<()> {
    let store = MemStore::new();
    let drive = seed_drive(&store);
    let vfs = Vfs::new(Drive::from_state(drive.state().clone()), store.clone());

    // readdirplus (FUSE `readdirplus` / ProjFS enum): manifest-only, zero object fetches.
    let root = vfs.readdirplus(ROOT_INO).await?;
    assert!(root.iter().any(|e| e.name == "README.md"));
    assert!(root.iter().any(|e| e.name == "src" && e.attr.is_dir));
    assert_eq!(store.object_fetches(), 0, "listing never downloads bytes");

    // lookup → open → read (the FUSE lookup/open/read callbacks) on the small file only.
    let src = vfs.lookup(ROOT_INO, "src").await?;
    let main = vfs.lookup(src.ino, "main.rs").await?;
    let fh = vfs.open(main.ino).await?;
    let data = vfs.read(fh, 0, 4096).await?;
    assert!(data.starts_with(b"fn main()"));
    assert_eq!(store.object_fetches(), 1, "only the touched file hydrated, not big.bin");

    // A second read hits the engine buffer — no extra fetch (the cache the adapters rely on).
    let _ = vfs.read(fh, 0, 8).await?;
    assert_eq!(store.object_fetches(), 1, "cached read does no network");
    vfs.release(fh).await?;
    Ok(())
}

/// The FUSE/WinFsp write-path callback sequence: open → write → flush → release. Write-back: writes
/// buffer; flush produces a new content CID; the model reflects the new size.
#[tokio::test]
async fn write_path_call_sequence_writes_back() -> Result<()> {
    let store = MemStore::new();
    let drive = seed_drive(&store);
    let vfs = Vfs::new(Drive::from_state(drive.state().clone()), store.clone());

    let readme = vfs.lookup(ROOT_INO, "README.md").await?;
    let fh = vfs.open(readme.ino).await?;
    vfs.write(fh, 0, b"# adapter smoke v2 longer\n").await?;
    // The release callback flushes write-back and returns the new content.
    let flushed = vfs.release(fh).await?.expect("dirty handle flushed on release");
    assert!(!flushed.cid.is_empty(), "flush recorded a new content CID");

    // Re-read through a fresh handle (close-to-open): the new bytes round-tripped via the store.
    let fh2 = vfs.open(readme.ino).await?;
    let back = vfs.read(fh2, 0, 4096).await?;
    assert_eq!(back, b"# adapter smoke v2 longer\n");
    vfs.release(fh2).await?;
    Ok(())
}

/// The ProjFS adapter contract: `readdirplus_path` (what `GetDirectoryEnumeration` calls) + the
/// `projfs_logic` Win32-path→engine-path mapping + `PRJ_FILE_BASIC_INFO` field derivation, end-to-end
/// against the real drive — the exact logic the `cfg(windows)` ProjFS callbacks invoke, verified here.
#[tokio::test]
async fn projfs_contract_against_real_drive() -> Result<()> {
    let store = MemStore::new();
    let drive = seed_drive(&store);
    let vfs = Vfs::new(Drive::from_state(drive.state().clone()), store.clone());

    // GetDirectoryEnumeration("\\src") → engine path "/src" → readdirplus_path, no bytes.
    let engine_path = projfs_logic::win_path_to_engine("\\src").expect("\\src maps");
    assert_eq!(engine_path, "/src");
    let entries = vfs.readdirplus_path(&engine_path).await?;
    let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&"main.rs".to_string()));
    assert!(names.contains(&"big.bin".to_string()));
    assert_eq!(store.object_fetches(), 0, "ProjFS enumeration projects placeholders, no hydration");

    // GetPlaceholderInfo("\\src\\main.rs") → lookup_path → PRJ_FILE_BASIC_INFO fields.
    let main_engine = projfs_logic::win_path_to_engine("\\src\\main.rs").expect("maps");
    let attr = vfs.lookup_path(&main_engine).await?;
    let basic = projfs_logic::basic_info_fields(&attr);
    assert!(!basic.is_directory);
    assert_eq!(basic.file_size, 30);
    assert_eq!(basic.file_attributes, projfs_logic::FILE_ATTRIBUTE_NORMAL);
    // mtime 20 ms → FILETIME is the 1601 epoch + 20*10_000 ticks.
    assert_eq!(basic.filetime, projfs_logic::ms_to_filetime(20));

    // A directory placeholder is marked as a directory.
    let dir_attr = vfs.lookup_path("/src").await?;
    assert!(projfs_logic::basic_info_fields(&dir_attr).is_directory);

    // GetFileData("\\src\\main.rs", 0, len) → engine read → bytes to hydrate into NTFS.
    let fh = vfs.open(attr.ino).await?;
    let bytes = vfs.read(fh, 0, 4096).await?;
    assert!(bytes.starts_with(b"fn main()"), "GetFileData hydrates the real bytes");
    vfs.release(fh).await?;

    // An illegal Win32 component (reserved name) is rejected by the mapping (callback returns 404).
    assert!(projfs_logic::win_path_to_engine("\\src\\CON").is_none());
    Ok(())
}

/// Atomic rename preserves the inode — the stable-identity invariant every adapter's `rename`
/// callback depends on (or `make`/`cargo` spuriously rebuild). Also survives a close-to-open
/// revalidate (remount), as the adapters require.
#[tokio::test]
async fn rename_preserves_inode_across_revalidate() -> Result<()> {
    let store = MemStore::new();
    let drive = seed_drive(&store);
    let state: DriveState = drive.state().clone();
    let vfs = Vfs::new(Drive::from_state(state.clone()), store.clone());

    let before = vfs.lookup(ROOT_INO, "README.md").await?.ino;
    vfs.rename(ROOT_INO, "README.md", ROOT_INO, "READING.md").await?;
    assert!(vfs.lookup(ROOT_INO, "README.md").await.is_err(), "old name gone");
    let after = vfs.lookup(ROOT_INO, "READING.md").await?.ino;
    assert_eq!(before, after, "rename preserves inode (no spurious rebuild)");

    // After a revalidate the inode for the same node is preserved.
    vfs.revalidate(vfs.state().await).await;
    let again = vfs.lookup(ROOT_INO, "READING.md").await?.ino;
    assert_eq!(after, again, "inode stable across remount");
    Ok(())
}
