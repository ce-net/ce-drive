//! The content store — chunk/dedup/delta-upload/CID-verify over `rdev::chunk` + `rdev::delta` +
//! `ce-rs::data`.
//!
//! CE Drive does **not** reinvent the chunk engine. A file's bytes are split by `rdev::chunk`
//! (fixed 1 MiB chunks, `sha256` CIDs, an `ce-object-v1` manifest whose hash is the file CID), the
//! same engine the node's `/blobs` store is keyed by — so dedup is global and free. Delta transfer
//! uploads only the chunk CIDs the store lacks (`rdev::delta`). Reads fetch by CID, range/block-wise,
//! each chunk verified against its CID (content-addressing *is* the integrity proof).
//!
//! This module is the thin async glue between those pure engines and a live [`CeClient`]:
//! [`Store::put_file`] chunks + delta-uploads + returns the [`FileContent`] to record in the content
//! map; [`Store::get_file`] resolves a CID's manifest, fetches missing chunks, reassembles + verifies.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ce_rs::CeClient;
use ce_rs::data::{Manifest, MANIFEST_KIND_V1, cid as blob_cid};
use rdev::chunk::{ChunkedFile, chunk_bytes, content_id};
use rdev::delta;
use sha2::{Digest, Sha256};

use crate::content::FileContent;

/// The chunk size used by the streaming put/get paths (1 MiB — matches the shared engine boundary so
/// chunks dedup against everything else in the blob store).
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// The default maximum file size the store will accept (16 GiB). A hard upper bound so a runaway or
/// malicious input cannot exhaust disk/RAM. Override with [`Store::with_max_file_size`].
pub const DEFAULT_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024;

/// A content store backed by a CE node's blob layer.
pub struct Store {
    client: CeClient,
    /// Reject files larger than this (bytes). Guards both the in-memory and streaming put paths.
    max_file_size: u64,
}

/// The result of storing a file: the [`FileContent`] to record plus how many chunks actually moved.
#[derive(Debug, Clone)]
pub struct StoredFile {
    /// The content record to insert into the content map (keyed by the node's stable id). Its `cid`
    /// is the **object CID** (manifest hash) that `get_object` resolves.
    pub content: FileContent,
    /// The ordered chunk manifest of the file (kept so durability can pin every chunk CID).
    pub manifest: Manifest,
    /// The whole-file hash (`sha256(file_bytes)`) — an extra integrity cross-check distinct from the
    /// object CID. Equal to what the reassembled bytes hash to.
    pub file_cid: String,
    /// Number of chunks newly uploaded (0 when the file fully deduped against the store).
    pub uploaded_chunks: usize,
}

impl Store {
    /// A store over the given client (a local CE node, or a remote one with a token).
    pub fn new(client: CeClient) -> Self {
        Store { client, max_file_size: DEFAULT_MAX_FILE_SIZE }
    }

    /// Set the maximum file size this store accepts (a DoS / disk-exhaustion guard).
    pub fn with_max_file_size(mut self, max: u64) -> Self {
        self.max_file_size = max;
        self
    }

    /// The configured maximum file size (bytes).
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// The underlying client (so the caller can publish the manifest blob, announce, etc.).
    pub fn client(&self) -> &CeClient {
        &self.client
    }

