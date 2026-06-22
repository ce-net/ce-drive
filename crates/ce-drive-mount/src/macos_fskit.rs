//! macOS adapter — macFUSE **FSKit** backend (with a kext fast-mode opt-in).
//!
//! The design (`PLAN/10-drive-fs.md` §7.1) chooses the FSKit user-space backend
//! (`-o backend=fskit`, mounting under `/Volumes/CEDrive/<drive>`) so there is no kext and no
//! Recovery dance, with the kext backend as an opt-in "fast mode."
//!
//! On macOS, `macFUSE` exposes the libfuse ABI, and the `fuser` crate links against it through
//! pkg-config (`osxfuse`/`macfuse`). So this adapter is — by design — the *same* thin translation
//! of FUSE callbacks onto the shared OS-independent [`Vfs`](crate::vfs) engine that the Linux
//! adapter is; it differs only in the macOS-specific **mount options** (the FSKit/kext backend
//! selector, `/Volumes` mountpoint convention, and `volname`) and in the name normalization the
//! design's §7.4 checklist requires on a case-insensitive, NFD-normalizing platform.
//!
//! ## Why a feature, not just `cfg(target_os = "macos")`
//! Linking macFUSE requires its headers/pkg-config to be present. Most macs (incl. CI without
//! macFUSE, and this dev host) do not have it installed. Gating behind the **`macos-fskit`** Cargo
//! feature (which pulls in `fuser` + `libc`) keeps the *default* `cargo build` green everywhere;
//! you opt in with `cargo build --features macos-fskit` on a mac with macFUSE 5.x installed. Without
//! the feature, callers get [`mount`]'s actionable "not enabled" error and fall back to the
//! driverless `materialize` path.
//!
//! All the hard logic (lazy hydration, write-back, stable inodes, readdirplus, atomic rename) lives
//! in [`Vfs`]; this module only adds the macOS plumbing.

#![cfg(target_os = "macos")]

use std::path::PathBuf;

/// Which macFUSE backend to use on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// User-space FSKit backend (no kext, mounts under `/Volumes`). The default and recommended path.
    #[default]
    FsKit,
    /// Kernel-extension backend — faster, opt-in ("fast mode"), requires the macFUSE kext loaded.
    Kext,
}

impl Backend {
    /// The macFUSE mount-option token selecting this backend (`backend=fskit` / `backend=kext`).
    /// This is the design's `-o backend=fskit` selector.
    pub fn mount_token(self) -> &'static str {
        match self {
            Backend::FsKit => "backend=fskit",
            Backend::Kext => "backend=kext",
        }
    }
}

/// The conventional mount root for a CE drive on macOS (FSKit restricts mounts to `/Volumes`).
pub fn default_mountpoint(drive: &str) -> PathBuf {
    PathBuf::from("/Volumes/CEDrive").join(drive)
}

/// macOS path-component normalization (§7.4): re-export the canonical, platform-independent rule
/// from [`crate::winnames`] (where it is unit-tested on every host). macOS is permissive — only
/// `/`/NUL/`.`/`..`/empty are illegal — so a drive authored on Linux mounts cleanly.
pub use crate::winnames::normalize_macos as normalize_name;

