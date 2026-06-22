# ce-drive-wasm

The **pure, I/O-free CRDT core** of CE Drive, compiled to `wasm32` via `wasm-bindgen`.

It holds only the deterministic state machine — the Kleppmann move-CRDT directory tree, the
content/version metadata model, path resolution, and conflict info — plus serde. It depends on
**nothing** that targets native I/O: no `ce-rs`, `tokio`, `reqwest`, `rdev`, `libp2p`, or `ce-coord`.
That keeps it cleanly compilable to `wasm32-unknown-unknown` for the in-browser CE node.

## What's here vs. in the browser

| In wasm (this crate) | In the browser (`@ce-net/sdk`) |
|---|---|
| tree structure (move-CRDT) | the in-browser CE node + mesh transport |
| per-node content **metadata** (CID/size/version) | file **bytes/blobs** (`putBlob`/`getObject`) |
| path resolution, conflict computation | ce-coord `Merged` op delivery (fetch/publish) |
| minting a new local op (returns bytes) | publishing op bytes over the mesh |

**File bytes never cross into wasm.** The browser stores/fetches blobs through `@ce-net/sdk` keyed by
the CIDs this crate tracks.

## Wire compatibility (golden-vector parity)

Ops cross the JS boundary as opaque `Uint8Array` of JSON — **byte-identical** to what
`ce-drive-core` ships over the mesh through ce-coord's `Merged`. The op structs (`MoveOp`,
`StampedContentOp`, `ContentOp`, `Timestamp`, `NodeKind`, `FileContent`) mirror core's serde shapes
exactly, so:

- an op the Rust core publishes applies here unchanged, and
- an op minted here (`addFile`/`mkdir`/`move`/`remove`/`setContent`) applies in the Rust core.

## Build

```bash
# host (unit tests for the pure CRDT + wire round-trip):
cargo test -p ce-drive-wasm

# wasm32 target (structural wasm-cleanliness):
cargo build -p ce-drive-wasm --target wasm32-unknown-unknown

# browser bundle (+ generated .js glue and .d.ts) — requires wasm-pack:
wasm-pack build --target web crates/ce-drive-wasm
```

A hand-written `ce_drive_wasm.d.ts` ships alongside the crate documenting the exact exported JS API
(it matches what `wasm-pack` generates). The single exported class is `DriveCrdt`; see the `.d.ts`.
