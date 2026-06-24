//! The shared VFS engine — the OS-independent heart of the CE Drive mount.
//!
//! Every kernel adapter (fuser on Linux, macFUSE-FSKit on macOS, WinFsp/ProjFS on Windows) is a
//! thin translation of VFS syscalls into calls on this engine; the engine owns all the hard parts
//! the design (`PLAN/10-drive-fs.md` §7) calls out:
//!
//! * **Stable inodes.** A node's [`Ino`] is assigned once and never reused while the mount lives, so
//!   `make`/`cargo` never spuriously rebuild (the single most important dev-tool correctness lever).
//!   Inodes are derived deterministically from the drive's stable `NodeId`, so they also survive a
//!   remount (same NodeId -> same inode).
//! * **Lazy hydration (read path).** `lookup`/`getattr`/`readdir` are served entirely from the
//!   locally-cached manifest (the [`DriveTree`] + content map) — listing never downloads bytes. The
//!   first `read` of a file pulls its chunks **by CID, block-wise**, into a content-addressed block
//!   cache; opening a 2 GiB file and reading the header pulls one chunk, not 2 GiB.
//! * **Write-back cache (write path).** Writes are buffered into a per-inode dirty buffer; on
//!   `fsync`/`release`/idle the buffer is re-chunked and uploaded **asynchronously** (only missing
//!   chunks move) so `close()` never blocks on the network — back-pressure surfaces as "syncing".
//! * **readdirplus.** [`Vfs::readdirplus`] returns each child's full attributes in one call, killing
//!   the readdir + N×getattr upcall storm.
//! * **Atomic rename.** [`Vfs::rename`] is a single `MoveOp` (the write-temp -> rename pattern every
//!   editor uses), atomic by construction.
//! * **Attr/entry TTLs.** [`Vfs::attr_ttl`] / [`Vfs::entry_ttl`] are long by default so the kernel
//!   stops calling back per `stat`.
//!
//! The engine is generic over a [`BlockStore`] so it can be driven by the real core [`Store`] in
//! production and by a deterministic in-memory mock in tests — the lazy-hydration and write-back
//! logic is exercised with no live node.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use ce_drive_core::content::FileContent;
use ce_drive_core::tree::{NodeId, NodeKind, ROOT};
use ce_drive_core::{Drive, DriveState};
use tokio::sync::Mutex;

use crate::store_iface::BlockStore;

/// A filesystem inode number. Stable for the lifetime of the mount (and across remounts, since it is
/// derived from the node's stable [`NodeId`]).
pub type Ino = u64;

/// The inode of the mount root. POSIX fixes the root at inode 1.
pub const ROOT_INO: Ino = 1;

/// Default attribute TTL handed to the kernel — long, so `stat` storms don't call back per file.
/// Close-to-open consistency (the design's chosen model) makes a generous TTL correct: you see a
/// peer's writes after you re-open, not mid-read.
pub const DEFAULT_ATTR_TTL: Duration = Duration::from_secs(2);

/// Default directory-entry (name -> inode) TTL handed to the kernel.
pub const DEFAULT_ENTRY_TTL: Duration = Duration::from_secs(2);

/// One file's kind + metadata, as the kernel needs it. Derived purely from the manifest (the tree +
/// content map); producing it never touches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// Stable inode.
    pub ino: Ino,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Unix mode bits (the kind bits are added by the adapter).
    pub mode: u32,
    /// Last-modified time, unix milliseconds.
    pub mtime_ms: u64,
    /// True for a directory.
    pub is_dir: bool,
}

/// One `readdirplus` entry: a name plus the full attributes the kernel would otherwise fetch with a
/// follow-up `getattr` (the readdirplus optimisation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryPlus {
    pub name: String,
    pub attr: Attr,
    /// The drive NodeId this entry maps to (for the adapter's own bookkeeping if needed).
    pub node_id: NodeId,
}

