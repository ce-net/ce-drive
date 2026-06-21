# CE Drive — Distributed FS + Open-Source Google Drive/Workspace Replacement

> Workstream: ce-drive — distributed FS + open-source Google Drive/Workspace replacement.
> Repo: `github.com/ce-net/ce-drive` (new, sibling to `rdev`, `ce-notes`, `ce-pin`, `swarm`)
> Layering: `CE Drive → {ce-coord, ce-rs, ce-cap, rdev::chunk/delta, ce-notes, ce-pin} → local CE node :8844 → mesh`
> Node changes: **none**. ce-coord additions: 3, all additive and already on its roadmap.

CE Drive is the capstone app that ties together everything CE has built. It is **two products over one core**:

1. **A developer filesystem** — mountable cross-platform, content-addressed, dedup'd, naturally versioned (git/IPFS-grade), so you can `cd` into a CE drive and run `cargo build`, `git status`, an editor's file watcher.
2. **An open-source Google Drive + Workspace** — web UI with folders, sharing, real-time collaborative docs, and an enterprise audit trail.

Both faces are pure policy/UX over CE primitives. CE Drive **composes** existing pieces — it does **not** reinvent the chunk engine (reuses `rdev::chunk`/`delta`/`ce-rs::data`), the directory model (reuses ce-coord `StateMachine`), or the doc CRDT (embeds `ce-notes`/yrs), and it does **not** propose a single new node endpoint.

---

## 1. Goals / Non-Goals

### Goals
- **One core, two faces.** A pure-Rust `ce-drive-core` library is the single source of truth for the storage model, the directory CRDT, sharing, and conflict handling. The mount layer and the web app are thin adapters over it.
- **Content-addressed, dedup'd, versioned storage.** Files are CDC-/fixed-chunked content-addressed blobs (sha256 CIDs via `ce-rs::data`), dedup'd globally, made durable by `ce-pin`. Immutable blocks ⇒ every save is a new CID ⇒ version history is free.
- **A real, mountable developer FS.** Lazy hydration + write-back caching tuned so dev tooling (metadata-latency-bound, small-file-storm) is usable; a driverless `materialize` fallback for CI/containers/locked-down machines.
- **A Drive-grade web app.** Folder browser, upload/download, share links + per-folder read/write/admin permissions, trash, versions, collaborative docs, search, previews, audit.
- **Capability-based sharing.** Per-folder, attenuated, revocable `ce-cap` chains; the org root key *is* the workspace.
- **Collaborative docs by composition.** Embed `ce-notes` (yrs CRDT) as the "Google Docs" layer; a `.cedoc` file in the tree is a NodeId-addressed ce-notes document.
- **Optional end-to-end encryption** for private workspaces, with dedup preserved (KBFS-style deterministic nonces).
- **Enterprise/HIPAA-friendly audit** via CE's immutable on-chain interaction history + per-object access log.
- **Mesh-first, no allowlists.** All authorization is signed attenuating capability chains; all transport is mesh AppRequest/pubsub + the blob layer.

### Non-Goals (and where they land)
- **No new node RPCs/endpoints.** Every transport is `ce-rs` `put_blob`/`get_blob`/`request`/`reply`/`publish` + ce-coord. (Same discipline as `rdev`/`ce-pin`/`ce-notes`.)
- **No reinvented chunk engine.** Reuse `rdev::chunk` + `rdev::delta` + `ce-rs::data` verbatim.
- **No reinvented doc CRDT.** Embed `ce-notes` for collaborative documents; CE Drive owns only the *file/folder* tree, not the prose CRDT.
- **No strong cross-machine FS coherence in v1.** Close-to-open consistency (NFS-style), the correct tradeoff for a dev mount; opt-in stronger coherence is future.
- **No Sheets formula engine / Slides in v1–v2.** v3+, app-tier, out of this doc's core scope.
- **Not a block-level WYSIWYG in v1.** Markdown + attachments via ce-notes; block model follows ce-notes.

---

## 2. The two faces over one core

```
                       ┌──────────────────────────────────────────────────────────┐
                       │                  ce-drive-core (pure Rust)                │
                       │  • DriveTree: Kleppmann move-CRDT  (StateMachine/ce-coord)│
                       │      nodes keyed by stable NodeId; edges (parent,name)    │
                       │      path -> {CID,size,mtime,mode,kind}                    │
                       │  • Content map: RMap<NodeId, FileContent{cid,size,...}>    │
                       │  • Chunk/delta engine  (rdev::chunk + rdev::delta)        │
                       │  • Durability           (ce-pin add/announce/status)      │
                       │  • Sharing/permissions  (ce-cap chains, per-folder)       │
                       │  • E2E envelope (optional, per-workspace)                 │
                       │  • Audit reader (CE on-chain history + per-object log)    │
                       │  • Embedded docs (ce-notes handle, by NodeId)             │
                       └───────────────┬───────────────────────────┬──────────────┘
            ┌──────────────────────────┘                           └───────────────────────┐
            ▼                                                                                ▼
  FACE 1: DEV MOUNT (ce-drive-mount)                                  FACE 2: DRIVE WEB APP (ce-drive-web)
  ┌───────────────────────────────────┐                              ┌────────────────────────────────────┐
  │ fuser (Linux) / macFUSE-FSKit     │                              │ @ce-net/sdk (JS port of ce-rs)      │
  │ (macOS) / WinFsp|ProjFS (Windows) │                              │ React file browser + share dialog   │
  │ lazy hydrate + write-back cache   │                              │ CodeMirror+Yjs doc editor (ce-notes)│
  │ stable inodes, readdirplus        │                              │ previews/thumbnails, search, audit  │
  │ + `ce-drive materialize` fallback │                              │ browser CE node (ce-net.com/node)   │
  └───────────────────────────────────┘                              └────────────────────────────────────┘
```

