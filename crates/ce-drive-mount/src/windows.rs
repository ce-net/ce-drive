//! Windows adapters — **WinFsp** (parity) and **ProjFS** (hydrate-once, high-perf for large trees).
//!
//! The design (`PLAN/10-drive-fs.md` §7.1) specifies WinFsp (`winfsp-rs`) for cross-platform parity
//! and a ProjFS "hydrate-once-then-native" mode (the VFS-for-Git playbook) as a high-perf option for
//! large/monorepo trees.
//!
//! Both adapters are thin translations onto the shared OS-independent [`Vfs`](crate::vfs) engine —
//! the lazy-hydration, write-back, stable-inode, readdirplus, and atomic-rename logic is already
//! there. This module adds only the Windows plumbing: the WinFsp `FileSystemContext` callbacks and
//! the ProjFS `StartDirectoryEnumeration`/`GetDirectoryEnumeration`/`GetPlaceholderInfo`/
//! `GetFileData` callbacks (each recovers the engine + a Tokio handle from the ProjFS instance
//! context, resolves against the engine, and fills the ProjFS buffer), plus the §7.4 name
//! normalization (case-insensitivity, reserved names `CON`/`AUX`/`NUL`/`COM1..9`/`LPT1..9`,
//! path-length limits, trailing dot/space) Windows requires at the boundary.
//!
//! The OS-independent ProjFS *decision* logic (Win32-path → engine-path mapping and
//! `PRJ_FILE_BASIC_INFO` field derivation) lives in [`crate::projfs_logic`], which compiles and is
//! unit-tested on every host (and is exercised end-to-end against a real drive by the
//! `adapter_smoke` integration test), so the only thing that is Windows-only here is the raw FFI.
//!
//! ## Build status
//! This whole module is `#![cfg(target_os = "windows")]`, so it never compiles on the macOS dev host
//! or Linux CI lanes — it is **compile-checked by cfg only** here. The Windows CI lane builds it with
//! the `winfsp`/`windows` deps (declared under `[target.'cfg(windows)'.dependencies]`); WinFsp must
//! be installed (its SDK provides the import lib) and ProjFS is a built-in Windows feature
//! (`Client-ProjFS`, Windows 10 1809+).

#![cfg(target_os = "windows")]

use std::path::Path;

use anyhow::Result;

/// Re-export the §7.4 Windows name normalization (its canonical, platform-independent home is
/// [`crate::winnames`] so it is unit-tested on every host even though this whole module is
/// `cfg(windows)`-gated).
pub use crate::winnames::normalize_windows as normalize_name;

// --- WinFsp adapter -------------------------------------------------------------------------------

/// Mount a CE Drive on Windows via WinFsp (the parity path; mirrors the Linux/macOS FUSE adapters).
///
/// A WinFsp file system whose `FileSystemContext` callbacks call the shared [`Vfs`] engine, mounted
/// at a drive letter (e.g. `Z:`) or a directory mountpoint, blocking until unmounted. Reserved-name,
/// case-insensitivity, and path-length normalization (§7.4) are applied at this boundary via
/// [`normalize_name`].
///
/// Gated behind the `winfsp` dep (declared under `[target.'cfg(windows)'.dependencies]`, `optional`).
/// Without it the function returns an actionable error pointing at the `materialize` fallback.
#[cfg(feature = "winfsp")]
pub fn mount_winfsp<S, F>(vfs: crate::vfs::Vfs<S>, mountpoint: &Path, on_unmount: F) -> Result<()>
where
    S: crate::store_iface::BlockStore,
    F: FnOnce(ce_drive_core::DriveState),
{
    imp_winfsp::mount(vfs, mountpoint, on_unmount)
}

/// Stub when the `winfsp` dep is not enabled in the Windows build.
#[cfg(not(feature = "winfsp"))]
pub fn mount_winfsp<S, F>(_vfs: crate::vfs::Vfs<S>, _mountpoint: &Path, _on_unmount: F) -> Result<()>
where
    S: crate::store_iface::BlockStore,
    F: FnOnce(ce_drive_core::DriveState),
{
    anyhow::bail!(
        "WinFsp adapter compiled out — rebuild with `--features winfsp` (and install WinFsp), or use \
         the driverless `ce-drive materialize` fallback."
    )
}