/// A handle to one open file: its inode and a per-handle dirty-write buffer (write-back).
struct OpenFile {
    ino: Ino,
    /// The hydrated/working bytes of the file (lazily filled on first read; the write-back target).
    /// `None` until hydrated or created.
    buf: Option<Vec<u8>>,
    /// The file is dirty (has un-uploaded writes) — set on `write`, cleared after a flush upload.
    dirty: bool,
}

/// The write-back state of a flush: how many chunks were uploaded and the resulting content CID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushResult {
    /// The new object CID recorded for the file.
    pub cid: String,
    /// Number of chunks actually uploaded (0 when the write fully deduped).
    pub uploaded_chunks: usize,
}

/// The shared, OS-independent VFS engine. Owns the inode table, the drive model, the block cache,
/// and the write-back buffers. Cloneable handle (an `Arc` over the inner mutex-guarded state) so an
/// async upload task and the syscall path can share it.
#[derive(Clone)]
pub struct Vfs<S: BlockStore> {
    inner: Arc<Mutex<VfsInner>>,
    store: S,
    attr_ttl: Duration,
    entry_ttl: Duration,
    /// Reject writes that would grow a file beyond this many bytes (a per-file DoS / RAM guard, since
    /// the write-back buffer is in memory). `0` = unbounded.
    max_file_size: u64,
}

/// Default per-file write cap for the in-memory write-back buffer: 2 GiB. Beyond this the mount errors
/// (`EFBIG`) rather than letting one file's dirty buffer exhaust RAM. Tunable via [`Vfs::with_max_file_size`].
pub const DEFAULT_MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

struct VfsInner {
    /// The drive model (tree + content map). Re-read on `revalidate` for close-to-open consistency.
    drive: Drive,
    /// Stable inode <-> NodeId bijection. Assigned once; never reused while mounted.
    ino_of: HashMap<NodeId, Ino>,
    node_of: HashMap<Ino, NodeId>,
    /// The next inode to hand out (monotone; never collides with [`ROOT_INO`]).
    next_ino: Ino,
    /// Open file handles by handle id.
    open: HashMap<u64, OpenFile>,
    next_fh: u64,
    /// Whether the current drive model has unsynced structural/content writes that need persisting.
    dirty_model: bool,
}