// The actual FUSE-callback filesystem is only compiled when the `macos-fskit` feature is on (so the
// default build needs no macFUSE). It is identical in shape to the Linux adapter — same `Vfs`
// engine, same callback translation — with macOS-specific mount options.
#[cfg(feature = "macos-fskit")]
mod imp {
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::{Context, Result};
    use ce_drive_core::DriveState;
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
    };
    use tokio::runtime::Handle;

    use super::Backend;
    use crate::store_iface::BlockStore;
    use crate::vfs::{Attr, Ino, ROOT_INO, Vfs};

    /// A macFUSE filesystem backed by the shared [`Vfs`] engine. Generic over the block store so the
    /// same adapter mounts a real CE-node-backed drive or (in adapter smoke tests) a mock-backed one.
    pub struct CeDriveFs<S: BlockStore> {
        vfs: Vfs<S>,
        /// A Tokio runtime handle: FUSE callbacks are synchronous, so we `block_on` the engine's
        /// async API. The caller mounts from within a Tokio runtime and passes its handle.
        rt: Handle,
        /// Inode generation. Constant 1 because inodes are never reused while mounted (the stability
        /// invariant the engine guarantees).
        generation: u64,
    }

    impl<S: BlockStore> CeDriveFs<S> {
        pub fn new(vfs: Vfs<S>, rt: Handle) -> Self {
            CeDriveFs { vfs, rt, generation: 1 }
        }

        fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
            self.rt.block_on(fut)
        }
    }

    /// Mount a CE Drive on macOS via macFUSE's `backend` (FSKit user-space by default; kext fast-mode
    /// opt-in). Blocks until unmounted. On unmount the (possibly dirty) drive model is handed back via
    /// `on_unmount` so the caller persists the new [`DriveState`].
    pub fn mount<S, F>(
        vfs: Vfs<S>,
        mountpoint: &Path,
        backend: Backend,
        fs_name: &str,
        vol_name: &str,
        on_unmount: F,
    ) -> Result<()>
    where
        S: BlockStore,
        F: FnOnce(DriveState),
    {
        let rt = Handle::try_current().context("ce-drive macOS mount must run inside a Tokio runtime")?;
        let state_engine = vfs.clone();
        let fs = CeDriveFs::new(vfs, rt.clone());

        // FSKit restricts mounts to `/Volumes`; surface a clear error rather than letting macFUSE
        // fail opaquely deep in the mount syscall.
        if backend == Backend::FsKit && !mountpoint.starts_with("/Volumes") {
            anyhow::bail!(
                "the FSKit backend mounts only under /Volumes; got {} — use macos_fskit::default_mountpoint(drive) or pass Backend::Kext",
                mountpoint.display()
            );
        }

        let options = vec![
            MountOption::FSName(fs_name.to_string()),
            MountOption::Subtype("cedrive".to_string()),
            MountOption::DefaultPermissions,
            // The design's backend selector: `-o backend=fskit` (or kext for fast-mode).
            MountOption::CUSTOM(backend.mount_token().to_string()),
            // macOS Finder shows this as the volume name.
            MountOption::CUSTOM(format!("volname={vol_name}")),
            // macFUSE honors the same writeback-cache lever as Linux to coalesce editor fsync storms.
            MountOption::CUSTOM("writeback_cache".to_string()),
            // Avoid macOS sprinkling `._*`/`.DS_Store` AppleDouble files all over the mount.
            MountOption::CUSTOM("noappledouble".to_string()),
        ];

        // Blocks until unmounted.
        fuser::mount2(fs, mountpoint, &options).context("macFUSE mount")?;

        // Hand the final model back for persistence (close-to-open: flush all on unmount).
        let final_state = rt.block_on(async move { state_engine.state().await });
        on_unmount(final_state);
        Ok(())
    }

    const TTL_FALLBACK: Duration = Duration::from_secs(1);

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

    fn errno(e: &anyhow::Error) -> i32 {
        let msg = e.to_string();
        if msg.contains("no such") || msg.contains("unknown inode") || msg.contains("no such entry") {
            libc::ENOENT
        } else {
            libc::EIO
        }
    }

    impl<S: BlockStore> Filesystem for CeDriveFs<S> {
        fn lookup(&mut self, _req: &Request<'_>, parent: Ino, name: &std::ffi::OsStr, reply: ReplyEntry) {
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
            name: &std::ffi::OsStr,
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
            name: &std::ffi::OsStr,
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

        fn unlink(&mut self, _req: &Request<'_>, parent: Ino, name: &std::ffi::OsStr, reply: ReplyEmpty) {
            let name = name.to_string_lossy().to_string();
            match self.block(self.vfs.unlink(parent, &name)) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno(&e)),
            }
        }

        fn rmdir(&mut self, _req: &Request<'_>, parent: Ino, name: &std::ffi::OsStr, reply: ReplyEmpty) {
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
            name: &std::ffi::OsStr,
            newparent: Ino,
            newname: &std::ffi::OsStr,
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
            let mut all: Vec<(Ino, FileType, String)> = vec![
                (ino, FileType::Directory, ".".to_string()),
                (ROOT_INO, FileType::Directory, "..".to_string()),
            ];
            for (name, child_ino, is_dir) in entries {
                all.push((child_ino, if is_dir { FileType::Directory } else { FileType::RegularFile }, name));
            }
            for (i, (child_ino, kind, name)) in all.into_iter().enumerate().skip(offset.max(0) as usize) {
                if reply.add(child_ino, (i + 1) as i64, kind, &name) {
                    break;
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
            let self_attr = self.block(self.vfs.getattr(ino)).ok();
            let mut idx = 0i64;
            if (offset as usize) == 0 {
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

    /// Fallback timestamp helper (kept for callbacks that need a default).
    #[allow(dead_code)]
    fn now() -> Duration {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(TTL_FALLBACK)
    }
}

#[cfg(feature = "macos-fskit")]
pub use imp::{CeDriveFs, mount};

/// When the `macos-fskit` feature is off, [`mount`] is a stub that returns an actionable error so
/// callers fall back to the driverless `materialize` path. (With the feature on, the real
/// macFUSE-backed `mount` above shadows this.)
#[cfg(not(feature = "macos-fskit"))]
pub fn mount_unavailable() -> anyhow::Error {
    anyhow::anyhow!(
        "macOS FSKit/macFUSE mount is compiled out — rebuild with `--features macos-fskit` on a mac \
         with macFUSE 5.x installed, or use the driverless `ce-drive materialize` fallback (works \
         everywhere, no kernel driver)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mountpoint_is_under_volumes() {
        let mp = default_mountpoint("work");
        assert_eq!(mp, PathBuf::from("/Volumes/CEDrive/work"));
        assert!(mp.starts_with("/Volumes"), "FSKit requires /Volumes mountpoints");
    }

    #[test]
    fn backend_tokens_match_design() {
        assert_eq!(Backend::default(), Backend::FsKit);
        assert_eq!(Backend::FsKit.mount_token(), "backend=fskit");
        assert_eq!(Backend::Kext.mount_token(), "backend=kext");
    }

    #[test]
    fn normalize_rejects_illegal_names() {
        assert_eq!(normalize_name("a.txt"), Some("a.txt".to_string()));
        assert_eq!(normalize_name("CON"), Some("CON".to_string()), "CON is legal on macOS");
        assert!(normalize_name(".").is_none());
        assert!(normalize_name("..").is_none());
        assert!(normalize_name("").is_none());
        assert!(normalize_name("a/b").is_none());
    }
}

// The default-build (no `macos-fskit`) stub error helper is exercised only when the feature is off;
// keep a smoke around it so the cfg path is not silently dead.
#[cfg(all(test, not(feature = "macos-fskit")))]
mod stub_tests {
    #[test]
    fn mount_unavailable_is_actionable() {
        let e = super::mount_unavailable().to_string();
        assert!(e.contains("macos-fskit"), "error tells you how to enable the real mount");
        assert!(e.contains("materialize"), "error points at the fallback");
    }
}
