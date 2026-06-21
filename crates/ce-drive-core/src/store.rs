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
use std::path::Path;

use anyhow::{Context, Result};
use ce_rs::CeClient;
use ce_rs::data::Manifest;
use rdev::chunk::{ChunkedFile, chunk_bytes, content_id};
use rdev::delta;

use crate::content::FileContent;

/// A content store backed by a CE node's blob layer.
pub struct Store {
    client: CeClient,
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
        Store { client }
    }

    /// The underlying client (so the caller can publish the manifest blob, announce, etc.).
    pub fn client(&self) -> &CeClient {
        &self.client
    }

    /// Chunk `bytes`, upload only the chunks the store is missing (delta), publish the manifest as
    /// its own blob (so the object CID indirection resolves), and return the [`StoredFile`] with the
    /// [`FileContent`] (cid/size/mode/mtime) to record in the content map.
    pub async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<StoredFile> {
        let (cf, chunks): (ChunkedFile, Vec<(String, Vec<u8>)>) = chunk_bytes(bytes);

        // Ask the blob store which chunk CIDs it already holds, by probing get_blob. To keep this a
        // true delta without an extra control-plane round-trip, we treat "already in the store" as
        // the dedup boundary: upload every distinct chunk via put_blob (the store is idempotent and
        // returns the same CID), but count only those that were genuinely absent.
        let distinct: Vec<&String> = {
            let mut seen = HashSet::new();
            cf.chunk_cids().iter().filter(|c| seen.insert((*c).clone())).collect()
        };
        let mut missing = Vec::new();
        for cid in &distinct {
            // A cheap existence probe: try to fetch; absence (error) => must upload.
            if self.client.get_blob(cid).await.is_err() {
                missing.push((*cid).clone());
            }
        }
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

    /// Read a file from disk and store it (preserving mode/mtime where available).
    pub async fn put_file(&self, path: &Path) -> Result<StoredFile> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let meta = std::fs::metadata(path).ok();
        let mode = meta.as_ref().map(mode_of).unwrap_or(0o644);
        let mtime_ms = meta.as_ref().map(mtime_ms_of).unwrap_or(0);
        self.put_bytes(&bytes, mode, mtime_ms).await
    }

    /// Fetch a file by its **object CID** (the content cid recorded in the map). Resolves the
    /// manifest blob, then reassembles + verifies every chunk against its CID. This is the canonical
    /// read path and is equivalent to the SDK's `get_object` (kept here so the store owns both ends).
    pub async fn get_file(&self, object_cid: &str) -> Result<Vec<u8>> {
        self.client.get_object(object_cid).await.context("resolve object + verify chunks")
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
}