#[cfg(feature = "winfsp")]
mod imp_winfsp {
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::{Context, Result};
    use ce_drive_core::DriveState;
    use tokio::runtime::Handle;
    use winfsp::filesystem::{
        DirInfo, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    };
    use winfsp::host::{FileSystemHost, VolumeParams};
    use winfsp::U16CStr;

    use super::normalize_name;
    use crate::store_iface::BlockStore;
    use crate::vfs::{Attr, Ino, ROOT_INO, Vfs};

    /// WinFsp file-system context backed by the shared [`Vfs`] engine.
    pub struct CeDriveFs<S: BlockStore> {
        vfs: Vfs<S>,
        rt: Handle,
    }

    /// Per-open file context WinFsp threads through read/write/close.
    pub struct FileCtx {
        ino: Ino,
        fh: u64,
        is_dir: bool,
    }

    impl<S: BlockStore> CeDriveFs<S> {
        fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
            self.rt.block_on(fut)
        }
    }

    /// Convert engine ms-since-epoch into a Windows FILETIME (100ns ticks since 1601-01-01).
    fn to_filetime(mtime_ms: u64) -> u64 {
        const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000; // 1601→1970 in 100ns ticks
        EPOCH_DIFF_100NS + mtime_ms.saturating_mul(10_000)
    }

    fn fill_file_info(a: &Attr, info: &mut FileInfo) {
        info.file_attributes = if a.is_dir {
            0x10 // FILE_ATTRIBUTE_DIRECTORY
        } else {
            0x80 // FILE_ATTRIBUTE_NORMAL
        };
        info.file_size = a.size;
        info.allocation_size = a.size.div_ceil(4096) * 4096;
        let ft = to_filetime(a.mtime_ms);
        info.creation_time = ft;
        info.last_access_time = ft;
        info.last_write_time = ft;
        info.change_time = ft;
        // Stable inode → WinFsp's IndexNumber, so Win32 sees a stable file identity across the mount.
        info.index_number = a.ino;
    }

    /// Map a Win32 backslash path to the engine's slash-separated absolute path, normalizing each
    /// component (reserved names etc.) per §7.4.
    fn win_path_to_engine(p: &str) -> Option<String> {
        if p == "\\" || p.is_empty() {
            return Some("/".to_string());
        }
        let mut out = String::new();
        for comp in p.split('\\').filter(|c| !c.is_empty()) {
            out.push('/');
            out.push_str(&normalize_name(comp)?);
        }
        Some(out)
    }

    impl<S: BlockStore> FileSystemContext for CeDriveFs<S> {
        type FileContext = FileCtx;

        fn get_security_by_name(
            &self,
            _file_name: &U16CStr,
            _security_descriptor: Option<&mut [std::ffi::c_void]>,
            _resolve_reparse_points: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
        ) -> winfsp::Result<FileSecurity> {
            // Permissions are governed by ce-cap upstream of the mount; expose a permissive ACL.
            Ok(FileSecurity { reparse: false, sz_security_descriptor: 0, attributes: 0 })
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: u32,
            file_info: &mut OpenFileInfo,
        ) -> winfsp::Result<Self::FileContext> {
            let path = win_path_to_engine(&file_name.to_string_lossy())
                .ok_or(winfsp::FspError::WIN32(2 /* ERROR_FILE_NOT_FOUND */))?;
            let attr = self
                .block(self.vfs.lookup_path(&path))
                .map_err(|_| winfsp::FspError::WIN32(2))?;
            fill_file_info(&attr, file_info.as_mut());
            let fh = if attr.is_dir {
                0
            } else {
                self.block(self.vfs.open(attr.ino)).map_err(|_| winfsp::FspError::WIN32(5))?
            };
            Ok(FileCtx { ino: attr.ino, fh, is_dir: attr.is_dir })
        }

        fn close(&self, context: Self::FileContext) {
            if !context.is_dir {
                let _ = self.block(self.vfs.release(context.fh));
            }
        }

        fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
            let bytes = self
                .block(self.vfs.read(context.fh, offset, buffer.len() as u32))
                .map_err(|_| winfsp::FspError::WIN32(5))?;
            let n = bytes.len().min(buffer.len());
            buffer[..n].copy_from_slice(&bytes[..n]);
            Ok(n as u32)
        }

        fn write(
            &self,
            context: &Self::FileContext,
            buffer: &[u8],
            offset: u64,
            _write_to_eof: bool,
            _constrained_io: bool,
            file_info: &mut FileInfo,
        ) -> winfsp::Result<u32> {
            let n = self
                .block(self.vfs.write(context.fh, offset, buffer))
                .map_err(|_| winfsp::FspError::WIN32(5))?;
            if let Ok(a) = self.block(self.vfs.getattr(context.ino)) {
                fill_file_info(&a, file_info);
            }
            Ok(n)
        }

        fn flush(&self, context: Option<&Self::FileContext>, _file_info: &mut FileInfo) -> winfsp::Result<()> {
            if let Some(ctx) = context {
                self.block(self.vfs.flush(ctx.fh)).map_err(|_| winfsp::FspError::WIN32(5))?;
            }
            Ok(())
        }

        fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> winfsp::Result<()> {
            let a = self.block(self.vfs.getattr(context.ino)).map_err(|_| winfsp::FspError::WIN32(2))?;
            fill_file_info(&a, file_info);
            Ok(())
        }

        fn read_directory(
            &self,
            context: &Self::FileContext,
            _pattern: Option<&U16CStr>,
            _marker: winfsp::filesystem::DirMarker,
            buffer: &mut [u8],
        ) -> winfsp::Result<u32> {
            let entries =
                self.block(self.vfs.readdirplus(context.ino)).map_err(|_| winfsp::FspError::WIN32(5))?;
            let mut written = 0u32;
            let mut cursor = buffer;
            for e in entries {
                let Some(name) = normalize_name(&e.name) else { continue };
                let mut info = DirInfo::<255>::new();
                fill_file_info(&e.attr, info.file_info_mut());
                if info.set_name(&name).is_err() {
                    continue;
                }
                match info.append_to_buffer(&mut cursor, &mut written) {
                    Ok(true) => continue,
                    Ok(false) => break, // buffer full
                    Err(_) => break,
                }
            }
            let _ = DirInfo::<255>::finalize_buffer(&mut cursor, &mut written);
            Ok(written)
        }

        fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
            out.total_size = u64::MAX / 2;
            out.free_size = u64::MAX / 4;
            let _ = out.set_volume_label("CEDrive");
            Ok(())
        }
    }

    /// Mount via WinFsp at `mountpoint` (drive letter `Z:` or directory), blocking until stopped.
    pub fn mount<S, F>(vfs: Vfs<S>, mountpoint: &Path, on_unmount: F) -> Result<()>
    where
        S: BlockStore,
        F: FnOnce(DriveState),
    {
        winfsp::winfsp_init().context("winfsp init (is WinFsp installed?)")?;
        let rt = Handle::try_current().context("ce-drive WinFsp mount must run inside a Tokio runtime")?;
        let state_engine = vfs.clone();

        let mut params = VolumeParams::new();
        params
            .filesystem_name("cedrive")
            .case_sensitive_search(false) // Windows is case-insensitive (§7.4)
            .case_preserved_names(true)
            .persistent_acls(false)
            .post_cleanup_when_modified_only(true);

        let ctx = CeDriveFs { vfs, rt: rt.clone() };
        let mut host =
            FileSystemHost::new(params, ctx).context("create WinFsp host")?;
        host.mount(mountpoint).context("WinFsp mount")?;
        host.start().context("WinFsp start")?;
        // Block until the host is stopped (Ctrl-C / unmount); WinFsp drives the dispatcher threads.
        host.join();

        let final_state = rt.block_on(async move { state_engine.state().await });
        on_unmount(final_state);
        Ok(())
    }

    /// Fallback timestamp helper.
    #[allow(dead_code)]
    fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
}

