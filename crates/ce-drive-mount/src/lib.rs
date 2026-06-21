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
//! * [`macos_fskit`] / [`windows`] — macFUSE-FSKit (macOS) and WinFsp/ProjFS (Windows) adapter
//!   **stubs**: the interface is defined and `cfg`-gated, the implementation is a clearly-marked
//!   TODO that returns an actionable error pointing at the `materialize` fallback.
//!
//! ## Standards
//! Edition 2024, `anyhow::Result`, `tracing` (no `println!` in the library), no `unsafe`, no
//! `unwrap()` on production paths. Mesh-first / capability-only trust is inherited from
//! `ce-drive-core` (this crate adds no node RPC and no new transport).

pub mod materialize;
pub mod store_iface;
pub mod vfs;

// The fuser adapter is feature-gated so the workspace builds without libfuse. The module's own
// `#![cfg(feature = "fuse")]` keeps it out of the build unless the feature is on.
#[cfg(feature = "fuse")]
pub mod linux_fuser;

// Per-OS adapter stubs. `cfg`-gated so they only compile on their target OS (their interfaces are
// defined; the implementations are TODOs that fall back to `materialize`).
#[cfg(target_os = "macos")]
pub mod macos_fskit;
#[cfg(target_os = "windows")]
pub mod windows;

pub use materialize::{MaterializeReport, PushReport, materialize, push, watch};
pub use store_iface::{BlockStore, CoreStore, PutResult};
pub use vfs::{Attr, DirEntryPlus, FlushResult, Ino, ROOT_INO, Vfs};
