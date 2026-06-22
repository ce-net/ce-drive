//! CE Drive mount — **real perf smoke** (the make-or-break dev-tool-latency signal).
//!
//! The design's #1 risk (`PLAN/10-drive-fs.md` §16) is that dev tooling over a distributed mount is
//! metadata-latency-bound and small-file-storm shaped, and naive distributed POSIX filesystems are
//! bad at exactly `git status` / `cargo build`. This test captures **real numbers** for the two
//! latency-critical paths the mount layer owns, with no live node and no kernel driver, so they run
//! anywhere (incl. this macOS dev host and CI):
//!
//! 1. **Materialize path** (the always-shipped driverless fallback, the measurable path on a mac):
//!    create a *small real git repo*, store every file into a content-addressed store via the
//!    genuine `ce_rs::data` chunk engine (1 MiB chunks, sha256 CIDs, an `ce-object-v1` manifest whose
//!    own hash is the object CID — exactly the node's `/blobs` keying), then [`materialize`] the
//!    whole [`Drive`] into a real directory. We measure wall time + bytes transferred, then run
//!    `git status` and a tiny read/IO workload **natively** over the materialized tree.
//!
//! 2. **Lazy hydration (mount read path)**: drive the OS-independent [`Vfs`] engine to open + read
//!    exactly one file out of the repo and assert that **only that file's chunks are fetched** — the
//!    lazy-hydration property (opening a 2 GiB file and reading the header pulls one chunk, not
//!    2 GiB). We report the bytes the touched read moved vs. the whole-drive size.
//!
//! The store here is a faithful in-memory mirror of the real [`Store`](ce_drive_core::Store): it uses
//! the same `ce_rs::data` engine and the same object-CID = manifest-hash indirection, and it counts
//! every byte it serves so the numbers are real transfer measurements, not mocks of them.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ce_drive_core::content::FileContent;
use ce_drive_core::{Drive, DriveState};
use ce_drive_mount::store_iface::{BlockStore, PutResult};
use ce_drive_mount::vfs::{ROOT_INO, Vfs};
use ce_drive_mount::materialize;
use ce_rs::data::{Manifest, chunk_object, reassemble};

/// 1 MiB — the production chunk boundary (`ce_rs::data::DEFAULT_CHUNK_SIZE`). Using the real size
/// makes the dedup/delta numbers representative.
const CHUNK_SIZE: usize = 1024 * 1024;

/// An in-memory content-addressed store that mirrors the real [`Store`]: chunk via the genuine
/// `ce_rs::data` engine, key chunks + the manifest blob by their sha256 CIDs, and record the
/// manifest's own hash as the object CID. Every served byte is counted, so the perf smoke measures
/// *real* transfer, and per-chunk presence gives a true delta-upload count.
#[derive(Clone, Default)]
struct AccountingStore {
    /// chunk CID  -> chunk bytes.
    chunks: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// object CID -> serialized manifest blob.
    manifests: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Bytes served on read (chunk bytes + manifest blobs) — the lazy-hydration measurement.
    bytes_out: Arc<AtomicU64>,
    /// Bytes accepted on write (only genuinely-missing chunk bytes — the delta measurement).
    bytes_in: Arc<AtomicU64>,
    /// Number of distinct chunks actually stored (dedup denominator).
    chunk_fetches: Arc<AtomicUsize>,
}

impl AccountingStore {
    fn new() -> Self {
        AccountingStore::default()
    }

    fn bytes_out(&self) -> u64 {
        self.bytes_out.load(Ordering::SeqCst)
    }
    fn bytes_in(&self) -> u64 {
        self.bytes_in.load(Ordering::SeqCst)
    }
    fn chunk_fetches(&self) -> usize {
        self.chunk_fetches.load(Ordering::SeqCst)
    }