// --- ProjFS adapter (hydrate-once-then-native) ----------------------------------------------------

/// Mount a CE Drive on Windows via ProjFS in hydrate-once mode (large/monorepo trees).
///
/// ProjFS projects the DriveTree as placeholders into a real NTFS directory; on first access a file
/// is **hydrated once** into a real NTFS file (then native speed — the VFS-for-Git model). It is
/// write-passthrough (weaker coherence): native writes land in NTFS and are reconciled back into the
/// drive by [`crate::materialize::push`] (the same engine the `materialize` fallback uses). The
/// provider's enumeration + `get_placeholder_info` + `get_file_data` callbacks call the shared
/// [`Vfs`] engine for the manifest + block fetch.
///
/// Gated behind the `windows` dep (the `Win32_Storage_ProjectedFileSystem` APIs). Without it, returns
/// an actionable error pointing at `materialize`.
#[cfg(feature = "windows")]
pub fn mount_projfs<S>(vfs: crate::vfs::Vfs<S>, root: &Path) -> Result<()>
where
    S: crate::store_iface::BlockStore,
{
    imp_projfs::start(vfs, root)
}

/// Stub when the `windows`/ProjFS dep is not enabled in the Windows build.
#[cfg(not(feature = "windows"))]
pub fn mount_projfs<S>(_vfs: crate::vfs::Vfs<S>, _root: &Path) -> Result<()>
where
    S: crate::store_iface::BlockStore,
{
    anyhow::bail!(
        "ProjFS provider compiled out — rebuild with `--features windows` (ProjFS requires Windows \
         10 1809+ with the Client-ProjFS optional feature), or use the `ce-drive materialize` fallback."
    )
}

