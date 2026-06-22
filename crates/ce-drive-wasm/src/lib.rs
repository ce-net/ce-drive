//! # ce-drive-wasm — the PURE CRDT core of CE Drive, compiled to wasm32
//!
//! This crate is the deterministic, **I/O-free** heart of CE Drive's directory model, exposed to the
//! browser via `wasm-bindgen`. It holds only the move-CRDT tree + the content/version model + path
//! resolution + conflict info, plus serde. It depends on **nothing** that touches native sockets or
//! the filesystem — no ce-rs, tokio, reqwest, rdev, libp2p, or ce-coord (none of which target wasm).
//!
//! ## The split (what is and isn't here)
//! * **In wasm (this crate):** the tree structure, per-node content **metadata** (CID / size /
//!   version), path resolution, conflict computation, and minting a new local [`MoveOp`].
//! * **In the browser (via `@ce-net/sdk`):** the in-browser CE node, the mesh transport, ce-coord's
//!   `Merged` op delivery, and the **file bytes/blobs** (`put_blob`/`get_object`). The browser fetches
//!   opaque op bytes off the mesh and feeds them here; it takes the op bytes this crate mints and
//!   publishes them. File bytes are never marshalled through wasm.
//!
//! ## Wire compatibility
//! Ops cross the boundary as **opaque JSON byte arrays** (`Uint8Array`) — exactly the encoding
//! ce-coord uses on the mesh. The op structs ([`crdt::MoveOp`], [`crdt::StampedContentOp`]) are
//! byte-identical to `ce-drive-core`'s, so an op produced by the Rust core applies here and vice
//! versa. That is the golden-vector parity the design's web milestone (M3) requires.
//!
//! ## The exported JS API (see the generated `.d.ts`)
//! A single [`DriveCrdt`] class:
//! * `new DriveCrdt(replicaId)` — open an empty CRDT for this device.
//! * `applyMoveOp(bytes)` / `applyContentOp(bytes)` — ingest one opaque op fetched off the mesh.
//! * `applyMoveOps(bytesArray)` / `applyContentOps(bytesArray)` — bulk ingest (bootstrap/tail).
//! * `list(path)` — directory listing (`DirEntry[]`).
//! * `resolve(path)` — path -> node id (or `undefined`).
//! * `contentOf(nodeId)` — a node's content metadata (`FileContent`), or `undefined`.
//! * `pathOf(nodeId)` — node id -> derived absolute path.
//! * `conflicts(path)` — surfaced name collisions in a directory (`Conflict[]`).
//! * `mkdir / addFile / move / remove / setContent` — mint a **new local op**; each returns the
//!   op **bytes** to publish over the mesh (and applies it locally). `addFile`/`mkdir` return an
//!   `{ nodeId, moveOp, contentOp? }` envelope so the caller knows the new id.
//! * `allCids()` — every CID the drive references (the blobs the browser must keep available).

mod crdt;

use std::cell::Cell;

use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crdt::{
    ContentMap, ContentOp, DriveTree, FileContent as RawFileContent, MoveOp, NodeKind,
    StampedContentOp, Timestamp, ROOT, TRASH,
};

/// Called once from JS (`init()`) to route Rust panics to `console.error` for debuggability.
#[wasm_bindgen]
pub fn init() {
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
}

// =================================================================================================
// JS-facing value types (serialized via serde-wasm through JsValue as plain JS objects).
// =================================================================================================

/// One directory listing entry handed to JS.
#[derive(Serialize, Deserialize)]
struct DirEntry {
    name: String,
    #[serde(rename = "nodeId")]
    node_id: String,
    #[serde(rename = "isDir")]
    is_dir: bool,
    /// Content metadata for files (CID/size/version), `null` for dirs / contentless files.
    content: Option<RawFileContent>,
}

/// The result of minting a file/dir op: the new node id plus the op bytes the caller must publish.
#[derive(Serialize, Deserialize)]
struct MintResult {
    #[serde(rename = "nodeId")]
    node_id: String,
    /// The MoveOp bytes (publish on the tree merged-log), or `null` when this was a pure edit.
    #[serde(rename = "moveOp")]
    move_op: Option<Vec<u8>>,
    /// The StampedContentOp bytes (publish on the content merged-log), or `null` for a bare dir.
    #[serde(rename = "contentOp")]
    content_op: Option<Vec<u8>>,
}