The mount and the web app are **views of the same DriveTree + content store**. A file created in the web UI appears in the mount (after close-to-open re-read); a file written through the mount appears in the web UI. They differ only in *how they touch the core*: the mount translates VFS syscalls into core calls; the web app translates HTTP/SDK calls into the same core (compiled to WASM in the browser node, or talking to a local helper).

---

## 3. Storage model — files = content-addressed chunked blobs

CE Drive uses the dominant, validated prior-art pattern (IPFS/Git/Upspin/Seafile/KBFS): **an immutable content-addressed block store with a thin mutable naming layer on top.** It does not build a new one — it reuses the engine `rdev` already ships.

### 3.1 Bytes (the immutable layer) — reuse `rdev::chunk` + `ce-rs::data`
- A file's bytes are split by **`rdev::chunk` / `ce-rs::data::chunk_object`** (fixed-size 1 MiB chunks today; the code is already the shared substrate for rdev/Notes/ce-pin, and CDC is its noted future boundary upgrade).
- Each chunk → `sha256` CID; the ordered chunk list + sizes is an `ce-object-v1` **Manifest** whose own hash is the **file CID**. This is exactly the node's `/blobs` keying, so dedup is global and free (identical chunks/files across the org store once).
- Chunks travel via `put_blob`/`get_blob`; the mesh fetch-by-hash + per-chunk CID verification (data-layer Stage 2) means **content-addressing IS the integrity proof** — a host can never serve bytes the publisher didn't store.
- **Delta transfer** uses `rdev::delta`: on write, only chunk CIDs the receiver lacks are uploaded (the `have`/`commit` negotiation already specced in `05-autosync.md`). A 1-byte edit in a 500 MB file ships one chunk.

### 3.2 Durability / availability — `ce-pin`
- Best-effort blob caching is not durability. Each file CID (and each directory snapshot CID) is pinned via **`ce-pin add <cid> --replication N`** and announced on the DHT (`ce-pin announce`). `ce-pin status --audit` gives proof-of-retrievability.
- A **workspace pin policy** (app-tier) decides replication factor and which hosts (paid rent via payment channels, or self-hosted org nodes). Deleting/trashing a file unpins after the retention window.

### 3.3 Versioning — natural, from immutability
- Immutable blocks ⇒ **every save produces a new file CID; the old CID remains valid**. The directory CRDT keeps an ordered list of prior `(version_ts, cid, size)` per node (capped/prunable). "Restore version" = point the node's content entry back at an old CID (a new CRDT op). Pinning policy keeps the last K versions retrievable.
- This is Seafile's per-library snapshot model and Git's "old objects stay" property, for free.

---

## 4. The directory-tree CRDT — Kleppmann move-op over ce-coord

The tree is the *hard* half (the content layer is easy). A folder tree under concurrent edit is the one structure where naive CRDTs break correctness: concurrent **moves** can create cycles. CE Drive implements **Kleppmann's highly-available move CRDT** as a ce-coord `StateMachine` — the single most important design decision, taken directly from the crdt-tree research.

### 4.1 Why not `RMap<path, entry>`
Keying by **path** is wrong: directory move is O(subtree) and non-atomic, files have no identity across renames, cycles are invisible, and concurrent structural intent is lost. The universal fix: give every node a **stable globally-unique `NodeId`** and store the tree as a set of `(child_id → {parent_id, name, meta})` edges. **Path is derived** by walking parent pointers, never stored as identity.

### 4.2 The model (one op type)
```rust
// ce-drive-core::tree
struct MoveOp {
    ts: (Lamport, ReplicaId),   // total-order key (lamport, tiebreak replica)
    child: NodeId,
    new_parent: NodeId,         // ROOT, TRASH, or a dir node
    new_name: String,
}
enum NodeMeta { Dir, File { cid: String, size: u64, mode: u32, mtime_ms: u64 } }
```
**Create** = move a fresh node out of limbo; **delete** = move into reserved `TRASH`; **rename** = move to same parent with new name; **move dir** = one O(1) edge flip (all descendants follow because path is derived). Every structural mutation is a single `Move` — one op, one conflict story.

`apply(Move)` runs the **cycle check**: if `new_parent == child` or `is_ancestor(child, new_parent)`, **skip** the move (deterministic given op order). `is_ancestor` is O(depth) (trees are shallow). A derived `children: HashMap<parent_id, BTreeSet<(name,id)>>` index gives O(k log k) `readdir`.