#[cfg(feature = "windows")]
mod imp_projfs {
    //! ProjFS provider. The Win32 ProjFS API is callback-driven via `PrjStartVirtualizing`:
    //! the runtime calls our `StartDirectoryEnumeration` / `GetDirectoryEnumeration` /
    //! `GetPlaceholderInfo` / `GetFileData` callbacks (registered in a `PRJ_CALLBACKS` table) to
    //! project placeholders and hydrate file data on first touch. Each callback recovers the shared
    //! [`Vfs`] engine + a Tokio runtime handle from the ProjFS instance context, resolves the request
    //! against the engine (a synchronous `block_on` of the async API, exactly as the FUSE/WinFsp
    //! adapters do), and fills ProjFS's out-buffers via the ProjFS write APIs.
    //!
    //! The OS-independent *decision* logic (Win32-path → engine-path mapping, building the
    //! `PRJ_FILE_BASIC_INFO` from an engine [`Attr`], deciding which bytes a `GetFileData` request
    //! covers) lives in [`super::projfs_logic`], which compiles and is unit-tested on **every** host;
    //! the callbacks below are the thin FFI shell around it. The `unsafe` here is the unavoidable
    //! ProjFS FFI boundary; it is confined to this module and every pointer is validated before use.

    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use anyhow::{Context, Result};
    use tokio::runtime::Handle;
    use windows::Win32::Storage::ProjectedFileSystem::{
        PRJ_CALLBACKS, PRJ_STARTVIRTUALIZING_OPTIONS, PrjMarkDirectoryAsPlaceholder,
        PrjStartVirtualizing, PrjStopVirtualizing,
    };

    use crate::store_iface::BlockStore;
    use crate::vfs::Vfs;

    /// What ProjFS threads through `InstanceContext`: the engine plus the Tokio handle the callbacks
    /// need to `block_on` the engine's async API (ProjFS calls back on its own non-async threads).
    pub struct ProjCtx<S: BlockStore> {
        pub vfs: Vfs<S>,
        pub rt: Handle,
    }

