//! Windows adapters — **WinFsp** (parity) and **ProjFS** (hydrate-once, high-perf for large trees).
//!
//! STATUS: **stubs.** Both interfaces are defined and `cfg`-gated to Windows; the implementations
//! are TODOs. The design (§7.1) specifies WinFsp (`winfsp-rs`) for cross-platform parity and a
//! ProjFS "hydrate-once-then-native" mode (the VFS-for-Git playbook) as a high-perf option for
//! large/monorepo trees.
//!
//! Both will be thin translations onto the shared [`Vfs`](crate::vfs) engine — the lazy-hydration,
//! write-back, stable-inode, readdirplus, and atomic-rename logic is already OS-independent there.

#![cfg(target_os = "windows")]

use std::path::Path;

use anyhow::{Result, bail};

use crate::store_iface::BlockStore;
use crate::vfs::Vfs;

/// Mount a CE Drive on Windows via WinFsp.
///
/// TODO(windows): implement the WinFsp adapter (`winfsp-rs`). Mirrors [`crate::linux_fuser::mount`]:
/// a WinFsp file system whose callbacks call the shared [`Vfs`] engine, mounted at a drive letter or
/// directory mountpoint, blocking until unmounted. Case-insensitivity, reserved names (`CON`/`AUX`),
/// and path-length normalization (the design's §7.4 checklist) are applied at this boundary.
pub fn mount_winfsp<S: BlockStore>(_vfs: Vfs<S>, _mountpoint: &Path) -> Result<()> {
    bail!(
        "Windows WinFsp mount is not implemented yet — use the driverless `ce-drive materialize` \
         fallback. Tracking: windows.rs TODO (mount_winfsp)."
    )
}

/// Mount a CE Drive on Windows via ProjFS in hydrate-once mode (large/monorepo trees).
///
/// TODO(windows): implement the ProjFS provider. ProjFS is write-passthrough (weaker coherence) and
/// hydrates a file once into a real NTFS file on first access (then native speed) — the VFS-for-Git
/// model. The provider's placeholder/hydration callbacks call the shared [`Vfs`] engine for the
/// manifest + block fetch.
pub fn mount_projfs<S: BlockStore>(_vfs: Vfs<S>, _root: &Path) -> Result<()> {
    bail!(
        "Windows ProjFS mount is not implemented yet — use the driverless `ce-drive materialize` \
         fallback. Tracking: windows.rs TODO (mount_projfs)."
    )
}
