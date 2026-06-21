//! The Linux `fuser` adapter (the primary build/validation target).
//!
//! `fuser` speaks the libfuse ABI without linking libfuse, so this is the reference kernel adapter.
//! It is **feature-gated behind `fuse`** (default off) so the workspace builds on machines without
//! libfuse (e.g. a developer mac, this build host): `cargo build` is green everywhere; `cargo build
//! --features fuse` builds the real mount where libfuse headers are present.
//!
//! The adapter is intentionally thin: every callback translates a kernel request into a call on the
//! OS-independent [`Vfs`] engine (`crate::vfs`), which owns lazy hydration, the write-back cache,
//! stable inodes, readdirplus, and atomic rename. The adapter only does (1) the libfuse type
//! plumbing (`FileAttr`, reply objects, errno mapping) and (2) blocking on the engine's async API
//! via a Tokio handle. Mount options follow the design (§7.1): `writeback_cache` + long attr/entry
//! TTLs.

#![cfg(feature = "fuse")]

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ce_drive_core::DriveState;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tokio::runtime::Handle;

use crate::store_iface::BlockStore;
use crate::vfs::{Attr, Ino, ROOT_INO, Vfs};

/// A libfuse filesystem backed by the shared [`Vfs`] engine. Generic over the block store so the
/// same adapter mounts a real CE-node-backed drive or (in adapter smoke tests) a mock-backed one.
pub struct CeDriveFs<S: BlockStore> {
    vfs: Vfs<S>,
    /// A Tokio runtime handle: libfuse callbacks are synchronous, so we `block_on` the engine's
    /// async API. The caller mounts from within a Tokio runtime and passes its handle.
    rt: Handle,
    /// Generation counter for inodes (libfuse wants a generation; we use a constant 1 because inodes
    /// are never reused while mounted — the stability invariant).
    generation: u64,
}

impl<S: BlockStore> CeDriveFs<S> {
    /// Build the filesystem from an engine and the runtime handle to drive its async API.
    pub fn new(vfs: Vfs<S>, rt: Handle) -> Self {
        CeDriveFs { vfs, rt, generation: 1 }
    }

    /// Block on an engine future from a synchronous libfuse callback.
    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(fut)
    }
}

/// Mount a CE Drive at `mountpoint` using the fuser adapter. Blocks until unmounted. The mount is
/// opened `writeback_cache` (coalesces small writes) with the engine's long TTLs in effect. On
/// unmount the (possibly dirty) drive model is handed back via `on_unmount` so the caller persists
/// the new DriveState.
pub fn mount<S, F>(vfs: Vfs<S>, mountpoint: &Path, fs_name: &str, on_unmount: F) -> Result<()>
where
    S: BlockStore,
    F: FnOnce(DriveState),
{
    let rt = Handle::try_current().context("ce-drive mount must run inside a Tokio runtime")?;
    let state_engine = vfs.clone();
    let fs = CeDriveFs::new(vfs, rt.clone());
    let options = vec![
        MountOption::FSName(fs_name.to_string()),
        MountOption::DefaultPermissions,
        MountOption::Subtype("cedrive".to_string()),
        // Coalesce small writes (every editor's fsync storm) — the write-back lever.
        MountOption::CUSTOM("writeback_cache".to_string()),
    ];
    // Blocks until unmounted.
    fuser::mount2(fs, mountpoint, &options).context("fuser mount")?;
    // Hand the final model back for persistence (close-to-open: flush all on unmount).
    let final_state = rt.block_on(async move { state_engine.state().await });
    on_unmount(final_state);
    Ok(())
}

const TTL_FALLBACK: Duration = Duration::from_secs(1);

