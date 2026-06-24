# CE Drive — Architecture

CE Drive is a distributed, mesh-native filesystem and Google-Drive / Workspace replacement built
**entirely on CE primitives** (identity, mesh transport, blobs, the on-chain ledger, and the `ce-cap`
capability verifier). It adds **no node endpoints**: everything is app/SDK tier over `ce-coord`,
`ce-rs`, `ce-cap`, and the `rdev` chunk/delta engine.

This document explains the model the code implements. The product vision and milestone sequencing
live in [`design.md`](design.md); this file describes what is *shipped* and *why it converges*.

---

## One core, two collections, two faces

The heart of CE Drive is **`ce-drive-core`**: a pure (I/O-free for the CRDT parts) Rust library that
is the single source of truth for the storage model. Both faces — the cross-platform developer
**mount** (`ce-drive-mount`) and the (in-progress) Drive **web app** — are thin adapters over it. The
same crate compiles to **WASM** (`ce-drive-wasm`) for the in-browser node, with golden-wire parity
tests against the native core.

The model is deliberately split into **two orthogonal CRDT collections**:

| Collection | Crate module | Keyed by | What it owns | Conflict resolution |
|---|---|---|---|---|
| **A. Tree** | `tree.rs` (`DriveTree`) | stable `NodeId` edges | directory structure (parent + name + kind) | Kleppmann move-CRDT: undo/do/redo, cycle-skip, deterministic conflict-rename |
| **B. Content** | `content.rs` (`ContentMap`) | the same `NodeId` | a file's bytes pointer (`cid`, size, mode, mtime) + capped version history | LWW by `(mtime_ms, cid)`, concurrent-edit conflicts surfaced |

**Why two collections?** Because rename and edit must never collide. In a path-keyed design, renaming
`a.md`→`b.md` while another replica saves new bytes to `a.md` is ambiguous. Here, the file has one
**stable `NodeId`**; the tree maps that id to *where it lives* and the content map maps the *same id*
to *what bytes it holds*. A rename is a `MoveOp` in collection A; an edit is a `ContentOp::Set` in
collection B. They are independent, so neither loses the other's update.

### Path is derived, never stored

A node's absolute path is computed by walking parent pointers to `ROOT` (`DriveTree::path`). Moving a
directory is therefore **one O(1) edge flip** — every descendant's path follows automatically with no
per-descendant op. Identity (`NodeId`) is stable across every rename and move.

---

## Ordering and convergence

Every op carries a **`Timestamp { lamport, replica }`** — a Lamport clock tie-broken by the writer's
node-id hex. `(lamport, replica)` is a strict total order across all replicas.

* **Tree (A):** the move log is kept sorted by `Timestamp`. A late op that carries an earlier
  timestamp triggers Kleppmann's **undo → do → redo**: undo the ops after the insertion point (each
  `MoveOp` records the edge it displaced, its exact inverse), apply the new op, then redo the tail
  (each re-runs its cycle check, which may now resolve differently — exactly the point). Same op-set +
  same total order ⇒ identical acyclic tree on every replica, regardless of delivery order.
* **Content (B):** the content log is folded in ascending-`Timestamp` order. Because `FileContent`
  resolution is LWW-by-`(mtime, cid)` and the fold visits ops in one canonical order everywhere, two
  replicas that receive the same content ops in *different arrival orders* converge to byte-identical
  state.

### Cycle safety

`DriveTree::do_move` runs a cycle check before applying: if `new_parent == child` or `child` is an
ancestor of `new_parent`, the move is **skipped** deterministically. This is the core safety
invariant — concurrent "move A under B" / "move B under A" can never create a cycle; the
later-timestamp move wins and the earlier is superseded. The ancestor walk is bounded by the edge
count to defend against any pathological state.

### Conflict surfacing

* **Name collisions** (two live children of a dir sharing a name) render as deterministic
  `"<name>.conflict-<replica>-<lamport>"` copies at `readdir` time — a pure function of the converged
  tree, identical on every replica. Nothing is hidden.
* **Concurrent content edits** (two different cids written to the same node id with an equal-or-newer
  mtime that loses LWW) flag the losing version as a `conflict`, surfaced by
  `Drive::content_conflicts` as a Dropbox-style `*.conflict` copy rather than being buried silently in
  version history.

---

## Storage: chunk / dedup / delta / verify

`store.rs` is the only I/O-bound part of the core. It does **not** reinvent the chunk engine; it is
thin glue over `rdev::chunk` + `rdev::delta` + `ce-rs::data`:

