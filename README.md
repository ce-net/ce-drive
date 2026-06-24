# CE Drive

> Distributed, mesh-native filesystem + open-source Google Drive / Workspace replacement, built
> entirely on CE primitives. **Zero node changes** — pure app/SDK tier over `ce-coord`, `ce-rs`,
> `ce-cap`, and the `rdev` chunk/delta engine.

CE Drive is **one core, two faces**: a content-addressed, dedup'd, naturally-versioned storage model
with a Kleppmann move-CRDT directory tree, presented as (1) a cross-platform developer **mount** and
(2) a Drive/Workspace **web app**. The core compiles to **WASM** for the in-browser node.

See [`docs/architecture.md`](docs/architecture.md) for how the model works and why it converges, and
[`CHANGELOG.md`](CHANGELOG.md) for what is new.

## Crates

```
ce-drive/
├── Cargo.toml                        # workspace + [patch] unifying the git ce-rs/ce-cap onto local paths
└── crates/
    ├── ce-drive-core/                # the pure library (also targets WASM)
    │   ├── src/tree.rs               # DriveTree: Kleppmann move-CRDT as a ce-coord StateMachine (collection A)
    │   ├── src/content.rs            # ContentMap (NodeId -> FileContent) + capped version history (collection B)
    │   ├── src/store.rs              # streaming chunk/dedup/delta-upload/CID-verify over rdev::chunk + rdev::delta
    │   ├── src/share.rs              # ce-cap per-folder grants (read/comment/write/admin), links, on-chain revoke
    │   ├── src/durability.rs         # ce-pin-style replication/announce/status policy per workspace
    │   ├── src/audit.rs              # audit v1: on-chain grant/revoke history reader
    │   ├── src/changes.rs            # cursor-paginated change feed over both op logs
    │   ├── src/search.rs             # filename/path inverted index (token + trigram, scoped)
    │   ├── src/names.rs              # portable name validation (no empty/./../sep/NUL/overlong)
    │   ├── src/drive.rs              # single-writer Drive: tree + content + trash + conflicts + compaction
    │   ├── src/sync.rs               # multi-writer SyncedDrive over ce-coord Merged
    │   └── examples/drive_basics.rs  # runnable, node-free tour of the core API
    ├── ce-drive-mount/               # OS-independent VFS engine + driverless materialize/push/watch
    │   ├── src/vfs.rs                # lazy hydrate, write-back cache, stable inodes, atomic rename
    │   ├── src/linux_fuser.rs        # Linux fuser adapter (feature `fuse`)
    │   ├── src/macos_fskit.rs        # macOS FSKit adapter (cfg/feature-gated)
    │   └── src/windows.rs            # Windows WinFsp / ProjFS adapter (cfg/feature-gated)
    ├── ce-drive-wasm/                # I/O-free CRDT compiled to wasm; golden-wire parity vs core
    └── ce-drive-cli/                 # the `ce-drive` binary
```

## Features

- **Move-CRDT directory tree** — stable `NodeId` edges, **derived paths**, every structural mutation a
  single `MoveOp`; cycle-skip + Kleppmann undo/do/redo give order-independent convergence to one
  acyclic tree. Name collisions render as deterministic `*.conflict-<replica>-<lamport>` copies.
- **Orthogonal content map** — edits are keyed by the same stable id, so rename-vs-edit never collide;
  LWW-by-`(mtime, cid)` with a capped version history (free versioning + restore).
- **Streaming storage** — chunk/dedup/delta over the shared `rdev` engine; `put_file`/`get_file_to_path`
  move data chunk-by-chunk (peak RAM ≈ 1 MiB), downloads are CID-verified + fsync'd + atomically
  renamed, and a configurable `max_file_size` (default 16 GiB) bounds DoS.
- **Trash lifecycle** — recoverable delete, `restore`, `empty-trash` (hard-delete + unpin CIDs), and
  retention-honoring GC.