/// Map an engine [`Attr`] into a libfuse [`FileAttr`].
fn to_file_attr(a: &Attr) -> FileAttr {
    let mtime = UNIX_EPOCH + Duration::from_millis(a.mtime_ms);
    let kind = if a.is_dir { FileType::Directory } else { FileType::RegularFile };
    let perm = (a.mode & 0o7777) as u16;
    FileAttr {
        ino: a.ino,
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink: if a.is_dir { 2 } else { 1 },
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// errno for an engine error (the engine returns `anyhow`; the adapter coarsely maps to ENOENT for
/// "not found"-shaped errors and EIO otherwise).
fn errno(e: &anyhow::Error) -> i32 {
    let msg = e.to_string();
    if msg.contains("no such") || msg.contains("unknown inode") || msg.contains("no such entry") {
        libc::ENOENT
    } else {
        libc::EIO
    }
}

impl<S: BlockStore> Filesystem for CeDriveFs<S> {
    fn lookup(&mut self, _req: &Request<'_>, parent: Ino, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_string_lossy().to_string();
        match self.block(self.vfs.lookup(parent, &name)) {
            Ok(a) => reply.entry(&self.vfs.entry_ttl(), &to_file_attr(&a), self.generation),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: Ino, _fh: Option<u64>, reply: ReplyAttr) {
        match self.block(self.vfs.getattr(ino)) {
            Ok(a) => reply.attr(&self.vfs.attr_ttl(), &to_file_attr(&a)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: Ino, _flags: i32, reply: ReplyOpen) {
        match self.block(self.vfs.open(ino)) {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: Ino,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        match self.block(self.vfs.read(fh, offset.max(0) as u64, size)) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: Ino,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.block(self.vfs.write(fh, offset.max(0) as u64, data)) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: Ino, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        match self.block(self.vfs.flush(fh)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: Ino, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match self.block(self.vfs.flush(fh)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: Ino,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.block(self.vfs.release(fh)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: Ino,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name.to_string_lossy().to_string();
        match self.block(self.vfs.create(parent, &name)) {
            Ok((a, fh)) => reply.created(&self.vfs.entry_ttl(), &to_file_attr(&a), self.generation, fh, 0),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: Ino,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name.to_string_lossy().to_string();
        match self.block(self.vfs.mkdir(parent, &name)) {
            Ok(a) => reply.entry(&self.vfs.entry_ttl(), &to_file_attr(&a), self.generation),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: Ino, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_string_lossy().to_string();
        match self.block(self.vfs.unlink(parent, &name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: Ino, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_string_lossy().to_string();
        match self.block(self.vfs.unlink(parent, &name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: Ino,
        name: &OsStr,
        newparent: Ino,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let name = name.to_string_lossy().to_string();
        let newname = newname.to_string_lossy().to_string();
        match self.block(self.vfs.rename(parent, &name, newparent, &newname)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: Ino, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let entries = match self.block(self.vfs.readdir(ino)) {
            Ok(e) => e,
            Err(e) => {
                reply.error(errno(&e));
                return;
            }
        };
        // Synthesize "." and ".." then the children, honoring the resume offset.
        let mut all: Vec<(Ino, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_or_self(ino), FileType::Directory, "..".to_string()),
        ];
        for (name, child_ino, is_dir) in entries {
            all.push((child_ino, if is_dir { FileType::Directory } else { FileType::RegularFile }, name));
        }
        for (i, (child_ino, kind, name)) in all.into_iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(child_ino, (i + 1) as i64, kind, &name) {
                break; // buffer full
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: Ino,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let entries = match self.block(self.vfs.readdirplus(ino)) {
            Ok(e) => e,
            Err(e) => {
                reply.error(errno(&e));
                return;
            }
        };
        // "." and ".." with self/parent attrs.
        let self_attr = self.block(self.vfs.getattr(ino)).ok();
        let mut idx = 0i64;
        if (offset as usize) <= 0 {
            if let Some(a) = &self_attr {
                idx += 1;
                if reply.add(ino, idx, ".", &self.vfs.entry_ttl(), &to_file_attr(a), self.generation) {
                    reply.ok();
                    return;
                }
            }
        }
        for (i, e) in entries.into_iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(
                e.attr.ino,
                (i + 2) as i64,
                &e.name,
                &self.vfs.entry_ttl(),
                &to_file_attr(&e.attr),
                self.generation,
            ) {
                break;
            }
        }
        reply.ok();
    }
}

/// The parent inode for `..`. The engine does not expose reverse edges cheaply through this thin
/// adapter, so we approximate with the root for now (correct for top-level; a refinement tracks the
/// real parent inode). The root's parent is itself.
fn parent_or_self(ino: Ino) -> Ino {
    if ino == ROOT_INO { ROOT_INO } else { ROOT_INO }
}

/// The current unix time (helper kept for callbacks that need a fallback timestamp).
#[allow(dead_code)]
fn now() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(TTL_FALLBACK)
}