* A file is split into fixed **1 MiB chunks**; each chunk's `sha256` is its CID; an `ce-object-v1`
  manifest lists the chunk CIDs and the manifest's own hash is the **object CID** recorded in the
  content map.
* **Dedup is global and free** — chunks are content-addressed in the shared blob store, so a re-store
  of existing bytes moves nothing.
* **Delta upload** probes which chunk CIDs the store is *missing* (distinguishing a genuine 404 from a
  transient transport error, so a network blip never re-uploads the whole file) and uploads only
  those.
* **Streaming bounds:** `put_file` reads and uploads chunk-by-chunk (peak memory = one chunk, ~1 MiB)
  with an incremental size guard, and `get_file_to_path` fetches, CID-verifies, and appends
  chunk-by-chunk into a temp file that is fsync'd and atomically renamed into place. A multi-GB file
  never OOMs the process and an interrupted download never leaves a corrupt destination. A configurable
  `max_file_size` (default 16 GiB) is a hard DoS / disk-exhaustion bound.

---

## Sharing, durability, audit

* **Sharing** (`share.rs`) is `ce-cap` **attenuating capability chains**, scoped per folder
  (path-prefix), with expiry, on-chain revocation, and a `..`/separator guard so a caveat cannot be
  escaped. There is no member list by design — authorization is always a signed capability chain.
* **Durability** (`durability.rs`) is a per-workspace `PinPolicy` (replication factor, trash
  retention, announce cadence) over the `ce-pin`-style DHT announce/replicate model.
* **Audit v1** (`audit.rs`) reads CE's on-chain capability grant/revoke facts and journals locally so
  `history` can render active/expired/revoked shares (cross-checked against on-chain revocation when a
  node is reachable, journal-only when offline).

---

## Trash lifecycle

`rm` moves a node into the reserved `TRASH` subtree (hidden from live listings, recoverable). From
there:

* **`restore`** moves it back to a live directory (a single `MoveOp`).
* **`empty_trash`** hard-deletes every trashed node: it is detached to a private `LIMBO` parent (never
  listed/resolved/path-derived, but its tombstone move op stays in the log so peers converge on the
  removal) and a `ContentOp::Remove` is emitted so its CIDs become GC candidates (unpinned).
* **`gc_trash(now, retention_secs)`** GCs only nodes whose newest content mtime predates the cutoff,
  honoring `PinPolicy.trash_retention_secs`.

---

## Change feed

`changes.rs` exposes `changes_since(cursor, limit) -> (Vec<Change>, next_cursor)` — a unified,
time-ordered delta stream over both op logs. A **cursor is just the highest `Timestamp` already
delivered**, so it is monotone, replica-independent, and resumable across reconnects. This is the
foundation for efficient incremental client sync and an activity feed (Google Drive's Changes API),
and it is cheap because the op logs already carry a total order.

---

## Compaction

A long-lived single-writer drive's op log grows without bound. `Drive::compact` replaces the full
history with a **minimal equivalent log** (one create op per live node in BFS order so parents precede
children, plus each content record's retained version history) that replays to byte-identical live
state. Trashed/limbo nodes are dropped — compaction is the GC boundary. The multi-writer
`SyncedDrive` has its own `ce-coord` checkpoint path.

---

## Persistence

The CLI stores a drive as a single `<name>.cedrive` file: `magic || bincode(DriveState)` compressed
with **zstd level 3** (the project's persistence standard — deterministic and compact). Writes are
**atomic** (temp file + fsync + rename), and a legacy pretty-JSON `<name>.json` from an earlier version
is transparently migrated on first read. A corrupt/foreign file is rejected by a magic-header check
rather than silently deserialized.

---

## Crate map

```
ce-drive-core/   pure model: tree (A) + content (B) + store + share + durability + audit
                 + changes (feed) + search (filename index) + names (validation)
                 single-writer Drive, multi-writer SyncedDrive (ce-coord Merged), DriveState codec
ce-drive-mount/  OS-independent VFS engine (lazy hydrate, write-back, stable inodes, atomic rename)
                 + driverless materialize/push/watch; Linux fuser adapter (feature-gated, real),
                 macOS FSKit / Windows WinFsp+ProjFS adapters (cfg/feature-gated)
ce-drive-wasm/   I/O-free CRDT compiled to wasm for the in-browser node; golden-wire parity vs core
ce-drive-cli/    the `ce-drive` binary
```