    /// Probe which of `cids` the blob store is **missing**, distinguishing a genuine 404 from a
    /// transport error. A `get_blob` 404 ("blob not found") means the chunk must be uploaded; any
    /// other error (timeout, connection refused) is propagated rather than silently treated as
    /// "missing" — which would otherwise re-upload the whole file on a transient network blip (the
    /// audit's medium-severity delta-upload bug). Returns the subset of `cids` that are absent.
    async fn missing_chunks(&self, cids: &[String]) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for cid in cids {
            match self.client.get_blob(cid).await {
                Ok(_) => {}
                Err(e) => {
                    // ce-rs surfaces a not-found as "...: blob not found". Treat only that as absence.
                    let msg = e.to_string();
                    if msg.contains("not found") || msg.contains("404") {
                        missing.push(cid.clone());
                    } else {
                        return Err(e).with_context(|| format!("probe chunk {}", short(cid)));
                    }
                }
            }
        }
        Ok(missing)
    }

    /// Chunk `bytes`, upload only the chunks the store is missing (delta), publish the manifest as
    /// its own blob (so the object CID indirection resolves), and return the [`StoredFile`] with the
    /// [`FileContent`] (cid/size/mode/mtime) to record in the content map.
    pub async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<StoredFile> {
        if bytes.len() as u64 > self.max_file_size {
            bail!(
                "file is {} bytes, exceeds the {}-byte limit",
                bytes.len(),
                self.max_file_size
            );
        }
        let (cf, chunks): (ChunkedFile, Vec<(String, Vec<u8>)>) = chunk_bytes(bytes);

        // Ask the blob store which distinct chunk CIDs it is missing (a resilient probe that does not
        // mistake a transient error for absence).
        let distinct: Vec<String> = {
            let mut seen = HashSet::new();
            cf.chunk_cids().iter().filter(|c| seen.insert((*c).clone())).cloned().collect()
        };
        let missing = self.missing_chunks(&distinct).await?;
        let uploaded = delta::upload_missing(&self.client, &chunks, &missing)
            .await
            .context("delta upload of missing chunks")?;

        // Publish the manifest as its own blob; its hash is the **object CID** that `get_object`
        // resolves (manifest -> chunks -> reassemble, every chunk CID-verified). We record the
        // object CID as the content cid so the content map points at something `get_object` can
        // fetch. The whole-file hash (`cf.file_cid`) stays on the [`StoredFile`] for callers that
        // want an additional whole-file integrity cross-check.
        let manifest_bytes = serde_json::to_vec(&cf.manifest).context("serialize manifest")?;
        let object_cid = self
            .client
            .put_blob(manifest_bytes)
            .await
            .context("store object manifest")?;

        let content = FileContent::new(object_cid, cf.size(), mode, mtime_ms);
        Ok(StoredFile { content, manifest: cf.manifest, file_cid: cf.file_cid, uploaded_chunks: uploaded })
    }

    /// Read a file from disk and store it, **streaming chunk-by-chunk** so peak memory is one chunk
    /// (1 MiB), not the whole file. A multi-GB file is chunked, deduped, and delta-uploaded without
    /// ever being fully buffered in RAM (the audit's top robustness fix). The whole-file size bound is
    /// enforced incrementally as bytes are read, so an oversized file is rejected before it is fully
    /// consumed. Mode/mtime are preserved where the OS exposes them.
    pub async fn put_file(&self, path: &Path) -> Result<StoredFile> {
        let meta = std::fs::metadata(path).ok();
        let mode = meta.as_ref().map(mode_of).unwrap_or(0o644);
        let mtime_ms = meta.as_ref().map(mtime_ms_of).unwrap_or(0);
        // Fast pre-check against the declared size (cheap rejection before reading anything).
        if let Some(sz) = meta.as_ref().map(|m| m.len())
            && sz > self.max_file_size
        {
            bail!("file '{}' is {sz} bytes, exceeds the {}-byte limit", path.display(), self.max_file_size);
        }

        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut chunk_cids: Vec<String> = Vec::new();
        let mut total: u64 = 0;
        let mut file_hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK_SIZE];
        // Stream: read up to one chunk at a time, hash + upload-if-missing each, never buffering more
        // than one chunk. Dedup is per-chunk (the blob store is content-addressed), so a re-store of
        // an existing chunk moves nothing.
        let mut uploaded = 0usize;
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            let n = fill_chunk(&mut reader, &mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > self.max_file_size {
                bail!("file '{}' exceeds the {}-byte limit while streaming", path.display(), self.max_file_size);
            }
            let slice = &buf[..n];
            file_hasher.update(slice);
            let ccid = blob_cid(slice);
            chunk_cids.push(ccid.clone());
            // Only probe + upload each distinct chunk once per file.
            if seen.insert(ccid.clone()) {
                let missing = self.missing_chunks(std::slice::from_ref(&ccid)).await?;
                if !missing.is_empty() {
                    let stored = self.client.put_blob(slice.to_vec()).await.context("put chunk")?;
                    if stored != ccid {
                        bail!("blob store returned {stored} for chunk {ccid} (hash mismatch)");
                    }
                    uploaded += 1;
                }
            }
        }
        let file_cid = hex::encode(file_hasher.finalize());
        let manifest = Manifest {
            kind: MANIFEST_KIND_V1.to_string(),
            chunk_size: CHUNK_SIZE as u64,
            total_size: total,
            chunks: chunk_cids,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).context("serialize manifest")?;
        let object_cid =
            self.client.put_blob(manifest_bytes).await.context("store object manifest")?;
        let content = FileContent::new(object_cid, total, mode, mtime_ms);
        Ok(StoredFile { content, manifest, file_cid, uploaded_chunks: uploaded })
    }

    /// Fetch a file by its **object CID** (the content cid recorded in the map). Resolves the
    /// manifest blob, then reassembles + verifies every chunk against its CID. This is the canonical
    /// read path and is equivalent to the SDK's `get_object` (kept here so the store owns both ends).
    pub async fn get_file(&self, object_cid: &str) -> Result<Vec<u8>> {
        self.client.get_object(object_cid).await.context("resolve object + verify chunks")
    }

    /// Fetch a file by its object CID and write it to `dest`, **streaming chunk-by-chunk** so peak
    /// memory is one chunk (1 MiB), not the whole file. The object manifest is resolved, then each
    /// chunk is fetched, CID-verified, and appended to a temp file which is fsync'd and atomically
    /// renamed into place (so a partial/interrupted download never leaves a corrupt destination). The
    /// reassembled length is cross-checked against the manifest. Returns the bytes written.
    pub async fn get_file_to_path(&self, object_cid: &str, dest: &Path) -> Result<u64> {
        use std::io::Write;
        let manifest_bytes =
            self.client.get_blob(object_cid).await.with_context(|| format!("resolve manifest {}", short(object_cid)))?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("{} is not a v1 object manifest", short(object_cid)))?;
        if manifest.kind != MANIFEST_KIND_V1 {
            bail!("unsupported manifest kind: {}", manifest.kind);
        }
        if manifest.total_size > self.max_file_size {
            bail!("object {} is {} bytes, exceeds the {}-byte limit", short(object_cid), manifest.total_size, self.max_file_size);
        }
        let tmp = dest.with_extension("ce-drive-partial");
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?,
        );
        let mut written: u64 = 0;
        for chunk_cid in &manifest.chunks {
            let chunk =
                self.client.get_blob(chunk_cid).await.with_context(|| format!("fetch chunk {}", short(chunk_cid)))?;
            let got = blob_cid(&chunk);
            if got != *chunk_cid {
                let _ = std::fs::remove_file(&tmp);
                bail!("chunk verification failed: expected {chunk_cid}, got {got}");
            }
            out.write_all(&chunk).with_context(|| format!("write {}", tmp.display()))?;
            written += chunk.len() as u64;
        }
        if written != manifest.total_size {
            let _ = std::fs::remove_file(&tmp);
            bail!("reassembled size {written} != manifest total_size {}", manifest.total_size);
        }
        // Durable, atomic publish: flush + fsync + rename.
        out.flush().context("flush")?;
        let file = out.into_inner().context("finalize temp file")?;
        file.sync_all().context("fsync temp file")?;
        drop(file);
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        Ok(written)
    }

    /// Reassemble a file from an explicit manifest, verifying every chunk against its CID and the
    /// whole file against `file_cid` (the whole-file hash). Used when the manifest is already held
    /// (e.g. from a [`StoredFile`]) so no manifest-blob fetch is needed.
    pub async fn reassemble_file(&self, file_cid: &str, manifest: &Manifest) -> Result<Vec<u8>> {
        delta::apply_commit_verified(&self.client, manifest, file_cid)
            .await
            .context("reassemble + verify file")
    }

    /// Fetch a file with explicit already-held-chunk accounting: only chunks not in `held` move over
    /// the wire (a one-chunk delta on a one-byte edit). Returns `(bytes, n_fetched)`. `file_cid` is
    /// the whole-file hash used for the final integrity check.
    pub async fn get_file_delta(
        &self,
        file_cid: &str,
        manifest: &Manifest,
        held: &HashSet<String>,
    ) -> Result<(Vec<u8>, usize)> {
        delta::pull_file_verified(&self.client, manifest, file_cid, held)
            .await
            .context("delta pull + verify file")
    }

    /// The content id (sha256 hex) of bytes — re-exported from the shared engine so callers and the
    /// node's blob store agree on keying.
    pub fn content_id(bytes: &[u8]) -> String {
        content_id(bytes)
    }
}

