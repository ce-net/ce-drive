//! macOS adapter — macFUSE **FSKit** backend (with a kext fast-mode opt-in).
//!
//! STATUS: **stub.** The interface is defined and `cfg`-gated to macOS; the implementation is a
//! TODO. The design (§7.1) chooses the FSKit user-space backend (`-o backend=fskit`, mounting under
//! `/Volumes/CEDrive/<drive>`) so there is no kext and no Recovery dance, with the kext backend as an
//! opt-in "fast mode."
//!
//! Like the Linux adapter, the real implementation will be a thin translation of FSKit operations
//! onto the shared [`Vfs`](crate::vfs) engine — all the hard logic (lazy hydration, write-back,
//! stable inodes, readdirplus, atomic rename) already lives there and is OS-independent, so this
//! module only adds the macOS plumbing.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::store_iface::BlockStore;
use crate::vfs::Vfs;

/// Which macFUSE backend to use on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// User-space FSKit backend (no kext, mounts under `/Volumes`). The default and recommended path.
    #[default]
    FsKit,
    /// Kernel-extension backend — faster, opt-in ("fast mode"), requires the macFUSE kext loaded.
    Kext,
}

/// The conventional mount root for a CE drive on macOS (FSKit restricts mounts to `/Volumes`).
pub fn default_mountpoint(drive: &str) -> PathBuf {
    PathBuf::from("/Volumes/CEDrive").join(drive)
}

/// Mount a CE Drive on macOS via macFUSE.
///
/// TODO(macos): implement the FSKit backend. The shape mirrors [`crate::linux_fuser::mount`]:
/// construct an FSKit volume whose operations call the shared [`Vfs`] engine, register it under
/// `/Volumes/CEDrive/<drive>`, and block until unmounted. The `Kext` backend is the opt-in fast mode.
/// Until then this returns a clear, actionable error so callers fall back to `materialize`.
pub fn mount<S: BlockStore>(_vfs: Vfs<S>, _mountpoint: &Path, _backend: Backend) -> Result<()> {
    bail!(
        "macOS FSKit/macFUSE mount is not implemented yet — use the driverless `ce-drive materialize` \
         fallback (works everywhere, no kernel driver). Tracking: macos_fskit.rs TODO."
    )
}