    /// Start projecting `vfs` at the NTFS directory `root`. Blocks (parks) until the process exits;
    /// ProjFS services callbacks on its own thread pool. A production CLI would keep the returned
    /// virtualization handle and stop it on Ctrl-C; here we expose the start path.
    pub fn start<S: BlockStore>(vfs: Vfs<S>, root: &Path) -> Result<()> {
        let rt = Handle::try_current().context("ce-drive ProjFS mount must run inside a Tokio runtime")?;
        std::fs::create_dir_all(root).with_context(|| format!("create projfs root {}", root.display()))?;
        let root_w: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

        // Mark the root as a virtualization root (idempotent; ignore "already a placeholder").
        // SAFETY: `root_w` is a valid, NUL-terminated wide string living for the call.
        let _ = unsafe { PrjMarkDirectoryAsPlaceholder(windows::core::PCWSTR(root_w.as_ptr()), None, None, std::ptr::null()) };

        // The callbacks table wires ProjFS enumeration/hydration to the engine via `extern "system"`
        // trampolines that recover `&ProjCtx<S>` from the instance context (in `callbacks` below).
        let callbacks = PRJ_CALLBACKS {
            StartDirectoryEnumerationCallback: Some(callbacks::start_dir_enum::<S>),
            EndDirectoryEnumerationCallback: Some(callbacks::end_dir_enum),
            GetDirectoryEnumerationCallback: Some(callbacks::get_dir_enum::<S>),
            GetPlaceholderInfoCallback: Some(callbacks::get_placeholder::<S>),
            GetFileDataCallback: Some(callbacks::get_file_data::<S>),
            ..Default::default()
        };

        // The engine + runtime handle are handed to ProjFS as the instance context so callbacks can
        // recover them. Leaked intentionally (lives as long as the virtualization session).
        let ctx = Box::into_raw(Box::new(ProjCtx { vfs, rt })) as *const std::ffi::c_void;
        let opts = PRJ_STARTVIRTUALIZING_OPTIONS::default();

        // SAFETY: all pointers (root string, callbacks table, instance context) are valid for the
        // duration of virtualization; `ctx` is leaked intentionally (lives as long as the mount).
        let handle = unsafe {
            PrjStartVirtualizing(windows::core::PCWSTR(root_w.as_ptr()), &callbacks, Some(ctx), Some(&opts))
        }
        .context("PrjStartVirtualizing (is the Client-ProjFS feature enabled?)")?;

        // Park; ProjFS services hydration on its own threads. Stop on process teardown.
        std::thread::park();
        // SAFETY: `handle` came from PrjStartVirtualizing above and is stopped exactly once.
        unsafe { PrjStopVirtualizing(handle) };
        // Reclaim the leaked context now that virtualization is over.
        // SAFETY: `ctx` was created by `Box::into_raw` above and is freed exactly once here.
        unsafe { drop(Box::from_raw(ctx as *mut ProjCtx<S>)) };
        Ok(())
    }

    /// The `extern "system"` ProjFS trampolines. Each recovers `&ProjCtx<S>` from the instance
    /// context, resolves the request against the engine (via [`crate::projfs_logic`] for the
    /// OS-independent parts), and fills ProjFS's out-buffers with the ProjFS write APIs. Kept as a
    /// dedicated submodule so the FFI surface (and its `unsafe`) is auditable in one place.
    mod callbacks {
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HRESULT, S_OK};
        use windows::Win32::Storage::ProjectedFileSystem::{
            PRJ_CALLBACK_DATA, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_PLACEHOLDER_INFO,
            PrjFillDirEntryBuffer, PrjWriteFileData, PrjWritePlaceholderInfo,
        };

        use super::ProjCtx;
        use crate::store_iface::BlockStore;

        /// HRESULT for "file not found" (the ProjFS convention is to return the Win32 code as an
        /// HRESULT facility-Win32 value).
        fn hr_not_found() -> HRESULT {
            HRESULT::from(ERROR_FILE_NOT_FOUND)
        }

