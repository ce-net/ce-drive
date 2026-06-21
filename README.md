# CE Drive

> Distributed filesystem + open-source Google Drive / Workspace replacement, built entirely on CE
> primitives. **Zero node changes** — pure app/SDK tier over `ce-coord`, `ce-rs`, `ce-cap`, and the
> `rdev` chunk/delta engine.

CE Drive is **one core, two faces**: a content-addressed, dedup'd, naturally-versioned storage model
with a Kleppmann move-CRDT directory tree, presented as (1) a cross-platform developer mount and (2)
a Drive/Workspace web app. This repo currently ships the **core** (`ce-drive-core`) and a **CLI**
(`ce-drive`) — milestones **M1** and **M2**.

## What's here (M1 + M2)

```
ce-drive/
├── Cargo.toml                        # workspace + [patch] unifying the git ce-rs/ce-cap onto local paths
└── crates/
    ├── ce-drive-core/                # the pure library (also targets WASM)
    │   ├── src/tree.rs               # DriveTree: Kleppmann move-CRDT as a ce-coord StateMachine
    │   ├── src/content.rs            # RMap-style content map (NodeId -> FileContent) + version lists
    │   ├── src/store.rs              # chunk/dedup/delta-upload/CID-verify over rdev::chunk + rdev::delta + ce-rs::data
    │   ├── src/share.rs              # ce-cap per-folder grants (read/comment/write/admin), share links, revoke
    │   ├── src/durability.rs         # ce-pin-style replication/announce/status policy per workspace
    │   ├── src/audit.rs              # audit v1: on-chain grant/revoke history reader
    │   ├── src/drive.rs              # the single-writer Drive handle composing tree + content + store
    │   └── tests/live_store.rs       # gated single-node storage round-trip (skips w/o a node)
    └── ce-drive-cli/                 # the `ce-drive` binary
        └── src/main.rs               # init | add | ls | tree | mv | rm | history | share | sync
```

### M1 — core: tree CRDT + content + storage
- **`DriveTree`** — the move-CRDT (`tree.rs`). Stable `NodeId` edges; **path is derived**, never
  stored as identity. Every structural mutation (create / delete / rename / **move dir**) is a single
  `MoveOp`. `apply` runs the **cycle-skip** (`is_ancestor`) and Kleppmann **undo/do/redo** so any op
  delivery order converges to **one acyclic tree**. A derived `children` index gives O(k log k)
  `readdir`; name collisions are surfaced as deterministic `*.conflict-<replica>-<lamport>` copies
  (never hidden). Implemented as a `ce_coord::StateMachine`, so the single-writer v1 promotes to the
  v3 merged-log multi-writer path with no model change.
- **`ContentMap`** — `NodeId -> FileContent` (`content.rs`), orthogonal to the tree. Editing bytes is
  an `Insert` keyed by the same stable id, so rename-vs-edit never collide. LWW per key with a capped
  version history (every save is a new CID; old CIDs stay valid → free versioning + restore).
- **`store.rs`** — chunks bytes with `rdev::chunk` (1 MiB chunks, sha256 CIDs, `ce-object-v1`
  manifest), delta-uploads only the missing chunks (`rdev::delta`), records the **object CID** so
  `get_object` resolves and verifies every chunk. Global dedup is free.

### M2 — sharing, durability, audit v1
- **`share.rs`** — per-folder `ce-cap` grants scoped by a `path_prefix` subtree caveat; read/comment/
  write/admin; share links (read/comment only) with expiry; transitive read-only re-share enforced by
  attenuation; on-chain `RevokeCapability` via `(issuer, nonce)`. Reuses `ce_cap::authorize` verbatim
  (the rdev `handle_inner` discipline) plus a `..`-traversal guard.
- **`durability.rs`** — per-workspace `PinPolicy` (replication factor, trash retention, announce) over
  the DHT announce/find primitives `ce-pin` uses.
- **`audit.rs`** — audit v1 joins a local grant journal with the live on-chain revoked-capability set
  (`GET /capabilities/revoked`), so "who was granted/revoked what, when" is tamper-evident.

## CLI

```bash
ce-drive init                              # create a drive owned by this device
ce-drive add ./report.pdf /docs/report.pdf # chunk + store on the local node (mkdir -p implied)
ce-drive ls /docs                          # list a folder
ce-drive tree                              # whole-drive tree view
ce-drive mv /docs/report.pdf /archive/r.pdf
ce-drive rm /archive/r.pdf                 # move to trash (recoverable)
ce-drive share /docs --to <node-hex> --ability write --expires-days 30
ce-drive share /pub  --link --link-holder <key-hex> --ability read   # anyone-with-link
ce-drive history                           # sharing audit (on-chain revocation cross-checked)
ce-drive sync ./out                        # materialize the whole drive (CID-verified)
```

Storage commands (`add`, `sync`, `history`) talk to a local CE node (`--node`, default
`http://127.0.0.1:8844`). Structural commands (`ls`, `tree`, `mv`, `rm`, `share`) work fully offline
against the on-disk drive state (`$CE_DRIVE_DIR/<name>.json`).

## Build & test

```bash
cargo build            # workspace
cargo test             # 45 tests; the live single-node test skips gracefully without a node
# Run the storage round-trip against a live node:
CE_NODE_URL=http://127.0.0.1:8844 cargo test -p ce-drive-core --test live_store -- --nocapture
```

## Design

The full design (two faces, mount layer, web app, E2E encryption, collaborative `.cedoc` via embedded
ce-notes, multi-writer, search/previews, and the v1→v3 milestone sequencing) is in
`PLAN/10-drive-fs.md` at the workspace root. This repo implements its **v1.M1 + v1.M2**.

### Not yet built (later milestones, marked in the design)
- **M3** Drive web app (React on `@ce-net/sdk`, ce-drive-core → WASM).
- **M4/M5** the cross-platform mount (`ce-drive-mount`: fuser / FSKit / WinFsp / ProjFS) + the
  driverless `materialize` mount fallback, perf hardening.
- **M6** collaborative `.cedoc` docs (embedded ce-notes), optional E2E envelope, multi-writer via the
  ce-coord merged-log mode (the dormant undo/redo activates).
- **M7** full-text search, thumbnails/previews, per-object audit log v2 (root-CID checkpoint onto the
  chain), quota/billing.