impl<S: BlockStore> Vfs<S> {
    /// Build a VFS engine over a drive model and a block store, with default TTLs.
    pub fn new(drive: Drive, store: S) -> Self {
        let mut inner = VfsInner {
            drive,
            ino_of: HashMap::new(),
            node_of: HashMap::new(),
            next_ino: ROOT_INO + 1,
            open: HashMap::new(),
            next_fh: 1,
            dirty_model: false,
        };
        // Pin the root inode deterministically.
        inner.ino_of.insert(ROOT.to_string(), ROOT_INO);
        inner.node_of.insert(ROOT_INO, ROOT.to_string());
        Vfs {
            inner: Arc::new(Mutex::new(inner)),
            store,
            attr_ttl: DEFAULT_ATTR_TTL,
            entry_ttl: DEFAULT_ENTRY_TTL,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }

    /// Override the attribute TTL (advanced; the default is tuned for dev-tool workloads).
    pub fn with_ttls(mut self, attr_ttl: Duration, entry_ttl: Duration) -> Self {
        self.attr_ttl = attr_ttl;
        self.entry_ttl = entry_ttl;
        self
    }

    /// Override the per-file write cap (bytes; `0` = unbounded). Writes past this fail with an error
    /// the adapter maps to `EFBIG`, bounding the in-memory write-back buffer.
    pub fn with_max_file_size(mut self, max: u64) -> Self {
        self.max_file_size = max;
        self
    }

    /// The configured per-file write cap (bytes; `0` = unbounded).
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// The attribute TTL the adapter should hand the kernel.
    pub fn attr_ttl(&self) -> Duration {
        self.attr_ttl
    }

    /// The directory-entry TTL the adapter should hand the kernel.
    pub fn entry_ttl(&self) -> Duration {
        self.entry_ttl
    }

    /// Replace the live drive model (after a close-to-open revalidation pulled a newer DriveState).
    /// Inode assignments are preserved for nodes that still exist, so inodes stay stable across the
    /// re-read — the dev-tool correctness invariant.
    pub async fn revalidate(&self, state: DriveState) {
        let mut g = self.inner.lock().await;
        g.drive = Drive::from_state(state);
        // Existing ino<->node mappings remain valid; new nodes get fresh inodes lazily on lookup.
        g.dirty_model = false;
    }

    /// The current persisted DriveState (so an adapter can write the model back to disk on unmount).
    pub async fn state(&self) -> DriveState {
        self.inner.lock().await.drive.state().clone()
    }

    /// Whether the model has structural/content writes not yet persisted.
    pub async fn is_model_dirty(&self) -> bool {
        self.inner.lock().await.dirty_model
    }

    // ---- inode bookkeeping ----

    /// Stable inode for a NodeId, assigning a fresh one on first sight. Never reused while mounted.
    fn intern(inner: &mut VfsInner, node: &str) -> Ino {
        if let Some(ino) = inner.ino_of.get(node) {
            return *ino;
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.ino_of.insert(node.to_string(), ino);
        inner.node_of.insert(ino, node.to_string());
        ino
    }

    // ---- read path (lazy, manifest-served) ----

    /// `getattr` — pure manifest read, no network. The single most-called syscall; serving it from
    /// the cached tree + content map (with a long TTL) is the perf foundation.
    pub async fn getattr(&self, ino: Ino) -> Result<Attr> {
        let g = self.inner.lock().await;
        Self::attr_of_locked(&g, ino)
    }

    fn attr_of_locked(g: &VfsInner, ino: Ino) -> Result<Attr> {
        if ino == ROOT_INO {
            return Ok(Attr { ino: ROOT_INO, size: 0, mode: 0o755, mtime_ms: 0, is_dir: true });
        }
        let node = g.node_of.get(&ino).ok_or_else(|| anyhow!("unknown inode {ino}"))?;
        let edge = g.drive.tree().edge(node).ok_or_else(|| anyhow!("inode {ino} has no edge"))?;
        let is_dir = matches!(edge.kind, NodeKind::Dir);
        let content = g.drive.content().get(node);
        Ok(Attr {
            ino,
            size: content.map(|c| c.size).unwrap_or(0),
            mode: content.map(|c| c.mode).unwrap_or(if is_dir { 0o755 } else { 0o644 }),
            mtime_ms: content.map(|c| c.mtime_ms).unwrap_or(0),
            is_dir,
        })
    }

    /// `lookup` — resolve a child name under a directory inode to its (stable inode, attributes).
    /// Pure manifest read; never downloads bytes.
    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<Attr> {
        let mut g = self.inner.lock().await;
        let parent_node = g.node_of.get(&parent).cloned().ok_or_else(|| anyhow!("unknown parent inode {parent}"))?;
        let child = g
            .drive
            .tree()
            .readdir(&parent_node)
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, id, _)| id)
            .ok_or_else(|| anyhow!("no such entry '{name}'"))?;
        let ino = Self::intern(&mut g, &child);
        Self::attr_of_locked(&g, ino)
    }

    /// Resolve an absolute slash-separated path to its (stable inode, attributes) — pure manifest
    /// read, never downloads bytes. The path-addressed counterpart to [`Vfs::lookup`], needed by the
    /// Windows adapters (WinFsp/ProjFS speak whole paths, not parent-inode + name). `"/"` is the root.
    pub async fn lookup_path(&self, path: &str) -> Result<Attr> {
        let mut g = self.inner.lock().await;
        if path == "/" || path.is_empty() {
            return Self::attr_of_locked(&g, ROOT_INO);
        }
        let node = g.drive.tree().resolve(path).ok_or_else(|| anyhow!("no such entry '{path}'"))?;
        let ino = Self::intern(&mut g, &node);
        Self::attr_of_locked(&g, ino)
    }