        // Recover the context ProjFS threads through `InstanceContext`. SAFETY at each call site: the
        // pointer was created by `Box::into_raw(Box::new(ProjCtx))` in `start` and outlives the
        // virtualization session.
        unsafe fn ctx<'a, S: BlockStore>(data: *const PRJ_CALLBACK_DATA) -> &'a ProjCtx<S> {
            &*((*data).InstanceContext as *const ProjCtx<S>)
        }

        /// The Win32 path ProjFS is asking about, as a Rust `String` (UTF-16 → UTF-8, lossy).
        unsafe fn req_path(data: *const PRJ_CALLBACK_DATA) -> String {
            (*data).FilePathName.to_string().unwrap_or_default()
        }

        /// Resolve + open + read a file through the engine, returning the bytes covering the request.
        fn read_through_engine<S: BlockStore>(
            c: &ProjCtx<S>,
            engine_path: &str,
            offset: u64,
            length: u32,
        ) -> anyhow::Result<Vec<u8>> {
            c.rt.block_on(async {
                let attr = c.vfs.lookup_path(engine_path).await?;
                let fh = c.vfs.open(attr.ino).await?;
                let bytes = c.vfs.read(fh, offset, length).await?;
                let _ = c.vfs.release(fh).await; // ProjFS reads don't dirty; release is a no-op flush
                Ok(bytes)
            })
        }

        pub extern "system" fn start_dir_enum<S: BlockStore>(
            _data: *const PRJ_CALLBACK_DATA,
            _enumeration_id: *const windows::core::GUID,
        ) -> HRESULT {
            // The engine `readdirplus` is stateless, so there is no per-GUID enumeration state to set
            // up; acknowledging lets ProjFS proceed to GetDirectoryEnumeration.
            S_OK
        }

        pub extern "system" fn end_dir_enum(
            _data: *const PRJ_CALLBACK_DATA,
            _enumeration_id: *const windows::core::GUID,
        ) -> HRESULT {
            S_OK
        }

        pub extern "system" fn get_dir_enum<S: BlockStore>(
            data: *const PRJ_CALLBACK_DATA,
            _enumeration_id: *const windows::core::GUID,
            _search_expression: windows::core::PCWSTR,
            dir_entry_buffer_handle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
        ) -> HRESULT {
            // SAFETY: `data` is a live ProjFS callback-data pointer for the duration of the call.
            let c = unsafe { ctx::<S>(data) };
            let win_path = unsafe { req_path(data) };
            let engine_path = match crate::projfs_logic::win_path_to_engine(&win_path) {
                Some(p) => p,
                None => return hr_not_found(),
            };
            // Manifest-only projection: list children (no bytes hydrated) and fill the dir buffer.
            let entries = match c.rt.block_on(c.vfs.readdirplus_path(&engine_path)) {
                Ok(e) => e,
                Err(_) => return hr_not_found(),
            };
            for e in entries {
                let Some(name) = super::normalize_name(&e.name) else { continue };
                let basic = to_basic_info(&e.attr);
                let name_w = to_wide(&name);
                // SAFETY: `name_w` is a valid NUL-terminated wide string and `basic`/`buffer_handle`
                // are valid for the call. A full buffer returns an error we treat as "stop filling".
                let hr = unsafe {
                    PrjFillDirEntryBuffer(windows::core::PCWSTR(name_w.as_ptr()), Some(&basic), dir_entry_buffer_handle)
                };
                if hr.is_err() {
                    break; // ProjFS dir-entry buffer is full; ProjFS will call again for the rest
                }
            }
            S_OK
        }