- **Change feed** — `changes_since(cursor)` cursor-paginated delta stream (incremental sync + activity).
- **Search** — case-folded filename/path index (substring + trigram), scoped to a shared subtree.
- **Conflict copies** — genuine concurrent content edits surface as Dropbox-style `*.conflict` copies.
- **Sharing** — `ce-cap` attenuating, path-scoped, expiring, on-chain-revocable grants and share links.
- **Compaction** — rewrite the op log to a minimal equivalent, bounding state size and replay time.
- **Cross-platform mount** — a real Linux FUSE mount (feature `fuse`) plus a driverless
  materialize/push/watch fallback that works everywhere (CI, containers, no-admin machines).

## CLI

```bash
ce-drive init                              # create a drive owned by this device
ce-drive add ./report.pdf /docs/report.pdf # chunk + store on the local node (mkdir -p implied)
ce-drive ls /docs                          # list a folder
ce-drive tree                              # whole-drive tree view
ce-drive mv /docs/report.pdf /archive/r.pdf
ce-drive rm /archive/r.pdf                 # move to trash (recoverable)

ce-drive trash                             # list trashed (recoverable) nodes
ce-drive restore <node-id> --to / --name r.pdf
ce-drive empty-trash                       # hard-delete all trash (unpin CIDs)
ce-drive empty-trash --older-than 604800   # GC only trash older than 7 days

ce-drive search "report" --limit 20        # filename/path search
ce-drive changes --since 42:<replica-hex>  # delta feed since a cursor (omit --since for all history)
ce-drive conflicts                         # files with unresolved concurrent-edit conflicts
ce-drive compact                           # shrink the persisted op log

ce-drive share /docs --to <node-hex> --ability write --expires-days 30
ce-drive share /pub  --link --link-holder <key-hex> --ability read   # anyone-with-link
ce-drive history                           # sharing audit (on-chain revocation cross-checked)

ce-drive materialize ./out                 # driverless: fetch the whole drive into a real directory
ce-drive push ./out --watch                # driverless: sync local edits back (poll-based watcher)
ce-drive mount ./mnt                        # real FUSE mount (Linux, build with --features fuse)
```

Storage commands (`add`, `materialize`, `push`, `sync`, `history`) talk to a local CE node (`--node`,
default `http://127.0.0.1:8844`). Structural and query commands (`ls`, `tree`, `mv`, `rm`, `trash`,
`restore`, `empty-trash`, `search`, `changes`, `conflicts`, `compact`, `share`) work fully offline
against the on-disk drive state (`$CE_DRIVE_DIR/<name>.cedrive`, a compact bincode+zstd file written
atomically; a legacy `<name>.json` is migrated transparently).

## Build & test

> The dev laptop disk is shared and full — heavy compilation runs on the Hetzner build box via
> `tools/remote-test.sh ce-drive --clippy` (rsync + `cargo test` + clippy on a real toolchain). Run
> locally only if you have the disk for it.

```bash
cargo build                                 # workspace
cargo test                                  # unit + integration; the live-node test skips without a node
cargo run -p ce-drive-core --example drive_basics   # node-free tour of the core API

# Run the storage round-trip against a live node:
CE_NODE_URL=http://127.0.0.1:8844 cargo test -p ce-drive-core --test live_store -- --nocapture

# Real Linux mount (libfuse required):
cargo build -p ce-drive-cli --features fuse
```

## Design & roadmap

The product vision and milestone sequencing (two faces, web app, E2E encryption, collaborative
`.cedoc` via embedded ce-notes, previews, quota/billing) live in [`docs/design.md`](docs/design.md).
The shipped model is described in [`docs/architecture.md`](docs/architecture.md).

### Deferred (real slices shipped, full feature later)
- **Drive web app** (`ce-drive serve` + React on the wasm CRDT) — the core, change feed, and search
  APIs that back it are shipped; the HTTP server and frontend are the remaining work.
- **Full-text content search** — the filename/path index ships with an `add_text` content hook; an
  automatic text-extraction/indexing pipeline over fetched blobs is deferred.
- **Thumbnails / previews, at-rest E2E encryption envelope, collaborative `.cedoc`, quota/billing** —
  reserved hooks exist (`doc_id`, durability rent); the full implementations are future milestones.
- **macOS FSKit / Windows WinFsp+ProjFS** kernel adapters are cfg/feature-gated and compile-checked on
  their target OS; only the Linux FUSE adapter is exercised end to end in CI today.
