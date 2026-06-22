//! # ce-drive-mount — Face 1: the developer filesystem
//!
//! The mount layer of CE Drive: it presents a [`Drive`](ce_drive_core::Drive) (its
//! [`DriveTree`](ce_drive_core::tree::DriveTree) move-CRDT + content map + content store) as a real
//! filesystem you can `cd` into and `cargo build` / `git status` against.
//!
//! ## Architecture (one engine, thin adapters, a driverless fallback)
//! ```text
//! ce-drive-core  ──►  Vfs (this crate, OS-independent)  ──►  { fuser | macFUSE-FSKit | WinFsp/ProjFS }
//!                                                       └──►  materialize (no-driver fallback)
//! ```
//! * [`vfs`] — the shared engine: lazy block hydration on open, range/block fetch via a
//!   [`store_iface::BlockStore`], a write-back cache with async upload, stable inodes, readdirplus,
//!   atomic rename, and long attr/entry TTLs. **All hard logic lives here**, so every OS adapter is
//!   thin and the engine is fully unit-tested against a mock store with no live node.
//! * [`materialize`] — the driverless fallback (`materialize` / `push` / `watch`): walk the manifest
//!   into a real directory, edit/build natively, sync changes back. Works everywhere, no kernel
//!   driver (the default for CI/containers/no-admin machines).
//! * [`linux_fuser`] — the `fuser` (libfuse-ABI) Linux adapter. **Feature-gated behind `fuse`**
//!   (default off) so the workspace builds where libfuse is absent (e.g. macOS); build the real
//!   mount with `cargo build --features fuse` on Linux.
//! * `macos_fskit` — the macFUSE **FSKit** backend (`-o backend=fskit`, mounting under
//!   `/Volumes/CEDrive`), kext fast-mode opt-in. `cfg(target_os = "macos")`; the FUSE-callback
//!   filesystem inside it is gated behind the **`macos-fskit`** feature (which links macFUSE via
//!   pkg-config). The default macOS build is green *without* macFUSE; build the real mount with
//!   `cargo build --features macos-fskit` on a mac with **macFUSE 5.x** installed
//!   (`brew install --cask macfuse`). Without macFUSE, `fuser`'s build script fails at pkg-config
//!   (no `osxfuse.pc`), and callers fall back to `materialize`.
//! * [`windows`] — the **WinFsp** adapter (parity) and the **ProjFS** hydrate-once provider.
//!   `cfg(target_os = "windows")`, so it never compiles off-Windows (compile-checked by cfg on the
//!   mac/Linux lanes); the Windows CI lane builds it with the `winfsp`/`windows` deps. The §7.4 name
//!   rules it applies live in [`winnames`] and are unit-tested on every host.
//!
//! ## Standards
//! Edition 2024, `anyhow::Result`, `tracing` (no `println!` in the library), no `unwrap()` on
//! production paths. The only `unsafe` in the crate is the unavoidable Win32 FFI boundary into the
//! ProjFS API (`PrjStartVirtualizing` + its `extern "system"` callbacks), confined to the
//! `cfg(windows)` [`windows`] module and never compiled on Unix. Mesh-first / capability-only trust
//! is inherited from `ce-drive-core` (this crate adds no node RPC and no new transport).

pub mod materialize;
pub mod store_iface;
pub mod vfs;
// Pure §7.4 path-component normalization (Windows reserved names, macOS structural rules). Lives
// here — not behind a per-OS `cfg` — so its rules are unit-tested on every host; the per-OS adapters
// re-export from it.
pub mod winnames;

// OS-independent ProjFS decision logic (Win32-path → engine-path mapping, `PRJ_FILE_BASIC_INFO`
// field derivation). Always compiled — not behind `cfg(windows)` — so it is unit-tested on every
// host; the `cfg(windows)` ProjFS callbacks in `windows` are a thin FFI shell around it.
pub mod projfs_logic;

// The fuser adapter is feature-gated so the workspace builds without libfuse. The module's own
// `#![cfg(feature = "fuse")]` keeps it out of the build unless the feature is on.
#[cfg(feature = "fuse")]
pub mod linux_fuser;

// The macOS macFUSE/FSKit adapter. `cfg`-gated to macOS; the real FUSE-callback filesystem inside it
// is further gated behind the `macos-fskit` feature (which links macFUSE), so the default macOS build
// is green without macFUSE installed. Build the real mount with `--features macos-fskit`.
#[cfg(target_os = "macos")]
pub mod macos_fskit;

// The Windows WinFsp + ProjFS adapters. `cfg(windows)`-gated, so they never compile on macOS/Linux
// (compile-checked by cfg on those hosts); the Windows CI lane builds them with the `winfsp`/`windows`
// deps. The §7.4 name rules they apply live in `winnames` and are tested everywhere.
#[cfg(target_os = "windows")]
pub mod windows;

pub use materialize::{MaterializeReport, PushReport, materialize, push, watch};
pub use store_iface::{BlockStore, CoreStore, PutResult};
pub use vfs::{Attr, DirEntryPlus, FlushResult, Ino, ROOT_INO, Vfs};