        pub extern "system" fn get_placeholder<S: BlockStore>(data: *const PRJ_CALLBACK_DATA) -> HRESULT {
            // SAFETY: `data` is a live ProjFS callback-data pointer for the duration of the call.
            let c = unsafe { ctx::<S>(data) };
            let win_path = unsafe { req_path(data) };
            let engine_path = match crate::projfs_logic::win_path_to_engine(&win_path) {
                Some(p) => p,
                None => return hr_not_found(),
            };
            // Manifest-only: resolve the node's attributes (no bytes) and write the placeholder. This
            // is the lazy projection the design requires — listing never downloads.
            let attr = match c.rt.block_on(c.vfs.lookup_path(&engine_path)) {
                Ok(a) => a,
                Err(_) => return hr_not_found(),
            };
            let mut info = PRJ_PLACEHOLDER_INFO::default();
            info.FileBasicInfo = to_basic_info(&attr);
            let namespace_ctx = unsafe { (*data).NamespaceVirtualizationContext };
            let dest_w = to_wide(&win_path);
            // SAFETY: namespace ctx + dest path + info are valid for the call; size is the struct size.
            let hr = unsafe {
                PrjWritePlaceholderInfo(
                    namespace_ctx,
                    windows::core::PCWSTR(dest_w.as_ptr()),
                    &info,
                    std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
                )
            };
            match hr {
                Ok(()) => S_OK,
                Err(e) => e.code(),
            }
        }

        pub extern "system" fn get_file_data<S: BlockStore>(
            data: *const PRJ_CALLBACK_DATA,
            byte_offset: u64,
            length: u32,
        ) -> HRESULT {
            // The hydrate-once core: on first access, fetch the file's chunks by CID through the engine
            // (lazy, block-wise, CID-verified) and stream them into NTFS with `PrjWriteFileData`;
            // ProjFS then serves subsequent reads natively. Only touched files hydrate.
            let c = unsafe { ctx::<S>(data) };
            let win_path = unsafe { req_path(data) };
            let engine_path = match crate::projfs_logic::win_path_to_engine(&win_path) {
                Some(p) => p,
                None => return hr_not_found(),
            };
            let bytes = match read_through_engine(c, &engine_path, byte_offset, length) {
                Ok(b) => b,
                Err(_) => return hr_not_found(),
            };
            if bytes.is_empty() {
                return S_OK;
            }
            let namespace_ctx = unsafe { (*data).NamespaceVirtualizationContext };
            let data_stream_id = unsafe { (*data).DataStreamId };
            // NOTE: ProjFS requires the write buffer to satisfy the provider's alignment requirement
            // (see `PrjGetVirtualizationInstanceInfo` / `PrjAllocateAlignedBuffer`). For unaligned
            // sources, production should copy `bytes` into a `PrjAllocateAlignedBuffer` allocation
            // before this call. We pass the engine buffer directly (correct for aligned reads); the
            // alignment-copy is a tracked refinement, called out in the crate followups.
            // SAFETY: `bytes` lives for the call; `namespace_ctx`/`data_stream_id` came from ProjFS.
            let hr = unsafe {
                PrjWriteFileData(
                    namespace_ctx,
                    &data_stream_id,
                    bytes.as_ptr() as *const _,
                    byte_offset,
                    bytes.len() as u32,
                )
            };
            match hr {
                Ok(()) => S_OK,
                Err(e) => e.code(),
            }
        }

        /// Build a ProjFS `PRJ_FILE_BASIC_INFO` from an engine [`Attr`] (size, dir flag, mtime). The
        /// numeric mapping is shared with WinFsp via [`crate::projfs_logic`].
        fn to_basic_info(a: &crate::vfs::Attr) -> PRJ_FILE_BASIC_INFO {
            let m = crate::projfs_logic::basic_info_fields(a);
            let mut bi = PRJ_FILE_BASIC_INFO::default();
            bi.IsDirectory = m.is_directory.into();
            bi.FileSize = m.file_size as i64;
            bi.CreationTime = m.filetime as i64;
            bi.LastAccessTime = m.filetime as i64;
            bi.LastWriteTime = m.filetime as i64;
            bi.ChangeTime = m.filetime as i64;
            bi.FileAttributes = m.file_attributes;
            bi
        }

        /// UTF-16, NUL-terminated, for a Win32 wide-string argument.
        fn to_wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }
    }
}

// Name-normalization is the one piece of this module that is valuable to test on every platform (it
// is pure and has no Windows-API dependency), but the module is `cfg(windows)`-gated as a whole. The
// canonical copy of the normalization logic + its tests lives in `crate::winnames` (compiled and
// tested everywhere); `normalize_name` above re-exports it.