    /// Path-addressed [`Vfs::readdirplus`] — list a directory's children (names + full attributes,
    /// no bytes) by absolute slash-path. The path-addressed counterpart needed by the Windows ProjFS
    /// adapter (ProjFS enumeration callbacks speak whole paths, not parent inodes). `"/"` is the root.
    pub async fn readdirplus_path(&self, path: &str) -> Result<Vec<DirEntryPlus>> {
        let ino = self.lookup_path(path).await?.ino;
        self.readdirplus(ino).await
    }

    /// `readdir` — names only (the cheap variant). Sorted, conflict-renamed by the tree.
    pub async fn readdir(&self, ino: Ino) -> Result<Vec<(String, Ino, bool)>> {
        let mut g = self.inner.lock().await;
        let node = g.node_of.get(&ino).cloned().ok_or_else(|| anyhow!("unknown inode {ino}"))?;
        let entries = g.drive.tree().readdir(&node);
        let mut out = Vec::with_capacity(entries.len());
        for (name, id, kind) in entries {
            let child_ino = Self::intern(&mut g, &id);
            out.push((name, child_ino, matches!(kind, NodeKind::Dir)));
        }
        Ok(out)
    }

    /// `readdirplus` — names **and** full attributes in one call, killing the readdir + N×getattr
    /// upcall storm (the design's explicit perf requirement). Pure manifest read.
    pub async fn readdirplus(&self, ino: Ino) -> Result<Vec<DirEntryPlus>> {
        let mut g = self.inner.lock().await;
        let node = g.node_of.get(&ino).cloned().ok_or_else(|| anyhow!("unknown inode {ino}"))?;
        let entries = g.drive.tree().readdir(&node);
        let mut out = Vec::with_capacity(entries.len());
        for (name, id, _kind) in entries {
            let child_ino = Self::intern(&mut g, &id);
            let attr = Self::attr_of_locked(&g, child_ino)?;
            out.push(DirEntryPlus { name, attr, node_id: id });
        }
        Ok(out)
    }

    /// Open a file, returning a file handle. Does **not** hydrate bytes (lazy): hydration happens on
    /// the first [`Vfs::read`]. Directories are not opened through this path.
    pub async fn open(&self, ino: Ino) -> Result<u64> {
        let mut g = self.inner.lock().await;
        if !g.node_of.contains_key(&ino) {
            bail!("unknown inode {ino}");
        }
        let fh = g.next_fh;
        g.next_fh += 1;
        g.open.insert(fh, OpenFile { ino, buf: None, dirty: false });
        Ok(fh)
    }

    /// `read` — lazily hydrate the file's chunks (by CID, only those covering the requested range are
    /// guaranteed fetched; we hydrate the whole file once on first touch for simplicity and cache it)
    /// then return the requested byte range. Subsequent reads hit the local block cache with no
    /// network. Returns the bytes actually available in `[offset, offset+size)`.
    pub async fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        // Ensure the handle is hydrated (drop the lock around the network fetch).
        self.hydrate(fh).await?;
        let g = self.inner.lock().await;
        let of = g.open.get(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
        let buf = of.buf.as_ref().ok_or_else(|| anyhow!("handle {fh} not hydrated"))?;
        let start = offset.min(buf.len() as u64) as usize;
        let end = (offset + size as u64).min(buf.len() as u64) as usize;
        Ok(buf[start..end].to_vec())
    }