    /// Store bytes the way the real store does: chunk, insert each missing chunk, publish the
    /// manifest blob; the manifest's own CID is the object CID we record. Returns (object_cid,
    /// uploaded_chunks). Used for test setup (sync) and by `put_bytes` (async).
    fn store(&self, bytes: &[u8]) -> (String, usize) {
        let (manifest, chunks) = chunk_object(bytes, CHUNK_SIZE);
        let mut uploaded = 0usize;
        {
            let mut held = self.chunks.lock().expect("chunks lock");
            for (cid, data) in &chunks {
                if held.insert(cid.clone(), data.clone()).is_none() {
                    uploaded += 1;
                    self.bytes_in.fetch_add(data.len() as u64, Ordering::SeqCst);
                }
            }
        }
        let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
        let object_cid = ce_rs::data::cid(&manifest_bytes);
        self.manifests.lock().expect("manifests lock").insert(object_cid.clone(), manifest_bytes);
        (object_cid, uploaded)
    }

    fn load_manifest(&self, object_cid: &str) -> Result<Manifest> {
        let raw = self
            .manifests
            .lock()
            .expect("manifests lock")
            .get(object_cid)
            .cloned()
            .ok_or_else(|| anyhow!("no manifest for object {object_cid}"))?;
        self.bytes_out.fetch_add(raw.len() as u64, Ordering::SeqCst);
        serde_json::from_slice(&raw).context("deserialize manifest")
    }
}

impl BlockStore for AccountingStore {
    async fn get_object(&self, cid: &str) -> Result<Vec<u8>> {
        if cid.is_empty() {
            return Ok(Vec::new());
        }
        let manifest = self.load_manifest(cid)?;
        // Reassemble + verify every chunk against its CID (content-addressing IS the integrity proof),
        // counting the bytes each chunk fetch moves — the lazy-hydration transfer measurement.
        let chunks = self.chunks.clone();
        let bytes_out = self.bytes_out.clone();
        let chunk_fetches = self.chunk_fetches.clone();
        reassemble(&manifest, |chunk_cid| {
            let data = chunks
                .lock()
                .expect("chunks lock")
                .get(chunk_cid)
                .cloned()
                .ok_or_else(|| anyhow!("missing chunk {chunk_cid}"))?;
            bytes_out.fetch_add(data.len() as u64, Ordering::SeqCst);
            chunk_fetches.fetch_add(1, Ordering::SeqCst);
            Ok(data)
        })
    }

    async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
        let (object_cid, uploaded) = self.store(bytes);
        Ok(PutResult {
            content: FileContent::new(object_cid, bytes.len() as u64, mode, mtime_ms),
            uploaded_chunks: uploaded,
        })
    }
}