### 4.3 Convergence: undo / do / redo
Out-of-order delivery means a late move can arrive with an earlier timestamp. Kleppmann's fix: keep the log sorted by timestamp; on a late op, **undo** ops after its insertion point (each `Move` stores the `old_parent` it displaced = its exact inverse), **do** it, **redo** the rest (each re-runs its cycle check). Same op set + same total order ⇒ identical acyclic tree everywhere. The A→B / B→A cycle resolves to "later-timestamp move loses," identically on every replica.

### 4.4 Two collections per drive
- **(A) Structure log** — `DriveTree: StateMachine` whose `Op = MoveOp`. The ordered move log *is* the tree CRDT.
- **(B) Content map** — `RMap<NodeId, FileContent{cid,size,mode,mtime_ms,[doc_id]}>`, keyed by **stable NodeId, not path.** This orthogonality is decisive: renaming `a.md` (a `Move` in A) while editing its bytes (an `Insert` in B keyed by the same id) **never collide.** Path is a pure function of (A): `path(node) = walk edges to ROOT, join names`.

| User action | Structure log (A) | Content map (B) |
|---|---|---|
| create file | `Move{new_id, parent, name}` | `Insert(new_id, {cid,...})` |
| edit bytes | — | `Insert(id, {new_cid,...})` (LWW + conflict-copy) |
| rename / move file | `Move{id, ...}` | — (id stable) |
| **move dir** | **one** `Move{dir_id, new_parent}` | — (atomic, O(1)) |
| delete | `Move{id, TRASH}` | (content kept until GC → undelete) |

`FileContent.doc_id` (optional) points a `.cedoc` file at an embedded ce-notes document (§7).

### 4.5 Name collisions & conflict surfacing
The CRDT can legitimately hold two nodes with the same name in a dir (distinct ids) — a filesystem-illegal state that must be **surfaced, not hidden**. Default policy (deterministic, same on every replica): later-ts node keeps the name, loser is presented as `a.conflict-<replica>-<ts>.md`. This is a pure render-time function of the converged tree (no extra ops), mirroring `05-autosync.md`'s `.conflict-…` naming. Concurrent **delete + edit** → edit resurrects (content in B keyed by a still-valid id is unambiguous; nothing lost since TRASH retains content until GC).

### 4.6 Single-writer v1, multi-writer v3
- **v1 (single-writer): zero ce-coord changes.** One "primary" device/server owns the log; others are readers. The writer emits ops in timestamp order, so undo/redo never triggers (append-only). Implement `DriveTree: StateMachine` and you're done.
- **v3 (multi-writer): per-replica logs merged by Lamport ts** (the genuine CRDT path — better than Raft, which would stall offline moves). Each writer owns its log (`ce-coord/log/<writer-id>/drive-<id>`); a reader subscribes to all authorized writers and merges by `MoveOp.ts`, running the dormant undo/redo. **No leader, no quorum.** The undo/redo lives in `apply`, not in ce-coord.

### 4.7 Encryption-at-rest of the tree
For private workspaces (§9), `MoveOp` and `FileContent` are encrypted app-side (XChaCha20-Poly1305 under the workspace key) before `propose`, identical to ce-notes' opaque-op envelope. ce-coord ships opaque bytes; CE never sees plaintext tree structure.

---

## 5. Sharing + permissions — `ce-cap`, org root = workspace

Sharing is **attenuating, revocable capability chains** (`ce-cap`), CE's only authorization primitive. No ACL server, no device allowlists.

