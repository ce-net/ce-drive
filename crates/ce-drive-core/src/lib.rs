//! # ce-drive-core
//!
//! The single core of CE Drive: a pure-Rust library that is the source of truth for the storage
//! model, the directory CRDT, sharing, durability, and audit. Both faces of CE Drive — the
//! cross-platform developer **mount** and the **Drive/Workspace web app** — are thin adapters over
//! this crate (and it compiles to WASM for the in-browser node).
//!
//! It **composes** existing CE primitives rather than reinventing them:
//! - the chunk/CID/delta engine is [`rdev::chunk`] + [`rdev::delta`] over `ce-rs::data` ([`store`]),
//! - the directory tree is a Kleppmann move-CRDT implemented as a ce-coord [`StateMachine`]
//!   ([`tree`]) with an orthogonal NodeId-keyed content map ([`content`]),
//! - sharing is `ce-cap` attenuating capability chains, per folder ([`share`]),
//! - durability is `ce-pin`-style DHT announce/replicate policy ([`durability`]),
//! - audit v1 reads CE's on-chain capability grant/revoke facts ([`audit`]).
//!
//! Milestones covered here: **M1** (`tree` + `content` + `store` + the single-writer [`drive::Drive`]
//! handle), **M2** (`share` + `durability` + `audit`), and **multi-writer sync** ([`sync`]): the
//! [`sync::SyncedDrive`] handle promotes the tree + content map off single-writer onto ce-coord's
//! `Merged`, so N devices each append to their own writer log and all converge leaderless.
//!
//! ## Standards
//! Edition 2024, `anyhow::Result`, `tracing` (no `println!` in the library), no `unsafe`, no
//! `unwrap()` on production paths. Money (where it appears via durability/rent) is `ce-rs::Amount`
//! base units — integers, decimal strings on the wire, never floats.

pub mod audit;
pub mod changes;
pub mod content;
pub mod drive;
pub mod durability;
pub mod names;
pub mod search;
pub mod share;
pub mod store;
pub mod sync;
pub mod tree;

// Re-export the most-used types at the crate root for ergonomic downstream use.
pub use content::{ContentMap, ContentOp, FileContent};
pub use drive::{ContentConflict, Drive, DriveState, DirEntry};
pub use durability::{Durability, PinPolicy, PinStatus};
pub use share::{Ability, Grant, Workspace};
pub use store::{Store, StoredFile};
pub use sync::{ContentMerge, StampedContentOp, SyncedDrive, TreeMerge};
pub use tree::{DriveTree, LIMBO, MoveOp, NodeId, NodeKind, ROOT, TRASH, Timestamp};

pub use audit::{Audit, AuditEntry, AuditJournal, GrantRecord, RevokeRecord};

pub use changes::{Change, ChangeKind, ChangeLog, Cursor};
pub use names::{NameError, MAX_NAME_BYTES, check_name, validate_name};
pub use search::{SearchHit, SearchIndex};