/// Read up to `buf.len()` bytes into `buf`, returning how many were read. Handles short reads from
/// `BufReader` by looping until the buffer is full or EOF, so each emitted chunk is exactly
/// [`CHUNK_SIZE`] (except the last) — matching the in-memory chunker's boundaries so CIDs agree.
fn fill_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("read chunk"),
        }
    }
    Ok(filled)
}

/// Short (16-hex) form of a CID for error/log messages.
fn short(s: &str) -> &str {
    &s[..16.min(s.len())]
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

fn mtime_ms_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdev::chunk::{CHUNK_SIZE, chunk_bytes};

    // These tests exercise the pure chunk/delta planning the store relies on, without a live node.
    // The async put/get paths are covered by the two-node integration test (gated, NEXT_PORT).

    #[test]
    fn one_byte_edit_yields_one_missing_chunk() {
        let mut data: Vec<u8> = (0..(CHUNK_SIZE * 3) as u32).map(|i| i as u8).collect();
        let (orig, _) = chunk_bytes(&data);
        data[7] ^= 0xff;
        let (edited, _) = chunk_bytes(&data);
        let held: HashSet<String> = orig.chunk_cids().iter().cloned().collect();
        let missing = delta::compute_missing(edited.chunk_cids(), &held);
        let plan = delta::plan_transfer(edited.chunk_cids(), &missing);
        assert_eq!(plan.len(), 1, "one-byte edit moves exactly one chunk");
    }

    #[test]
    fn identical_bytes_dedupe_to_zero_uploads() {
        let data = vec![5u8; CHUNK_SIZE * 2];
        let (f1, _) = chunk_bytes(&data);
        let held: HashSet<String> = f1.chunk_cids().iter().cloned().collect();
        let (f2, _) = chunk_bytes(&data);
        let missing = delta::compute_missing(f2.chunk_cids(), &held);
        assert!(missing.is_empty());
    }

    #[test]
    fn content_id_matches_engine() {
        assert_eq!(Store::content_id(b"hello"), content_id(b"hello"));
    }

    #[test]
    fn streaming_chunker_matches_in_memory_manifest() {
        // The streaming put_file path must produce the byte-identical manifest (and hence object CID)
        // as the in-memory chunk_bytes path, or reads written by one and fetched by the other break.
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 123) as u32).map(|i| (i % 251) as u8).collect();
        let (cf, _) = chunk_bytes(&data);
        // Reproduce the streaming side: read CHUNK_SIZE at a time, hash each.
        let mut reader = std::io::Cursor::new(&data);
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut chunk_cids = Vec::new();
        let mut total = 0u64;
        loop {
            let n = fill_chunk(&mut reader, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n as u64;
            chunk_cids.push(blob_cid(&buf[..n]));
        }
        assert_eq!(chunk_cids, cf.chunk_cids(), "streaming chunk CIDs match in-memory");
        assert_eq!(total, data.len() as u64);
        // And the manifests (the object-CID preimage) are identical.
        let streamed = Manifest {
            kind: MANIFEST_KIND_V1.to_string(),
            chunk_size: CHUNK_SIZE as u64,
            total_size: total,
            chunks: chunk_cids,
        };
        assert_eq!(streamed, cf.manifest, "streaming manifest == in-memory manifest");
    }

    #[test]
    fn fill_chunk_handles_short_reads() {
        // A reader that returns 1 byte at a time must still fill a full chunk.
        struct DripReader<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl Read for DripReader<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }
        let data = vec![9u8; 10];
        let mut r = DripReader { data: &data, pos: 0 };
        let mut buf = vec![0u8; 4];
        assert_eq!(fill_chunk(&mut r, &mut buf).unwrap(), 4, "drip reader still fills the chunk");
        assert_eq!(&buf[..], &[9, 9, 9, 9]);
        assert_eq!(fill_chunk(&mut r, &mut buf).unwrap(), 4);
        assert_eq!(fill_chunk(&mut r, &mut buf).unwrap(), 2, "final short chunk");
        assert_eq!(fill_chunk(&mut r, &mut buf).unwrap(), 0, "EOF");
    }
}