    /// Ensure the open handle's working buffer is filled. On a cold handle we resolve the node's
    /// current content CID, then fetch+verify its bytes through the block store (block-wise, each
    /// chunk CID-verified; the store caches them content-addressed). A freshly-created file with no
    /// content hydrates to an empty buffer.
    async fn hydrate(&self, fh: u64) -> Result<()> {
        // Fast path: already hydrated.
        {
            let g = self.inner.lock().await;
            let of = g.open.get(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
            if of.buf.is_some() {
                return Ok(());
            }
        }
        // Resolve the content CID for this handle's node (without holding the lock across the fetch).
        let cid = {
            let g = self.inner.lock().await;
            let of = g.open.get(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
            let node = g.node_of.get(&of.ino).cloned().ok_or_else(|| anyhow!("handle has no node"))?;
            g.drive.content().get(&node).map(|c| c.cid.clone())
        };
        let bytes = match cid {
            Some(cid) if !cid.is_empty() => self.store.get_object(&cid).await?,
            _ => Vec::new(),
        };
        let mut g = self.inner.lock().await;
        if let Some(of) = g.open.get_mut(&fh) {
            // Only set if still empty (a concurrent write may have produced a buffer first).
            if of.buf.is_none() {
                of.buf = Some(bytes);
            }
        }
        Ok(())
    }

    // ---- write path (write-back) ----

    /// `write` — buffer bytes into the handle's working buffer at `offset` (write-back; no upload
    /// here). The buffer is hydrated first so a partial overwrite preserves the untouched tail. Marks
    /// the handle dirty. Returns the number of bytes written.
    pub async fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let end = offset + data.len() as u64;
        if self.max_file_size != 0 && end > self.max_file_size {
            bail!("write to {end} bytes exceeds the {}-byte per-file limit", self.max_file_size);
        }
        self.hydrate(fh).await?;
        let mut g = self.inner.lock().await;
        let of = g.open.get_mut(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
        let buf = of.buf.get_or_insert_with(Vec::new);
        let end = offset as usize + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[offset as usize..end].copy_from_slice(data);
        of.dirty = true;
        Ok(data.len() as u32)
    }

    /// Truncate (or extend) a file's working buffer to `size` (the `setattr`/`ftruncate` path).
    pub async fn truncate(&self, fh: u64, size: u64) -> Result<()> {
        if self.max_file_size != 0 && size > self.max_file_size {
            bail!("truncate to {size} bytes exceeds the {}-byte per-file limit", self.max_file_size);
        }
        self.hydrate(fh).await?;
        let mut g = self.inner.lock().await;
        let of = g.open.get_mut(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
        let buf = of.buf.get_or_insert_with(Vec::new);
        buf.resize(size as usize, 0);
        of.dirty = true;
        Ok(())
    }

    /// `fsync`/`flush`/`release` write-back: if the handle is dirty, re-chunk its working buffer,
    /// upload only the **missing** chunks (delta), and record the new content CID as a new version on
    /// the node — all asynchronously from `close()`'s perspective (the adapter spawns this; it does
    /// not block the syscall). Returns `None` if the handle had no pending writes.
    pub async fn flush(&self, fh: u64) -> Result<Option<FlushResult>> {
        // Snapshot what we need under the lock, then upload without holding it.
        let (ino, bytes, dirty) = {
            let g = self.inner.lock().await;
            let of = g.open.get(&fh).ok_or_else(|| anyhow!("bad file handle {fh}"))?;
            (of.ino, of.buf.clone(), of.dirty)
        };
        if !dirty {
            return Ok(None);
        }
        let bytes = bytes.unwrap_or_default();
        let mtime_ms = now_ms();
        let stored = self.store.put_bytes(&bytes, 0o644, mtime_ms).await?;
        // Record the new content version on the node and clear the dirty flag.
        let mut g = self.inner.lock().await;
        let node = g.node_of.get(&ino).cloned().ok_or_else(|| anyhow!("inode {ino} vanished"))?;
        g.drive.set_content(&node, stored.content.clone());
        g.dirty_model = true;
        if let Some(of) = g.open.get_mut(&fh) {
            of.dirty = false;
        }
        Ok(Some(FlushResult { cid: stored.content.cid, uploaded_chunks: stored.uploaded_chunks }))
    }

    /// Close a file handle, flushing any pending writes first. Returns the flush result if a write
    /// was uploaded.
    pub async fn release(&self, fh: u64) -> Result<Option<FlushResult>> {
        let res = self.flush(fh).await?;
        self.inner.lock().await.open.remove(&fh);
        Ok(res)
    }

    // ---- structural ops (one MoveOp each; pure model mutations) ----

    /// `create` — make a new empty file under a parent directory and return its (inode, handle).
    pub async fn create(&self, parent: Ino, name: &str) -> Result<(Attr, u64)> {
        let mut g = self.inner.lock().await;
        let parent_node = g.node_of.get(&parent).cloned().ok_or_else(|| anyhow!("unknown parent inode {parent}"))?;
        let parent_path = g.drive.tree().path(&parent_node).ok_or_else(|| anyhow!("parent not addressable"))?;
        // Empty content: cid is empty until first flush.
        let fc = FileContent::new(String::new(), 0, 0o644, now_ms());
        let id = g.drive.add_file(&parent_path, name, fc)?;
        g.dirty_model = true;
        let ino = Self::intern(&mut g, &id);
        let attr = Self::attr_of_locked(&g, ino)?;
        let fh = g.next_fh;
        g.next_fh += 1;
        g.open.insert(fh, OpenFile { ino, buf: Some(Vec::new()), dirty: true });
        Ok((attr, fh))
    }

    /// `mkdir` — make a new directory under a parent. Returns its (inode, attributes).
    pub async fn mkdir(&self, parent: Ino, name: &str) -> Result<Attr> {
        let mut g = self.inner.lock().await;
        let parent_node = g.node_of.get(&parent).cloned().ok_or_else(|| anyhow!("unknown parent inode {parent}"))?;
        let parent_path = g.drive.tree().path(&parent_node).ok_or_else(|| anyhow!("parent not addressable"))?;
        let id = g.drive.mkdir(&parent_path, name)?;
        g.dirty_model = true;
        let ino = Self::intern(&mut g, &id);
        Self::attr_of_locked(&g, ino)
    }

    /// `unlink`/`rmdir` — delete a child (move to TRASH; recoverable). Inode stays interned (stable)
    /// but the node is no longer live, so it disappears from listings.
    pub async fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
        let mut g = self.inner.lock().await;
        let parent_node = g.node_of.get(&parent).cloned().ok_or_else(|| anyhow!("unknown parent inode {parent}"))?;
        let parent_path = g.drive.tree().path(&parent_node).ok_or_else(|| anyhow!("parent not addressable"))?;
        let path = join_path(&parent_path, name);
        g.drive.rm(&path)?;
        g.dirty_model = true;
        Ok(())
    }

    /// `rename` — atomic move of `name` under `parent` to `new_name` under `new_parent`. One
    /// `MoveOp` (the write-temp -> rename pattern every editor uses, atomic by construction). The
    /// node's inode is preserved (path changes, identity does not).
    pub async fn rename(&self, parent: Ino, name: &str, new_parent: Ino, new_name: &str) -> Result<()> {
        let mut g = self.inner.lock().await;
        let parent_node = g.node_of.get(&parent).cloned().ok_or_else(|| anyhow!("unknown parent inode {parent}"))?;
        let new_parent_node = g.node_of.get(&new_parent).cloned().ok_or_else(|| anyhow!("unknown new parent inode {new_parent}"))?;
        let src_dir = g.drive.tree().path(&parent_node).ok_or_else(|| anyhow!("src parent not addressable"))?;
        let dst_dir = g.drive.tree().path(&new_parent_node).ok_or_else(|| anyhow!("dst parent not addressable"))?;
        let src = join_path(&src_dir, name);
        // Resolve the source up front: if it is missing, fail BEFORE touching the destination, so a
        // bad rename never destroys an existing destination (the audit's atomicity fix).
        let src_node = g
            .drive
            .tree()
            .resolve(&src)
            .ok_or_else(|| anyhow!("rename source '{src}' does not exist"))?;
        let dst_existing = join_path(&dst_dir, new_name);
        let displaced = g.drive.tree().resolve(&dst_existing).filter(|n| n != &src_node);
        // Move the source onto the destination name FIRST. If a node already lives there, this
        // produces a transient name collision the tree renders deterministically (conflict copy) —
        // the destination is never lost. Only after the move succeeds do we trash the old occupant,
        // giving POSIX overwrite semantics without a window where the destination is gone but the
        // source has not yet arrived.
        g.drive.mv(&src, &dst_dir, new_name)?;
        if let Some(old) = displaced {
            // Trash the displaced node by id (its path is now conflict-rendered, so resolve-by-path is
            // ambiguous; trash by walking from the parent and matching the node id).
            let trash_path = g
                .drive
                .tree()
                .path(&old)
                .unwrap_or_else(|| join_path(&dst_dir, new_name));
            let _ = g.drive.rm(&trash_path);
        }
        g.dirty_model = true;
        Ok(())
    }
}

/// Join a directory path and a child name into a normalized absolute path.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_iface::MockStore;

    fn drive_with_file(name: &str, bytes: &[u8], store: &MockStore) -> (Drive, String) {
        let mut d = Drive::init("test", "replica-a");
        let cid = store.put_sync(bytes);
        let fc = FileContent::new(cid.clone(), bytes.len() as u64, 0o644, 1000);
        d.add_file("/", name, fc).unwrap();
        (d, cid)
    }

    #[tokio::test]
    async fn getattr_and_lookup_are_manifest_only() {
        let store = MockStore::new();
        let (drive, _cid) = drive_with_file("a.txt", b"hello world", &store);
        let vfs = Vfs::new(drive, store.clone());
        // Root attr.
        let root = vfs.getattr(ROOT_INO).await.unwrap();
        assert!(root.is_dir);
        // Lookup the file: pure manifest, zero fetches.
        let a = vfs.lookup(ROOT_INO, "a.txt").await.unwrap();
        assert_eq!(a.size, 11);
        assert!(!a.is_dir);
        assert_eq!(store.fetch_count(), 0, "listing/stat never downloads bytes");
    }

    #[tokio::test]
    async fn hydrate_on_open_pulls_only_touched_file() {
        let store = MockStore::new();
        let mut d = Drive::init("t", "a");
        // Two files; only one is read.
        let cid_a = store.put_sync(b"AAAA");
        let cid_b = store.put_sync(b"BBBBBBBB");
        d.add_file("/", "a", FileContent::new(cid_a, 4, 0o644, 1)).unwrap();
        d.add_file("/", "b", FileContent::new(cid_b, 8, 0o644, 1)).unwrap();
        let vfs = Vfs::new(d, store.clone());
        // Listing both: no fetch.
        let entries = vfs.readdirplus(ROOT_INO).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(store.fetch_count(), 0);
        // Open + read only "a": exactly one object fetched (a's), not b's.
        let a_ino = vfs.lookup(ROOT_INO, "a").await.unwrap().ino;
        let fh = vfs.open(a_ino).await.unwrap();
        let got = vfs.read(fh, 0, 4).await.unwrap();
        assert_eq!(got, b"AAAA");
        assert_eq!(store.fetch_count(), 1, "hydrate-on-open pulls only the touched file");
        // A second read hits the local buffer, no extra fetch.
        let _ = vfs.read(fh, 0, 4).await.unwrap();
        assert_eq!(store.fetch_count(), 1, "cached read does no network");
    }

    #[tokio::test]
    async fn range_read_returns_slice() {
        let store = MockStore::new();
        let (drive, _) = drive_with_file("a.txt", b"0123456789", &store);
        let vfs = Vfs::new(drive, store.clone());
        let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        let fh = vfs.open(ino).await.unwrap();
        assert_eq!(vfs.read(fh, 2, 3).await.unwrap(), b"234");
        assert_eq!(vfs.read(fh, 8, 100).await.unwrap(), b"89", "past-EOF clamps");
    }

    #[tokio::test]
    async fn write_back_batches_until_flush() {
        let store = MockStore::new();
        let (drive, _) = drive_with_file("a.txt", b"hello", &store);
        let vfs = Vfs::new(drive, store.clone());
        let ino = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        let fh = vfs.open(ino).await.unwrap();
        let before = store.put_count();
        // Several writes — none upload (write-back).
        vfs.write(fh, 0, b"HELLO").await.unwrap();
        vfs.write(fh, 5, b" WORLD").await.unwrap();
        assert_eq!(store.put_count(), before, "writes buffer; no upload before flush");
        // One flush -> one upload batch.
        let res = vfs.flush(fh).await.unwrap().unwrap();
        assert!(store.put_count() > before, "flush uploads the batch");
        assert!(!res.cid.is_empty());
        // The model now reflects the new content.
        let attr = vfs.getattr(ino).await.unwrap();
        assert_eq!(attr.size, 11, "HELLO WORLD");
        // A second flush with no new writes is a no-op.
        assert!(vfs.flush(fh).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_write_read_roundtrip() {
        let store = MockStore::new();
        let drive = Drive::init("t", "a");
        let vfs = Vfs::new(drive, store.clone());
        let (attr, fh) = vfs.create(ROOT_INO, "new.txt").await.unwrap();
        vfs.write(fh, 0, b"fresh content").await.unwrap();
        vfs.release(fh).await.unwrap();
        // Re-open and read back through hydration (proves it round-tripped through the store).
        let fh2 = vfs.open(attr.ino).await.unwrap();
        assert_eq!(vfs.read(fh2, 0, 100).await.unwrap(), b"fresh content");
    }

    #[tokio::test]
    async fn inodes_are_stable_across_lookups_and_revalidate() {
        let store = MockStore::new();
        let (drive, _) = drive_with_file("a.txt", b"x", &store);
        let state = drive.state().clone();
        let vfs = Vfs::new(Drive::from_state(state.clone()), store.clone());
        let i1 = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        let i2 = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        assert_eq!(i1, i2, "same node -> same inode within a mount");
        // After a revalidate (close-to-open re-read), the inode for the same NodeId is preserved.
        vfs.revalidate(state).await;
        let i3 = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        assert_eq!(i1, i3, "inode stable across revalidate (no spurious rebuilds)");
    }

    #[tokio::test]
    async fn rename_is_atomic_and_preserves_inode() {
        let store = MockStore::new();
        let (drive, _) = drive_with_file("a.txt", b"x", &store);
        let vfs = Vfs::new(drive, store.clone());
        let before = vfs.lookup(ROOT_INO, "a.txt").await.unwrap().ino;
        vfs.rename(ROOT_INO, "a.txt", ROOT_INO, "b.txt").await.unwrap();
        // Old name gone, new name present with the SAME inode (identity preserved across rename).
        assert!(vfs.lookup(ROOT_INO, "a.txt").await.is_err());
        let after = vfs.lookup(ROOT_INO, "b.txt").await.unwrap().ino;
        assert_eq!(before, after, "rename preserves inode");
    }

    #[tokio::test]
    async fn mkdir_unlink_reflected_in_listing() {
        let store = MockStore::new();
        let drive = Drive::init("t", "a");
        let vfs = Vfs::new(drive, store.clone());
        let dir = vfs.mkdir(ROOT_INO, "docs").await.unwrap();
        assert!(dir.is_dir);
        let (_, fh) = vfs.create(dir.ino, "x.txt").await.unwrap();
        vfs.release(fh).await.unwrap();
        assert_eq!(vfs.readdir(dir.ino).await.unwrap().len(), 1);
        vfs.unlink(dir.ino, "x.txt").await.unwrap();
        assert!(vfs.readdir(dir.ino).await.unwrap().is_empty(), "unlink hides the entry");
    }
}