- **The org root key *is* the workspace.** A workspace = a root key (self-hosted org node's key, or a configured `roots/` key for enterprises). Every grant chains to it. Membership = the set of issued capabilities, not a list.
- **Per-folder abilities** are opaque strings, scoped by a `path_prefix`/`subtree:<NodeId>` caveat: `drive:read`, `drive:write`, `drive:admin`. Granting `drive:write` on `subtree:<docs_id>` lets a node mutate the DriveTree only under `docs/` (the same `path_prefix` caveat rdev already enforces in `fs_action`).
- **Attenuation = transitive read-only sharing (Tahoe-style).** A holder of `drive:read` on a folder can re-share a strictly-attenuated `drive:read` of a sub-folder, never widen to write. The capability chain enforces it cryptographically.
- **Share links** = a minted `drive:read` (or `:comment`) capability scoped to a node CID, optionally `--expires`. "Anyone with link" = a capability embedded in the URL fragment.
- **Revocation** = on-chain `RevokeCapability` (the node's revoked-set, already refreshed by rdev/ce-pin) + expiry. For E2E workspaces, revocation also rotates the per-folder key and re-wraps to survivors (KBFS lazy rekey, §9).
- **Enforcement.** Two layers: (1) writers only *apply* DriveTree/content ops from authorized writers (app policy over ce-coord's free `from`-verification); (2) any host serving the durable blobs / a `ce-drive serve` endpoint runs `ce_cap::authorize(host_id, roots, …, action, &chain, &is_revoked)` exactly as rdev's `handle_inner` does today, reusing the verifier verbatim.

---

## 6. Collaborative docs — embed `ce-notes`

CE Drive does **not** build a doc CRDT. A collaborative document is a **`.cedoc` node in the tree whose `FileContent` carries a `doc_id` (an embedded ce-notes document id) instead of (or alongside) a blob CID.**

- Opening a `.cedoc` in the web editor mounts the ce-notes yrs `Y.Doc` (CodeMirror + Yjs binding), syncing via ce-notes' `MergeSet` over ce-coord — multi-writer, offline-first, convergent prose with no conflict UI, exactly as designed in `04-notes-app.md`.
- The Drive **tree** owns the *file's identity, name, location, sharing* (the `.cedoc` node); ce-notes owns the *prose CRDT*. Two orthogonal CRDTs composed: move/rename the doc in Drive (a `MoveOp`) while two people co-edit its text (ce-notes ops) — no interference, because content is keyed by the stable NodeId.
- **Sharing reuse:** a Drive `drive:write` capability on the folder is the authorization to be a ce-notes writer for docs in it; the workspace key wraps the ce-notes space key. One sharing model, two CRDTs.
- **Attachments inside docs** ride the same blob layer (`put_object`/`get_object` by `{cid,key}`), shared with Drive's file storage.
- Comments / suggestions / presence = ce-notes / ce-coord `Stream<T>` features, inherited.

This makes CE Drive the "files + folders + sharing + mount" face and ce-notes the "live documents" face of **one** workspace.

---

## 7. The cross-platform MOUNT layer (Face 1)

A real kernel mount so a developer can `cd` in and build. Dev tooling is **metadata-latency-bound** and produces **small-file storms** — naïve distributed POSIX FS (JuiceFS's own docs admit this) are bad at `git`/`cargo` until you cache metadata hard and avoid an RPC per syscall. CE Drive is designed around that.

### 7.1 One core, three thin adapters + a fallback
```
ce-drive-core  ──►  ce-drive-mount  ──►  { fuser | macFUSE-FSKit | WinFsp/ProjFS }
                                    └──►  ce-drive materialize (no-driver fallback)
```
- **Linux:** `fuser` (libfuse ABI, no libfuse link) mounted `-o writeback_cache -o readdirplus`, long attr/entry TTLs. Primary build/validation target.
- **macOS:** **macFUSE FSKit backend** (`-o backend=fskit`, mount under `/Volumes/CEDrive/<drive>`) — user-space, no kext, no Recovery dance; kext backend as opt-in "fast mode."
- **Windows:** **WinFsp** (`winfsp-rs`) for parity; **ProjFS mode** (hydrate-once-then-native, the VFS-for-Git playbook) as a high-perf option for large/monorepo trees.

### 7.2 Lazy hydration (read path)
1. `readdir`/`lookup`/`getattr` served from a **locally-cached manifest** = the DriveTree's `children` index → names → file CID/size/mode/mtime. Listing never downloads bytes. Long attr/entry TTLs (the single biggest perf lever) stop the kernel calling back per `stat`. Implement **`readdirplus`** to kill the readdir+N×getattr storm.
2. First `read` of a file → fetch its chunks **by CID, range/block-wise** (`get_blob` per chunk, mesh fetch-by-hash, CID-verified) into a local content-addressed cache (`~/.cache/ce/drive/blocks/`). Opening a 2 GB file and reading the header pulls one chunk, not 2 GB.
3. Subsequent reads hit the local cache (or, in ProjFS mode, the now-real NTFS file) with no network.

### 7.3 Write-back (write path)
1. Writes buffered locally (FUSE `writeback_cache` coalesces small writes). 
2. On `fsync`/`release`/idle: re-chunk dirty data via `rdev::chunk`, upload only **missing** chunks (`rdev::delta`), update the content map (B) → new file CID → a `MoveOp`/`Insert`. `ce-pin` picks up the new CID.
3. **Async upload + back-pressure:** don't block `close()` on network; surface "syncing." Don't turn every editor `fsync` into a synchronous upload (flush to durable local cache, upload async).

### 7.4 The perf checklist (baked into `ce-drive-core`)
Stable inodes across the mount lifetime (or `make`/`cargo` spuriously rebuild); monotonic mtimes; atomic `rename` (every editor's write-temp→rename); `readdirplus` + sibling prefetch on `readdir`; generous attr/entry TTLs; range/block lazy hydration; case-insensitivity + reserved-name (`CON`/`aux`) + path-length normalization on macOS/Windows; an optional `--warm`/prefetch to pay cold-start cost upfront; change notifications surfaced from the manifest layer for IDE watchers.

### 7.5 The driverless `materialize` fallback (always shipped)
For CI runners, containers without `/dev/fuse`, no-admin machines:
- `ce-drive materialize <drive> <path>` — walk the DriveTree manifest, `get_object` every blob into a real local directory. Native-speed, works everywhere, no driver.
- `ce-drive push <path> [--watch]` — re-chunk changed files into CE blobs, update the tree (this is literally the `rdev syncd` engine pointed at a CE Drive). Gives "materialize → edit/build natively → sync back."
- **Default in CI/containers** (predictable, native-speed, no privileges); the lazy mount is the default on developer workstations.

### 7.6 Coherence
Reads/writes against the durable store ride CE's AppRequest/stream/blob + `ce-cap` as an **app**, never a node RPC. **Close-to-open consistency** by default (you see a peer's writes after they flush and you re-open); strong cross-machine coherence is expensive and opt-in.

---

## 8. The web UI (Face 2) — on `@ce-net/sdk`

A Drive-like web app, the open-source Google Drive + Workspace.
- **Stack:** React on `@ce-net/sdk` (the JS port of ce-rs; produced/confirmed by the ce-notes M6 work — a thin fetch mirror). Runs against a local CE node or the **in-browser CE node** (`ce-net.com/node`, already shipped) with `ce-drive-core` compiled to **WASM** for the tree CRDT + envelope.
- **File browser:** folder tree + list (driven by the DriveTree `children` index), upload (chunk → `put_blob` → content map op), download (`get_object`, CID-verified), drag-move (a `MoveOp`), rename, trash/restore (TRASH node), version history (CID list), copy (dedup via shared CIDs).
- **Share dialog:** mints a `ce-cap` grant (read/comment/write/admin, scoped to the folder NodeId, `--expires`), produces a share link or per-user grant; revoke = on-chain `RevokeCapability` (+ rekey for E2E). Transitive read-only re-share is enforced by attenuation.
- **Doc editor:** CodeMirror 6 + Yjs binding over the embedded ce-notes document for `.cedoc` nodes; comments, presence (ce-coord `Stream<T>`).
- **Audit view** (§10), **search + previews** (§9 below).

### 8.1 Full-text search + previews/thumbnails (app-tier)
CE has no search/thumbnail primitive — both are honest app-tier work, scoped to v3:
- **Metadata/filename search (v1):** a per-workspace index over the DriveTree (`children` + content map). Instant, local.
- **Full-text search (v3):** a per-workspace inverted index (e.g. tantivy in `ce-drive-core`, or an indexer node) built from decrypted content on an authorized device; for E2E workspaces the index is built client-side and itself stored encrypted (search never leaves plaintext to the mesh).
- **Thumbnails/previews (v3):** a thumbnailer produces derived blobs (own CIDs) on first access; inline preview for sharees uses the blob layer's native **range/partial fetch** (verifiable partial reads). PDFs/images/video stream by range.

---

## 9. Offline + conflict surfacing + E2E encryption

### 9.1 Offline
Local-first by construction: the DriveTree reader has a local replica; the content cache is content-addressed (inherently offline-friendly). Edits/moves queue in the writer log; on reconnect, ce-coord catch-up fills gaps. The mount and `materialize` both work fully offline against the local cache.

### 9.2 Conflict surfacing (two regimes)
- **Structural** (tree) → resolved by the move-CRDT; move-cycles resolve silently (loser superseded, surfaced as a notification), name collisions become visible `.conflict-…` copies (never hide data).
- **File content** (concurrent byte edits) → per-file policy mirroring `05-autosync.md`: **LWW default** (mtime, cid tiebreak) but the loser is **always** written as `<name>.conflict-<node>-<ts>` (LWW never destroys bytes); **conflict-copy** opt-in; **CRDT-merge** for registered text/`.cedoc` types via the shared ce-notes `TextDoc` engine (converges, no copy). Three-tree reconcile (local / remote / last-synced, Dropbox-Nucleus) computes minimal sync plans.

### 9.3 End-to-end encryption (optional, per-workspace)
For private workspaces, reuse the **ce-notes envelope verbatim**:
- One **per-folder content key** (XChaCha20-Poly1305), wrapped per member device via X25519-from-Ed25519 sealed box. The workspace owner generates and wraps; sharing re-wraps to the invitee.
- **Dedup survives encryption (KBFS):** derive each chunk's nonce deterministically (HMAC of per-folder key + plaintext chunk hash) so identical plaintext → identical ciphertext CID → cross-user dedup without leaking, and a host can't equivocate by serving the same ciphertext under two CIDs.
- **MoveOp/FileContent encrypted** before `propose`; CE sees only ciphertext + authenticated `from`. Tree structure is private.
- **Revocation = lazy rekey:** bump the per-folder key generation, re-wrap to survivors, new writes use the new key; old blocks stay under old generations (no history re-encryption) + on-chain `RevokeCapability`.
- Non-E2E (org-internal) workspaces skip the envelope for native dedup/search ergonomics; the choice is per-workspace policy.

---

## 10. Audit log — CE on-chain interaction history (HIPAA/enterprise-friendly)

- **Capability grants/revokes are on-chain facts** (TrustGrant / `RevokeCapability`) — an immutable, tamper-evident record of *who was granted/revoked access to what, when*. The audit view reads `GET /history/:node_id` + the chain.
- **Per-object access log (v2):** every `ce-drive serve` access (read/write/share) emits a signed, append-only audit op (a small ce-coord log or a per-object claim log), anchored periodically by checkpointing a root CID onto the chain so the server can't silently roll back or equivocate (KBFS Merkle-tree property; the mutable head is rollback-resistant).
- This gives the immutable, exportable audit trail enterprise/HIPAA buyers require, with cryptographic non-repudiation — a property a centralized Drive cannot offer.

---

## 11. Repo / directory layout

```
ce-drive/                              # github.com/ce-net/ce-drive
├── Cargo.toml                         # workspace
├── crates/
│   ├── ce-drive-core/                 # PURE library — the single core (also compiles to WASM)
│   │   ├── tree.rs                    # DriveTree: Kleppmann move-CRDT as a ce-coord StateMachine
│   │   ├── content.rs                 # RMap<NodeId, FileContent>; version lists
│   │   ├── store.rs                   # chunk/delta over rdev::chunk + rdev::delta + ce-rs::data
│   │   ├── durability.rs              # ce-pin policy (replication, announce, audit)
│   │   ├── share.rs                   # ce-cap minting/authorize, per-folder caveats
│   │   ├── crypto.rs                  # optional E2E envelope (XChaCha20 + X25519, det. nonce)
│   │   ├── docs.rs                    # embedded ce-notes handle (.cedoc)
│   │   ├── audit.rs                   # on-chain history reader + per-object access log
│   │   └── reconcile.rs              # three-tree local/remote/synced planner, conflict policy
│   ├── ce-drive-mount/                # FACE 1 — the mount binary
│   │   ├── vfs.rs                     # shared inode/attr/hydrate/writeback engine
│   │   ├── linux_fuser.rs             # fuser adapter
│   │   ├── macos_fskit.rs             # macFUSE FSKit adapter (kext fast-mode)
│   │   ├── windows_winfsp.rs          # WinFsp adapter
│   │   ├── windows_projfs.rs          # ProjFS hydrate-once mode
│   │   └── materialize.rs             # driverless fallback (materialize / push / --watch)
│   └── ce-drive-cli/                  # `ce-drive` binary (mount, materialize, share, ls, ...)
├── ce-drive-web/                      # FACE 2 — React app on @ce-net/sdk (+ ce-drive-core compiled to WASM)
└── docs/design.md                     # this doc
```
Dependencies (all path/git, by design): `ce-rs`, `ce-coord`, `ce-cap` (`ce/crates/ce-cap`), `rdev` (for `chunk`/`delta`), `ce-notes` (for embedded docs). Mirrors how `ce-pin`/`rdev`/`ce-notes` already wire up.

---

## 12. How CE Drive composes each primitive (zero node changes)

| Need | CE primitive / app reused | Where |
|---|---|---|
| File bytes, dedup, delta, verify | content-addressed **blob store** + `rdev::chunk`/`rdev::delta`/`ce-rs::data` | `store.rs` — `put_blob`/`get_blob`, `chunk_object`/`reassemble`/`cid` |
| Durability / replication / PoR | **ce-pin** | `durability.rs` — `add --replication N`, `announce`, `status --audit` |
| Directory tree (multi-writer-safe) | **ce-coord** `StateMachine` + `RMap` | `tree.rs`, `content.rs` — `Replicated<DriveTree>` |
| Sharing / permissions / revoke | **ce-cap** signed attenuating chains | `share.rs` — `authorize`, `decode_chain`, `path_prefix`/`subtree` caveat, on-chain `RevokeCapability` |
| Collaborative documents | **ce-notes** (yrs CRDT, MergeSet) | `docs.rs` — embed by `doc_id`, share via Drive capability |
| Control plane (have/commit/list) | **AppRequest** request/reply | `ce-rs` `request`/`reply` (reuse `rdev::syncproto` verbs) |
| Audit | **chain** on-chain history + checkpoint | `audit.rs` — `/history`, periodic root-CID checkpoint |
| Workspace identity | **CE identity** (org root key) | root in `<data_dir>/roots/` or org node key |
| Web transport | **@ce-net/sdk** + browser CE node | `ce-drive-web` |

---

## 13. What (if anything) ce-coord needs added

Per the crdt-tree assessment, **single-writer v1 needs zero ce-coord changes** — implement `DriveTree: StateMachine` and ship. Three additive changes (all already on ce-coord's own roadmap, all benefiting Notes too):

1. **Snapshot / bootstrap API** (`Replicated::snapshot() -> (Version, Bytes)`, `bootstrap_from_snapshot(cid)` then tail). Needed so large/old drives don't replay the log from v1 — serialize `DriveTree.edges` + content map to a blob CID, fresh device fetches the snapshot (content-addressed, CID-verified) then tails. **Highest-priority addition.** (Shared with Notes M5.)
2. **Multi-writer "merged-log" mode** — `Replicated::multi_reader(writers: &[NodeId])` that merges N writer logs by an app-supplied `order_key(op) -> u128` (the `(lamport,replica)` ts). This is the genuine-CRDT multi-writer path (better than the planned Raft for this tree, because it preserves offline conflict-free moves). The undo/redo lives in `DriveTree::apply`, not in ce-coord. (Shared with Notes' `MergeSet`.) Needed only for v3 concurrent structural editing.
3. **(Optional) SSE push wrapper** in `ce-rs` over the existing `GET /mesh/messages/stream` — removes the 250 ms poll latency. Pure SDK change, no node change.

Everything else is reuse. **No note-/drive-specific endpoints are ever added to the node.**

---

## 14. Testing strategy

- **Pure unit (no infra):** move-CRDT — concurrent move-cycle resolves acyclically and identically on two replays (property test over random op orders + duplicates); dir-move is one op; create/rename/delete desugaring; name-collision → deterministic `.conflict-…`; `is_ancestor`/`children` index correctness; path derivation. Conflict decision table (LWW tiebreak determinism, conflict-copy naming). Chunk/delta reuse `rdev`'s existing `plan_transfer`/index tests.
- **Capability tests** (reuse rdev's `handle_inner` pattern): self-issued cap authorizes `drive:write` on a subtree; wrong audience/expired/non-rooted denied; `path_prefix`/`subtree` caveat blocks out-of-tree mutation; `..` traversal rejected; transitive read-only re-share cannot widen to write; on-chain revocation denies.
- **Crypto (E2E workspaces):** envelope round-trip; deterministic-nonce dedup (identical plaintext → identical ciphertext CID); X25519-from-Ed25519 vectors; lazy-rekey (revoked member can't apply post-rotation); tree-op confidentiality (a 3rd mesh node sees only ciphertext).
- **Two-node integration** (`NEXT_PORT` discipline, like ce-node/rdev): one-byte edit transfers exactly one chunk; dedup (same file two paths → chunks uploaded once); resume (kill after `put_blob`/before commit → converges, zero extra uploads); concurrent edit → both keep data + conflict copy; delete propagates idempotently; offline-edit-then-reconnect convergence via `await_version`.
- **Mount adapters:** per-OS smoke (fuser/FSKit/WinFsp/ProjFS) — `readdir`/`stat`/atomic-rename/write-temp→rename; stable inodes across remount; `git status` + `cargo build` on a materialized drive succeed; `readdirplus` reduces upcalls; hydrate-on-open pulls only touched chunks. CI matrix already builds macOS/Linux/Windows.
- **Docs (ce-notes embed):** concurrent `.cedoc` edits on two replicas converge byte-identical (reuse Notes' CRDT corpus); move/rename a doc while co-editing → no interference.
- **Web parity:** golden vectors — a `MoveOp`/`FileContent`/envelope produced by Rust core decodes+applies in the WASM/TS client and vice versa.
- **Scale/snapshot:** 100k-file drive — `readdir` O(children); fresh reader bootstraps from snapshot + tail with bounded replay; 5k structural ops converge.

---

## 15. Milestones (v1 sync+web → v2 mount → v3 collab+search)

See the structured `milestones` field. Sequencing rationale: v1 proves the *storage + tree + sharing + web* core with single-writer ce-coord and **zero node changes**; v2 adds the hard *mount* engine (lazy hydrate + write-back) and the driverless fallback over the same core; v3 adds *multi-writer collab docs + full-text search + previews + per-object audit*, driving the two ce-coord additions (snapshot/bootstrap, merged-log) and the search/thumbnail app-tier pipelines.

---

## 16. Risks

See the structured `risks` field.

---

## 17. One-line verdict

CE Drive is the capstone: **one `ce-drive-core` (Kleppmann move-CRDT tree over ce-coord + content-addressed chunked blobs via the rdev engine + ce-pin durability + ce-cap sharing + embedded ce-notes docs + optional dedup-preserving E2E) presented as both a lazy-hydrating cross-platform mount (with a driverless `materialize` fallback) and a Drive/Workspace web app on @ce-net/sdk** — composing every CE primitive, adding zero node endpoints, and needing only three already-roadmapped ce-coord additions (snapshot/bootstrap, merged-log multi-writer, SSE push).

## Milestones
- v1.M1 — ce-drive-core: tree CRDT + content + storage [L] — ce-drive-core crate: DriveTree Kleppmann move-CRDT as a ce-coord StateMachine (stable NodeId edges, cycle-skip, children index, path derivation, undo-data recorded but dormant); RMap<NodeId,FileContent> content map with version lists; store.rs over rdev::chunk+rdev::delta+ce-rs::data (chunk, dedup, delta upload, CID-verify). Single-writer, zero node/ce-coord changes. Pure unit + property tests (concurrent-move-cycle converges acyclically + identically; dir-move=1 op; collision→deterministic conflict-rename).\n- v1.M2 — Sharing, durability, audit-v1 [M] — share.rs: ce-cap per-folder grants (read/comment/write/admin) scoped by subtree/path_prefix caveat, attenuated transitive read-only, share links with expiry, on-chain RevokeCapability; reuse ce-cap authorize verbatim. durability.rs: ce-pin replication/announce/status policy per workspace. audit.rs v1: on-chain grant/revoke history reader. Capability tests (reuse rdev handle_inner pattern).\n- v1.M3 — Drive web app (sync + browse + share) [XL] — ce-drive-web on @ce-net/sdk (+ ce-drive-core compiled to WASM in the browser CE node): file browser (tree+list from DriveTree), upload/download (chunked, CID-verified), drag-move/rename/copy, trash/restore, version history, share dialog (mint+revoke ce-cap), filename/metadata search, audit-v1 view. Golden-vector parity Rust↔WASM. This is the open-source Google Drive MVP.\n- v2.M4 — Mount core + Linux + materialize fallback [XL] — ce-drive-mount: shared vfs engine (lazy hydrate from manifest, range/block fetch, write-back cache, async upload, stable inodes, readdirplus, atomic rename, attr/entry TTLs); fuser Linux adapter (-o writeback_cache,readdirplus); materialize.rs driverless fallback (materialize/push/--watch). Validate: git status + cargo build run on a CE drive; one-byte edit ships one chunk; hydrate-on-open pulls only touched chunks.\n- v2.M5 — macOS + Windows adapters + perf hardening [L] — macOS macFUSE-FSKit adapter (mount /Volumes/CEDrive, kext fast-mode opt-in); Windows WinFsp adapter + ProjFS hydrate-once mode for large trees; case-insensitivity/reserved-name/path-length normalization; --warm prefetch; IDE-watcher change notifications from the manifest layer. Per-OS CI smoke (fsevents/inotify/RDCW). Snapshot/bootstrap ce-coord addition landed so large-drive mount catch-up is bounded.\n- v3.M6 — Collaborative docs (embed ce-notes) + E2E + multi-writer [XL] — docs.rs: .cedoc nodes embedding ce-notes yrs documents (CodeMirror+Yjs editor in web, comments/presence via ce-coord Stream); Drive capability authorizes ce-notes write. Optional per-workspace E2E envelope (XChaCha20 + X25519, KBFS deterministic-nonce dedup, lazy rekey on revoke). Multi-writer DriveTree via ce-coord merged-log mode (order_key=Lamport ts; activate dormant undo/redo) — offline concurrent moves converge with no leader.\n- v3.M7 — Full-text search, previews/thumbnails, audit-v2 [XL] — Per-workspace full-text index (tantivy in core; client-side + encrypted for E2E workspaces); thumbnailer producing derived blobs (own CIDs); inline preview via blob range/partial fetch (PDF/image/video by range). Per-object access log (signed append-only, root-CID checkpointed onto chain for rollback-resistance) for HIPAA/enterprise audit. Quota/billing accounting over payment channels + ce-pin rent.

## Risks
- Dev-tool performance over a distributed mount is the make-or-break risk: git/cargo are metadata-latency-bound + small-file-storm workloads, and naive distributed POSIX FS (JuiceFS's own docs admit) are bad at them. Mitigation: long attr/entry TTLs, readdirplus, batched/prefetched listings, stable inodes, write-back with async upload, ProjFS hydrate-once on Windows, and always ship the native-speed `materialize` fallback (default in CI/containers).\n- Multi-writer concurrent structural editing (every device a writer) needs ce-coord's merged-log mode + the dormant undo/redo in apply; until v3 it is single-writer (one primary owns the tree log), so an offline primary stalls structural edits. Mitigation: gate true multi-writer to v3; v1/v2 use single-writer with the undo data recorded so promotion is zero-call-site-change. Note Raft (ce-coord's currently-planned next layer) is the WRONG fit — it would stall offline moves; the merged-log CRDT path is required.\n- ce-coord's single-writer log grows unbounded and fresh readers replay from v1 — expensive for large/old drives. Mitigation: the snapshot/bootstrap ce-coord addition (highest priority, shared with Notes M5): serialize tree+content map to a CID, new readers fetch the snapshot then tail.\n- Cross-platform mount fragmentation: macFUSE FSKit restricts mounts to /Volumes and is slower than the kext; ProjFS is write-passthrough (weaker coherence); WinFsp/FUSE don't emit native watcher events; editors' fsync/write-rename patterns stress write-back. Mitigation: per-platform defaults documented, atomic rename + stable inodes mandatory, manifest-layer change notifications, kext as opt-in fast-mode.\n- E2E encryption vs dedup/search tension: encrypted blocks normally kill cross-user dedup and server-side search. Mitigation: KBFS deterministic-nonce content-addressing (dedup survives encryption) and client-side encrypted search indexes; offer non-E2E org-internal workspaces where native dedup/search ergonomics matter and the threat model allows it.\n- Full-text search, thumbnails/previews, SSO/OIDC bridge, quota/billing, and the org admin console have NO CE primitive and are non-trivial app-tier builds. Mitigation: scope filename/metadata search to v1, defer full-text + previews to v3, and keep these honestly outside the v1/v2 core.\n- Embedded ce-notes parity risk: collaborative docs depend on ce-notes' yrs CRDT and its @ce-net/sdk JS surface, neither fully shipped. Mitigation: gate .cedoc collab to v3 (after ce-notes M5/M6); v1/v2 treat docs as ordinary versioned files; keep the doc layer behind a trait so Drive ships without it.\n- Per-object audit rollback-resistance and per-folder lazy-rekey add cryptographic complexity (Merkle checkpoint onto chain, key-generation bookkeeping) that must be correct for the HIPAA/enterprise claim. Mitigation: v1 audit = on-chain grant/revoke facts only (already immutable); defer the per-object access log + checkpointing to v2 with explicit threat-model docs.\n- fixed-size 1 MiB chunks (the current rdev/ce-rs substrate) dedup poorly under byte-shifting inserts vs content-defined chunking. Mitigation: acceptable for v1 (matches the shared engine); CDC is the noted boundary-algorithm upgrade in rdev::chunk, adopt it once and both rdev and CE Drive benefit.
