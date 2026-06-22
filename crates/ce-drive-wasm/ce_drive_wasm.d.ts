/**
 * ce-drive-wasm — TypeScript surface for the PURE CE Drive CRDT core compiled to wasm32.
 *
 * This is the JS-facing API `wasm-bindgen` exports from `crates/ce-drive-wasm`. It is the
 * deterministic move-CRDT directory tree + content/version model + path resolution + conflict info,
 * with NO I/O: it never touches the network, the filesystem, or file bytes. The browser drives it
 * with opaque op bytes fetched/published via `@ce-net/sdk` against the in-browser CE node.
 *
 * Wire format: ops cross the boundary as `Uint8Array` of JSON — byte-identical to what
 * `ce-drive-core` ships over the mesh through ce-coord's `Merged`. So an op the Rust core publishes
 * applies here unchanged, and an op minted here applies in the Rust core (golden-vector parity).
 *
 * Convergence (leaderless, no quorum): feed EVERY op you fetch off the mesh — your own and every
 * peer's — into `applyMoveOp` / `applyContentOp`. The tree integrates by total-order timestamp
 * (Lamport + replica) and the content map is LWW-by-mtime, so the folded state is identical on every
 * device regardless of delivery order. File BYTES are NOT here: store/fetch blobs via `@ce-net/sdk`
 * (`putBlob`/`getObject`) keyed by the CIDs this module tracks.
 *
 * Usage sketch:
 * ```ts
 * import init, { DriveCrdt } from "./ce_drive_wasm.js";  // wasm-pack --target web output
 * await init();
 * const drive = new DriveCrdt(myNodeIdHex);
 *
 * // bootstrap + tail: feed ops the SDK fetched off the mesh
 * drive.applyMoveOps(treeOpBytesArray);
 * drive.applyContentOps(contentOpBytesArray);
 *
 * // local edit: mint -> publish bytes over the mesh via @ce-net/sdk
 * const cid = await sdk.putBlob(fileBytes);        // bytes live in the browser/node, not wasm
 * const { nodeId, moveOp, contentOp } = drive.addFile("/docs", "spec.md", cid, size, 0o644, Date.now());
 * if (moveOp)    await sdk.publish(treeTopic, moveOp);
 * if (contentOp) await sdk.publish(contentTopic, contentOp);
 *
 * // read
 * const entries = drive.list("/docs");             // DirEntry[]
 * const meta    = drive.contentOf(nodeId);         // FileContent | undefined
 * const clashes = drive.conflicts("/docs");        // Conflict[]
 * ```
 */

/** Initialize the wasm module's panic hook (routes Rust panics to `console.error`). */
export function init(): void;

/** A node's content metadata (NO file bytes). Mirrors `ce-drive-core`'s `FileContent`. */
export interface FileContent {
  /** The current object CID (whole-file content id; resolve bytes via @ce-net/sdk getObject). */
  cid: string;
  /** Current size in bytes. */
  size: number;
  /** Unix mode bits (0 where unknown / on Windows). */
  mode: number;
  /** Last-modified time, unix milliseconds — the LWW ordering key. */
  mtime_ms: number;
  /** Prior versions, oldest-first (capped). */
  versions: Array<{ set_at_ms: number; cid: string; size: number }>;
  /** Optional embedded ce-notes document id for `.cedoc` nodes; `null` for ordinary files. */
  doc_id: string | null;
}

/** One directory listing entry. */
export interface DirEntry {
  /** The rendered name (conflict-renamed loser names appear as `name.conflict-<replica>-<lamport>`). */
  name: string;
  /** The stable node id (identity across renames/moves). */
  nodeId: string;
  /** True for directories. */
  isDir: boolean;
  /** Content metadata for files; `null` for dirs and contentless files. */
  content: FileContent | null;
}

/** A surfaced name collision within a directory (two live nodes claiming the same bare name). */
export interface Conflict {
  /** The contested bare name. */
  name: string;
  /** The node that keeps the bare name (greater timestamp). */
  winner: string;
  /** The node that is conflict-renamed. */
  loser: string;
  /** The deterministic rendered name for the loser. */
  rendered: string;
}

/**
 * The result of minting a structural op: the new node id plus the op bytes to publish.
 * `moveOp` is `null` when the call was a pure content edit; `contentOp` is `null` for a bare dir.
 */
export interface MintResult {
  nodeId: string;
  /** MoveOp JSON bytes to publish on the drive's tree merged-log, or `null`. */
  moveOp: Uint8Array | null;
  /** StampedContentOp JSON bytes to publish on the content merged-log, or `null`. */
  contentOp: Uint8Array | null;
}

/**
 * The browser-side CRDT handle: this device's merged view of the drive's tree + content metadata.
 */
export class DriveCrdt {
  /**
   * Open an empty CRDT for this device.
   * @param replicaId this device's NodeId hex (the timestamp tiebreak; must be globally unique).
   */
  constructor(replicaId: string);

  /** This device's replica id (NodeId hex). */
  readonly replicaId: string;

  // ---- ingest opaque ops fetched off the mesh ----

  /** Apply one opaque MoveOp (JSON bytes). Returns true if applied. Idempotent by timestamp. */
  applyMoveOp(bytes: Uint8Array): boolean;
  /** Bulk-apply MoveOps (bootstrap/tail). Returns the number applied. */
  applyMoveOps(ops: Uint8Array[]): number;
  /** Apply one opaque StampedContentOp (JSON bytes). Idempotent via the LWW fold. */
  applyContentOp(bytes: Uint8Array): boolean;
  /** Bulk-apply StampedContentOps. Returns the number applied. */
  applyContentOps(ops: Uint8Array[]): number;

  // ---- queries ----

  /** List a directory (conflict-renamed + sorted by name). Throws if `path` is not a directory. */
  list(path: string): DirEntry[];
  /** Resolve an absolute path to a node id, or `undefined`. */
  resolve(path: string): string | undefined;
  /** A node's content metadata, or `undefined`. */
  contentOf(nodeId: string): FileContent | undefined;
  /** A node id's derived absolute path, or `undefined`. */
  pathOf(nodeId: string): string | undefined;
  /** The surfaced name collisions in a directory. */
  conflicts(path: string): Conflict[];
  /** Every distinct CID the drive references (the blobs the browser must keep available). */
  allCids(): string[];
  /** Number of structural (move) ops folded so far. */
  moveOpCount(): number;
  /** Number of content records. */
  contentCount(): number;

  // ---- mint new local ops (applies locally + returns bytes to publish) ----

  /** Create a directory. Returns `{ nodeId, moveOp, contentOp: null }`. Throws on name clash. */
  mkdir(parentPath: string, name: string): MintResult;
  /**
   * Add a file with already-stored content (the browser put the blob and holds its CID). If a live
   * file of that name exists, this is an edit on the SAME id (returns `moveOp: null`).
   */
  addFile(parentPath: string, name: string, cid: string, size: number, mode: number, mtimeMs: number): MintResult;
  /** Record new content for an existing file by id (write-back). Returns StampedContentOp bytes. */
  setContent(nodeId: string, cid: string, size: number, mode: number, mtimeMs: number): Uint8Array;
  /** Move/rename a node. Returns MoveOp bytes to publish. */
  move(srcPath: string, dstParentPath: string, dstName: string): Uint8Array;
  /** Delete a node (move to TRASH). Returns MoveOp bytes to publish. */
  remove(path: string): Uint8Array;

  /** Free the wasm-held memory for this instance. */
  free(): void;
}