// =================================================================================================
// The exported CRDT handle.
// =================================================================================================

/// The browser-side CRDT state: this device's merged view of the drive's tree + content map, plus a
/// monotonic Lamport clock for minting new local ops. Wraps the pure [`DriveTree`] + [`ContentMap`].
///
/// Convergence is the merged-log model: the browser feeds every op it fetches off the mesh (its own
/// and every peer's) into `applyMoveOp`/`applyContentOp`; because the tree integrates by total-order
/// timestamp and the content map is LWW-by-mtime, the folded state is identical on every device
/// regardless of delivery order — leaderless, no quorum.
#[wasm_bindgen]
pub struct DriveCrdt {
    replica: String,
    lamport: Cell<u64>,
    tree: DriveTree,
    content: ContentMap,
}

#[wasm_bindgen]
impl DriveCrdt {
    /// Open an empty CRDT for this device. `replicaId` is the in-browser node's NodeId hex — the
    /// tiebreak in every op timestamp; it must be globally unique per device.
    #[wasm_bindgen(constructor)]
    pub fn new(replica_id: &str) -> DriveCrdt {
        DriveCrdt {
            replica: replica_id.to_string(),
            lamport: Cell::new(0),
            tree: DriveTree::new(),
            content: ContentMap::new(),
        }
    }

    /// This device's replica id (NodeId hex).
    #[wasm_bindgen(getter, js_name = replicaId)]
    pub fn replica_id(&self) -> String {
        self.replica.clone()
    }

    // ---- ingest opaque ops fetched off the mesh ----

    /// Apply one opaque [`MoveOp`] (JSON bytes the browser fetched via `@ce-net/sdk`). Returns `true`
    /// if it decoded and applied. Idempotent: re-delivering the same op is a no-op.
    #[wasm_bindgen(js_name = applyMoveOp)]
    pub fn apply_move_op(&mut self, bytes: &[u8]) -> Result<bool, JsError> {
        let op: MoveOp = serde_json::from_slice(bytes)
            .map_err(|e| JsError::new(&format!("decode MoveOp: {e}")))?;
        self.observe(&op.ts);
        self.tree.apply(op);
        Ok(true)
    }