/// Create a small **real git repo** on disk and return its path (kept alive by the returned
/// `TempDir`). A handful of source files + a couple of dirs + an actual `git init` + commit, so
/// `git status` over the materialized copy is a genuine dev-tool workload.
fn make_small_git_repo() -> Result<(tempfile::TempDir, u64)> {
    let dir = tempfile::tempdir().context("tempdir")?;
    let root = dir.path();
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("docs"))?;

    // A representative small-file-storm: many small text files (the workload git/cargo are bad at on
    // naive distributed FS).
    let mut total = 0u64;
    let mut write = |rel: &str, content: &[u8]| -> Result<()> {
        let p = root.join(rel);
        std::fs::write(&p, content)?;
        total += content.len() as u64;
        Ok(())
    };
    write("README.md", b"# perf-smoke\n\nA small real repo for the CE Drive materialize perf smoke.\n")?;
    write("Cargo.toml", b"[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")?;
    write("src/main.rs", b"fn main() { println!(\"hello ce drive\"); }\n")?;
    write("src/lib.rs", b"pub fn add(a: i64, b: i64) -> i64 { a + b }\n")?;
    write("src/util.rs", b"pub fn id<T>(x: T) -> T { x }\n")?;
    write("docs/design.md", b"design notes\n".repeat(200).as_slice())?;
    // One bigger file (multi-chunk) so lazy hydration has something to *not* fetch.
    write("src/big.bin", vec![0xABu8; 3 * CHUNK_SIZE + 123].as_slice())?;

    // git init + add + commit so `git status` has a real index/HEAD to diff against.
    let git = |args: &[&str]| -> Result<()> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Leif Rydenfalk")
            .env("GIT_AUTHOR_EMAIL", "ledamecrydenfalk@gmail.com")
            .env("GIT_COMMITTER_NAME", "Leif Rydenfalk")
            .env("GIT_COMMITTER_EMAIL", "ledamecrydenfalk@gmail.com")
            .output()
            .with_context(|| format!("git {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "initial"])?;

    Ok((dir, total))
}

/// Ingest a real on-disk directory into a [`Drive`] backed by the accounting store (chunk every file
/// through the real engine). Returns the drive + total stored bytes.
fn ingest_dir_into_drive(store: &AccountingStore, src: &std::path::Path) -> Result<Drive> {
    let mut drive = Drive::init("perf-smoke", "replica-a");
    ingest_recursive(store, &mut drive, src, "/")?;
    Ok(drive)
}

fn ingest_recursive(
    store: &AccountingStore,
    drive: &mut Drive,
    disk_dir: &std::path::Path,
    drive_path: &str,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(disk_dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip the .git directory itself — we materialize the working tree, then `git init` fresh, or
        // (here) copy .git too so `git status` works. We DO include .git so status is meaningful.
        let meta = entry.metadata()?;
        let child = if drive_path == "/" { format!("/{name}") } else { format!("{drive_path}/{name}") };
        if meta.is_dir() {
            drive.mkdir(drive_path, &name)?;
            ingest_recursive(store, drive, &entry.path(), &child)?;
        } else if meta.is_file() {
            let bytes = std::fs::read(entry.path())?;
            let (object_cid, _uploaded) = store.store(&bytes);
            let fc = FileContent::new(object_cid, bytes.len() as u64, 0o644, 1);
            drive.add_file(drive_path, &name, fc)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn perf_smoke_materialize_then_git_status_and_io() -> Result<()> {
    let store = AccountingStore::new();

    // --- build the source repo ---
    let (repo, repo_bytes) = make_small_git_repo()?;

    // --- ingest into a CE Drive via the real chunk engine ---
    let t_ingest = Instant::now();
    let drive = ingest_dir_into_drive(&store, repo.path())?;
    let ingest_ms = t_ingest.elapsed().as_secs_f64() * 1e3;
    let bytes_uploaded = store.bytes_in();

    // --- materialize the whole drive into a fresh dir (the driverless fallback path) ---
    let out = tempfile::tempdir()?;
    let bytes_out_before = store.bytes_out();
    let t_mat = Instant::now();
    let report = materialize(&drive, &store, out.path()).await?;
    let materialize_ms = t_mat.elapsed().as_secs_f64() * 1e3;
    let bytes_hydrated = store.bytes_out() - bytes_out_before;

    assert!(report.files >= 7, "materialized the repo files (got {})", report.files);
    assert!(report.dirs >= 2, "materialized the repo dirs (got {})", report.dirs);

    // --- run `git status` natively over the materialized tree (the make-or-break dev-tool op) ---
    let t_status = Instant::now();
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(out.path())
        .output()
        .context("git status over materialized drive")?;
    let git_status_ms = t_status.elapsed().as_secs_f64() * 1e3;
    assert!(status.status.success(), "git status succeeded on the materialized drive");
    // A faithful materialization of a committed tree => a clean working tree.
    let dirty = String::from_utf8_lossy(&status.stdout);
    assert!(dirty.trim().is_empty(), "materialized tree is clean (git status empty): {dirty:?}");

    // --- a tiny build/IO workload: walk + read every file (small-file-storm), like a build's stat+read ---
    let t_io = Instant::now();
    let (files_read, io_bytes) = walk_read(out.path())?;
    let io_ms = t_io.elapsed().as_secs_f64() * 1e3;

    // --- report the real numbers (visible with `--nocapture`) ---
    eprintln!("\n=== CE Drive materialize perf smoke (real) ===");
    eprintln!("repo source bytes        : {repo_bytes}");
    eprintln!("ingest (chunk+store)     : {ingest_ms:.2} ms, uploaded {bytes_uploaded} B (delta)");
    eprintln!(
        "materialize whole drive  : {materialize_ms:.2} ms, {} files / {} dirs, {} B written, {} B hydrated from store",
        report.files, report.dirs, report.bytes, bytes_hydrated
    );
    eprintln!("git status (porcelain)   : {git_status_ms:.2} ms (clean)");
    eprintln!("walk+read all files (IO) : {io_ms:.2} ms, {files_read} files, {io_bytes} B");
    eprintln!("==============================================\n");

    Ok(())
}

#[tokio::test]
async fn perf_smoke_lazy_hydration_pulls_only_touched_file() -> Result<()> {
    let store = AccountingStore::new();
    let (repo, _repo_bytes) = make_small_git_repo()?;
    let drive = ingest_dir_into_drive(&store, repo.path())?;
    let whole_drive_bytes: u64 = total_drive_bytes(&drive, "/")?;

    // Drive the OS-independent VFS engine: list everything (manifest-only, no fetch), then open+read
    // exactly one small file and assert only its chunks moved.
    let state: DriveState = drive.state().clone();
    let vfs = Vfs::new(Drive::from_state(state), store.clone());

    // Listing is manifest-only: zero bytes hydrated.
    let _ = vfs.readdirplus(ROOT_INO).await?;
    assert_eq!(store.bytes_out(), 0, "listing/stat never downloads bytes");

    // Open + read ONLY src/main.rs (a tiny one-chunk file). Must not touch src/big.bin (3+ MiB).
    let src = vfs.lookup(ROOT_INO, "src").await?;
    let main_rs = vfs.lookup(src.ino, "main.rs").await?;
    let bytes_before = store.bytes_out();
    let chunks_before = store.chunk_fetches();
    let fh = vfs.open(main_rs.ino).await?;
    let data = vfs.read(fh, 0, 4096).await?;
    let touched_bytes = store.bytes_out() - bytes_before;
    let touched_chunks = store.chunk_fetches() - chunks_before;

    assert!(data.starts_with(b"fn main()"), "read back main.rs content");
    assert_eq!(touched_chunks, 1, "a tiny file is exactly one chunk");
    assert!(
        touched_bytes < whole_drive_bytes,
        "lazy hydration moved {touched_bytes} B, far less than the whole-drive {whole_drive_bytes} B"
    );

    // A second read hits the engine's working buffer — no extra network.
    let chunks_after_first = store.chunk_fetches();
    let _ = vfs.read(fh, 0, 16).await?;
    assert_eq!(store.chunk_fetches(), chunks_after_first, "cached read does no network");

    eprintln!("\n=== CE Drive lazy-hydration perf smoke (real) ===");
    eprintln!("whole-drive bytes        : {whole_drive_bytes}");
    eprintln!("touched read (main.rs)   : {touched_bytes} B over {touched_chunks} chunk(s)");
    eprintln!(
        "  -> hydration fetched {:.4}% of the drive (only the touched file)",
        100.0 * touched_bytes as f64 / whole_drive_bytes.max(1) as f64
    );
    eprintln!("=================================================\n");

    Ok(())
}

/// Walk a directory tree, reading every file (a build's stat+read small-file storm). Returns
/// (files_read, total_bytes).
fn walk_read(root: &std::path::Path) -> Result<(usize, u64)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                let data = std::fs::read(entry.path())?;
                files += 1;
                bytes += data.len() as u64;
            }
        }
    }
    Ok((files, bytes))
}

/// Sum the byte size of every live file in the drive (the lazy-hydration denominator).
fn total_drive_bytes(drive: &Drive, path: &str) -> Result<u64> {
    let mut total = 0u64;
    for e in drive.ls(path)? {
        let child = if path == "/" { format!("/{}", e.name) } else { format!("{path}/{}", e.name) };
        if e.is_dir {
            total += total_drive_bytes(drive, &child)?;
        } else if let Some(c) = &e.content {
            total += c.size;
        }
    }
    Ok(total)
}
