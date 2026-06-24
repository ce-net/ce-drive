# Changelog

All notable changes to CE Drive are documented here. The project adheres to a pragmatic subset of
[Keep a Changelog](https://keepachangelog.com/) and semantic versioning.

## [Unreleased]

### Added
- **Trash lifecycle**: `restore` (move a node out of `TRASH`), `empty-trash` (hard-delete every
  trashed node, detaching to `LIMBO` and emitting `ContentOp::Remove` so CIDs are unpinned/GC'd), and
  `gc_trash(now, retention_secs)` honoring `PinPolicy.trash_retention_secs`. Exposed on `Drive` and as
  CLI subcommands `trash`, `restore`, `empty-trash`.
- **Change feed**: `changes_since(cursor, limit) -> (Vec<Change>, next_cursor)` — a unified,
  time-ordered, cursor-paginated delta stream over both op logs (the foundation for incremental client
  sync and an activity feed). CLI `changes [--since lamport:replica] [--limit N]`.
- **Filename / path search**: a case-folded token + trigram inverted index over the live tree
  (substring and prefix matching), with scoped search and a content-text hook (`add_text`). CLI
  `search <query> [--limit N]`.
- **Name validation**: a portable, always-enforced floor (no empty/`.`/`..`/separator/NUL/overlong
  names) at `mkdir`/`add_file`/`mv`, returning typed `NameError`s.
- **Concurrent content-edit conflict copies**: genuine concurrent edits to the same file id that lose
  LWW are flagged and surfaced as Dropbox-style `*.conflict` copies via `Drive::content_conflicts`
  (CLI `conflicts`) instead of being buried in version history.
- **Log compaction**: `Drive::compact` rewrites the op history to a minimal equivalent log, bounding
  state-file size and replay time. CLI `compact`.
- **Streaming, bounded storage I/O**: `Store::put_file` chunks + delta-uploads chunk-by-chunk (peak
  memory ~1 MiB), and `Store::get_file_to_path` fetches + verifies + appends chunk-by-chunk into an
  fsync'd, atomically-renamed temp file. A configurable `max_file_size` (default 16 GiB) DoS bound.
- Runnable example `cargo run -p ce-drive-core --example drive_basics`.
- `docs/architecture.md` describing the shipped two-collection CRDT model, ordering/convergence, and
  the trash/changes/compaction/persistence design.

### Changed
- **Persistence format**: drives are now stored as a compact `<name>.cedrive` (magic + bincode + zstd
  level 3) with **atomic** writes (temp + fsync + rename) and a magic-header integrity check, replacing
  pretty JSON. Legacy `<name>.json` state is transparently migrated on first read.
- **Delta upload robustness**: a missing-chunk probe now distinguishes a genuine 404 from a transient
  transport error, so a network blip never re-uploads an entire file.
- **VFS rename** moves the source onto the destination first and only then trashes any displaced
  occupant, so the destination is never lost in a crash/concurrency window (POSIX overwrite semantics
  without a gap).
- **Incremental content refold**: applying a remote content op re-derives only the one affected key
  instead of re-folding the entire map, keeping a long-lived drive's merge path linear.

### Fixed
- CLI persistence layer (incomplete `.cedrive` codec / `Paths` fields) that previously failed to
  compile; added `encode_state`/`decode_state`, the `legacy_json` field, and atomic `write_atomic`.
- Search ranking now boosts exact-name matches above substring-name matches above path-only matches.

### Tests
- CLI offline integration tests (`init` creates a compact state file; every read command loads it;
  corrupt/missing-state errors are clear) and unit tests for the state codec + atomic write.
- Core tests for trash restore/empty-trash/GC-retention, the change feed (ordering, pagination,
  resumability), search ranking/scoping/content-text, name validation, concurrent-edit conflicts,
  incremental-vs-full refold equivalence, and compaction.

## [0.1.0] — M1 + M2

- Initial core: `DriveTree` move-CRDT, `ContentMap`, chunk/delta `Store`, single-writer `Drive`,
  `ce-cap` sharing, durability policy, on-chain audit reader, and the `ce-drive` CLI.
- Multi-writer `SyncedDrive` over `ce-coord` `Merged`; `ce-drive-mount` VFS + driverless fallback;
  `ce-drive-wasm` in-browser CRDT.