    /// Apply many [`MoveOp`]s at once (bootstrap/tail). `ops` is an array of `Uint8Array`. Returns the
    /// number applied.
    #[wasm_bindgen(js_name = applyMoveOps)]
    pub fn apply_move_ops(&mut self, ops: Vec<js_sys::Uint8Array>) -> Result<usize, JsError> {
        let mut n = 0;
        for arr in ops {
            if self.apply_move_op(&arr.to_vec())? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Apply one opaque [`StampedContentOp`] (JSON bytes off the mesh). Idempotent by timestamp via
    /// the LWW fold.
    #[wasm_bindgen(js_name = applyContentOp)]
    pub fn apply_content_op(&mut self, bytes: &[u8]) -> Result<bool, JsError> {
        let stamped: StampedContentOp = serde_json::from_slice(bytes)
            .map_err(|e| JsError::new(&format!("decode StampedContentOp: {e}")))?;
        self.observe(&stamped.ts);
        self.content.apply(stamped.op);
        Ok(true)
    }

    /// Apply many [`StampedContentOp`]s at once. Returns the number applied.
    #[wasm_bindgen(js_name = applyContentOps)]
    pub fn apply_content_ops(&mut self, ops: Vec<js_sys::Uint8Array>) -> Result<usize, JsError> {
        let mut n = 0;
        for arr in ops {
            if self.apply_content_op(&arr.to_vec())? {
                n += 1;
            }
        }
        Ok(n)
    }

    // ---- queries ----

    /// List a directory: an array of `{ name, nodeId, isDir, content }`, conflict-renamed + sorted.
    #[wasm_bindgen]
    pub fn list(&self, path: &str) -> Result<JsValue, JsError> {
        let dir = self.resolve_dir(path)?;
        let entries: Vec<DirEntry> = self
            .tree
            .readdir(&dir)
            .into_iter()
            .map(|(name, id, kind)| DirEntry {
                name,
                is_dir: matches!(kind, NodeKind::Dir),
                content: self.content.get(&id).cloned(),
                node_id: id,
            })
            .collect();
        to_js(&entries)
    }

    /// Resolve an absolute path to a node id, or `undefined`.
    #[wasm_bindgen]
    pub fn resolve(&self, path: &str) -> Option<String> {
        self.tree.resolve(path)
    }

    /// A node's content metadata (`FileContent`: cid/size/mode/mtime/versions/doc_id), or `undefined`.
    #[wasm_bindgen(js_name = contentOf)]
    pub fn content_of(&self, node_id: &str) -> Result<JsValue, JsError> {
        match self.content.get(node_id) {
            Some(c) => to_js(c),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// A node id's derived absolute path, or `undefined`.
    #[wasm_bindgen(js_name = pathOf)]
    pub fn path_of(&self, node_id: &str) -> Option<String> {
        self.tree.path(node_id)
    }

    /// The surfaced name collisions in a directory: `{ name, winner, loser, rendered }[]`.
    #[wasm_bindgen]
    pub fn conflicts(&self, path: &str) -> Result<JsValue, JsError> {
        let dir = self.resolve_dir(path)?;
        to_js(&self.tree.conflicts(&dir))
    }

    /// Every distinct CID the drive references (the blobs the browser must keep available/pinned).
    #[wasm_bindgen(js_name = allCids)]
    pub fn all_cids(&self) -> Vec<String> {
        self.content.all_cids()
    }

    /// The number of structural (move) ops folded so far.
    #[wasm_bindgen(js_name = moveOpCount)]
    pub fn move_op_count(&self) -> usize {
        self.tree.log_len()
    }

    /// The number of content records.
    #[wasm_bindgen(js_name = contentCount)]
    pub fn content_count(&self) -> usize {
        self.content.len()
    }

    // ---- mint new local ops (apply locally + return bytes to publish) ----

    /// Create a directory under `parentPath`. Mints one [`MoveOp`], applies it locally, and returns
    /// `{ nodeId, moveOp, contentOp: null }` — publish `moveOp` on the tree merged-log.
    #[wasm_bindgen]
    pub fn mkdir(&mut self, parent_path: &str, name: &str) -> Result<JsValue, JsError> {
        let parent = self.resolve_dir(parent_path)?;
        if self.tree.child_named(&parent, name).is_some() {
            return Err(JsError::new(&format!("'{name}' already exists in '{parent_path}'")));
        }
        let ts = self.tick();
        let id = fresh_node_id(&ts);
        let op = MoveOp { ts, child: id.clone(), new_parent: parent, new_name: name.to_string(), kind: NodeKind::Dir };
        let bytes = encode(&op)?;
        self.tree.apply(op);
        to_js(&MintResult { node_id: id, move_op: Some(bytes), content_op: None })
    }

    /// Add a file under `parentPath` with already-stored content (the browser has put the blob and
    /// holds its CID/size/mtime). If a live file of that name exists, this becomes an edit on the
    /// SAME stable id (a content op only). Returns `{ nodeId, moveOp?, contentOp }`.
    #[wasm_bindgen(js_name = addFile)]
    pub fn add_file(
        &mut self,
        parent_path: &str,
        name: &str,
        cid: &str,
        size: u64,
        mode: u32,
        mtime_ms: u64,
    ) -> Result<JsValue, JsError> {
        let parent = self.resolve_dir(parent_path)?;
        if let Some(existing) = self.tree.child_named(&parent, name) {
            if matches!(self.tree.edge(&existing).map(|e| &e.kind), Some(NodeKind::File)) {
                let content_bytes = self.mint_content(ContentOp::Set {
                    id: existing.clone(),
                    cid: cid.to_string(),
                    size,
                    mode,
                    mtime_ms,
                })?;
                return to_js(&MintResult { node_id: existing, move_op: None, content_op: Some(content_bytes) });
            }
            return Err(JsError::new(&format!("'{name}' already exists as a directory")));
        }
        let ts = self.tick();
        let id = fresh_node_id(&ts);
        let mv = MoveOp { ts, child: id.clone(), new_parent: parent, new_name: name.to_string(), kind: NodeKind::File };
        let mv_bytes = encode(&mv)?;
        self.tree.apply(mv);
        let content_bytes = self.mint_content(ContentOp::Set {
            id: id.clone(),
            cid: cid.to_string(),
            size,
            mode,
            mtime_ms,
        })?;
        to_js(&MintResult { node_id: id, move_op: Some(mv_bytes), content_op: Some(content_bytes) })
    }

    /// Record new content for an existing file by its stable node id (write-back). Returns the
    /// [`StampedContentOp`] bytes to publish.
    #[wasm_bindgen(js_name = setContent)]
    pub fn set_content(
        &mut self,
        node_id: &str,
        cid: &str,
        size: u64,
        mode: u32,
        mtime_ms: u64,
    ) -> Result<Vec<u8>, JsError> {
        self.mint_content(ContentOp::Set { id: node_id.to_string(), cid: cid.to_string(), size, mode, mtime_ms })
    }

    /// Move/rename a node. Mints one [`MoveOp`]; returns its bytes.
    #[wasm_bindgen(js_name = "move")]
    pub fn move_node(&mut self, src_path: &str, dst_parent_path: &str, dst_name: &str) -> Result<Vec<u8>, JsError> {
        let node = self
            .tree
            .resolve(src_path)
            .ok_or_else(|| JsError::new(&format!("no such path: {src_path}")))?;
        if node == ROOT {
            return Err(JsError::new("cannot move the root"));
        }
        let dst_parent = self.resolve_dir(dst_parent_path)?;
        let kind = self.tree.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
        let ts = self.tick();
        let op = MoveOp { ts, child: node, new_parent: dst_parent, new_name: dst_name.to_string(), kind };
        let bytes = encode(&op)?;
        self.tree.apply(op);
        Ok(bytes)
    }

    /// Delete a node (move to TRASH). Mints one [`MoveOp`]; returns its bytes.
    #[wasm_bindgen]
    pub fn remove(&mut self, path: &str) -> Result<Vec<u8>, JsError> {
        let node = self
            .tree
            .resolve(path)
            .ok_or_else(|| JsError::new(&format!("no such path: {path}")))?;
        if node == ROOT {
            return Err(JsError::new("cannot delete the root"));
        }
        let name = self.tree.edge(&node).map(|e| e.name.clone()).unwrap_or_default();
        let kind = self.tree.edge(&node).map(|e| e.kind.clone()).unwrap_or(NodeKind::File);
        let ts = self.tick();
        let op = MoveOp { ts, child: node, new_parent: TRASH.to_string(), new_name: name, kind };
        let bytes = encode(&op)?;
        self.tree.apply(op);
        Ok(bytes)
    }

    // ---- internals ----

    fn mint_content(&mut self, op: ContentOp) -> Result<Vec<u8>, JsError> {
        let ts = self.tick();
        let stamped = StampedContentOp { ts, op };
        let bytes = encode(&stamped)?;
        self.content.apply(stamped.op);
        Ok(bytes)
    }

    fn tick(&self) -> Timestamp {
        let next = self.lamport.get() + 1;
        self.lamport.set(next);
        Timestamp::new(next, self.replica.clone())
    }

    fn observe(&self, ts: &Timestamp) {
        if ts.lamport > self.lamport.get() {
            self.lamport.set(ts.lamport);
        }
    }

    fn resolve_dir(&self, path: &str) -> Result<String, JsError> {
        let node = self
            .tree
            .resolve(path)
            .ok_or_else(|| JsError::new(&format!("no such directory: {path}")))?;
        if node != ROOT && !matches!(self.tree.edge(&node).map(|e| &e.kind), Some(NodeKind::Dir)) {
            return Err(JsError::new(&format!("'{path}' is not a directory")));
        }
        Ok(node)
    }
}

/// A globally-unique fresh node id from a timestamp, embedding the replica so concurrent writers
/// never mint the same id. Matches `ce-drive-core::sync`'s scheme shape.
fn fresh_node_id(ts: &Timestamp) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let rep: String = ts.replica.chars().take(8).collect();
    format!("{:012x}-{rep}-{c:08x}", ts.lamport)
}

/// Serialize an op to JSON bytes (the opaque payload published over the mesh).
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, JsError> {
    serde_json::to_vec(v).map_err(|e| JsError::new(&format!("encode op: {e}")))
}

/// Serialize a value to a plain JS object/array via JSON (kept dependency-light: serde_json ->
/// `js_sys::JSON::parse`, so the JS side gets real objects, not strings).
fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsError> {
    let s = serde_json::to_string(v).map_err(|e| JsError::new(&format!("to_js: {e}")))?;
    js_sys::JSON::parse(&s).map_err(|_| JsError::new("to_js: JSON.parse failed"))
}

#[cfg(test)]
mod tests {
    // Pure-logic tests live in crdt::tests. The wasm-bindgen surface (DriveCrdt) is exercised by
    // wasm-bindgen-test in a browser/node runner; here we sanity-check op round-trip parity with the
    // wire format using only host-safe code (no JsValue).
    use super::crdt::*;

    #[test]
    fn moveop_wire_roundtrips() {
        let op = MoveOp {
            ts: Timestamp::new(7, "node-abc"),
            child: "c1".into(),
            new_parent: ROOT.into(),
            new_name: "x".into(),
            kind: NodeKind::File,
        };
        let bytes = serde_json::to_vec(&op).unwrap();
        let back: MoveOp = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn stamped_content_wire_roundtrips() {
        let op = StampedContentOp {
            ts: Timestamp::new(3, "node-xyz"),
            op: ContentOp::Set { id: "c1".into(), cid: "cidA".into(), size: 10, mode: 0o644, mtime_ms: 99 },
        };
        let bytes = serde_json::to_vec(&op).unwrap();
        let back: StampedContentOp = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&back).unwrap(), bytes);
    }

    /// CROSS-CRATE PARITY: the exact golden JSON strings pinned in `ce-drive-core`'s `wire_golden.rs`
    /// must encode here byte-for-byte. If core and wasm ever disagree on field names/shape, one of
    /// these two mirrored tests fails — so an op produced by the Rust core decodes and applies in the
    /// browser CRDT and vice versa (the design's golden-vector parity requirement).
    #[test]
    fn wasm_wire_golden_matches_core() {
        // These literals are duplicated verbatim from ce-drive-core::tests::wire_golden — by design,
        // since the two crates cannot share a dependency. They are the inter-crate wire contract.
        const GOLDEN_MOVE_OP: &str =
            r#"{"ts":{"lamport":7,"replica":"node-abc"},"child":"c1","new_parent":"ROOT","new_name":"x","kind":"File"}"#;
        const GOLDEN_CONTENT_OP: &str =
            r#"{"ts":{"lamport":3,"replica":"node-xyz"},"op":{"Set":{"id":"c1","cid":"cidA","size":10,"mode":420,"mtime_ms":99}}}"#;

        let mv = MoveOp {
            ts: Timestamp::new(7, "node-abc"),
            child: "c1".into(),
            new_parent: ROOT.into(),
            new_name: "x".into(),
            kind: NodeKind::File,
        };
        assert_eq!(serde_json::to_string(&mv).unwrap(), GOLDEN_MOVE_OP, "wasm MoveOp wire drifted from core");
        // The golden string from core decodes here and applies (proving an op minted by the Rust core
        // is consumable by the browser CRDT).
        let mut tree = DriveTree::new();
        let decoded: MoveOp = serde_json::from_str(GOLDEN_MOVE_OP).unwrap();
        tree.apply(decoded);
        assert!(tree.path("c1").is_some(), "core-minted op applied in the wasm tree");

        let content = StampedContentOp {
            ts: Timestamp::new(3, "node-xyz"),
            op: ContentOp::Set { id: "c1".into(), cid: "cidA".into(), size: 10, mode: 0o644, mtime_ms: 99 },
        };
        assert_eq!(serde_json::to_string(&content).unwrap(), GOLDEN_CONTENT_OP, "wasm ContentOp wire drifted from core");
        let decoded_c: StampedContentOp = serde_json::from_str(GOLDEN_CONTENT_OP).unwrap();
        let mut cm = ContentMap::new();
        cm.apply(decoded_c.op);
        assert_eq!(cm.get("c1").unwrap().cid, "cidA", "core-minted content op applied in wasm map");
    }
}
