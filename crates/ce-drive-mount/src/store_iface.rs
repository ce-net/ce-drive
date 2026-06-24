//! The block-store interface the VFS engine is generic over.
//!
//! In production the engine is driven by the real core [`Store`](ce_drive_core::Store) (chunk + delta
//! upload + CID-verified object fetch over a live CE node). In tests it is driven by a deterministic
//! in-memory [`MockStore`], so the lazy-hydration / write-back / inode-stability logic of
//! [`crate::vfs::Vfs`] is exercised with **no live node** — exactly the discipline the design's
//! testing section requires ("the vfs engine logic ... with a mock store").
//!
//! The trait is intentionally narrow: the VFS engine only needs to (1) fetch an object's bytes by
//! its content CID (verified, block-wise — the lazy read path) and (2) store bytes and get back the
//! recorded [`FileContent`] + an uploaded-chunk count (the write-back path). Both map 1:1 onto the
//! core [`Store`]'s `get_object` / `put_bytes`.

use anyhow::Result;
use ce_drive_core::content::FileContent;

/// The result of a write-back store: the content record to set on the node, plus how many chunks
/// actually moved over the wire (0 when the write fully deduped — the delta property).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    /// The content record (cid/size/mode/mtime) to record on the node.
    pub content: FileContent,
    /// Number of chunks newly uploaded.
    pub uploaded_chunks: usize,
}

/// What the VFS engine needs from a content store. `Send + Sync` so an async write-back task and the
/// syscall path can share it across threads.
#[allow(async_fn_in_trait)]
pub trait BlockStore: Clone + Send + Sync + 'static {
    /// Fetch an object's full bytes by its content CID, verifying every chunk against its CID. This
    /// is the lazy read path: called on the first `read` of a file (block-wise, CID-verified). An
    /// empty CID yields empty bytes (a freshly-created, never-flushed file).
    async fn get_object(&self, cid: &str) -> Result<Vec<u8>>;

    /// Chunk + delta-upload `bytes`, returning the [`PutResult`] (content record + uploaded count).
    /// This is the write-back path: called on flush. Only missing chunks move over the wire.
    async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult>;
}

/// The real block store: the core [`Store`](ce_drive_core::Store) over a live CE node, behind an
/// `Arc` so it is cheaply `Clone` for the async write-back task.
#[derive(Clone)]
pub struct CoreStore {
    inner: std::sync::Arc<ce_drive_core::Store>,
}

impl CoreStore {
    /// Wrap a core [`Store`](ce_drive_core::Store).
    pub fn new(store: ce_drive_core::Store) -> Self {
        CoreStore { inner: std::sync::Arc::new(store) }
    }
}

impl BlockStore for CoreStore {
    async fn get_object(&self, cid: &str) -> Result<Vec<u8>> {
        if cid.is_empty() {
            return Ok(Vec::new());
        }
        self.inner.get_file(cid).await
    }

    async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
        let stored = self.inner.put_bytes(bytes, mode, mtime_ms).await?;
        Ok(PutResult { content: stored.content, uploaded_chunks: stored.uploaded_chunks })
    }
}

// This is a shared test-support module (it exports `MockStore` for sibling integration tests via the
// `pub use` below), not a `#[cfg(test)] mod tests` of in-file unit tests — so the re-export that must
// follow it is intentional.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ce_rs::data::cid;

    /// A deterministic in-memory store for unit tests. Keys objects by their whole-bytes CID
    /// (`sha256` hex via the shared `ce_rs::data::cid`, so it matches the real keying), counts
    /// fetches and uploads (so tests can assert "hydrate-on-open pulls only touched blocks" and
    /// "write-back batches"), and tracks per-chunk presence for a real delta-upload count.
    #[derive(Clone, Default)]
    pub struct MockStore {
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        chunks: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        fetches: Arc<AtomicUsize>,
        puts: Arc<AtomicUsize>,
    }

    /// Fixed chunk size for the mock's delta accounting (mirrors the real 1 MiB boundary at a tiny
    /// size so tests can exercise multi-chunk delta cheaply).
    const MOCK_CHUNK: usize = 4;

    impl MockStore {
        pub fn new() -> Self {
            MockStore::default()
        }

        /// Synchronously store bytes (test setup), returning the object CID — the same CID
        /// `put_bytes` would assign, so a drive built with `put_sync` reads back through the engine.
        pub fn put_sync(&self, bytes: &[u8]) -> String {
            let id = cid(bytes);
            self.objects.lock().expect("lock").insert(id.clone(), bytes.to_vec());
            for c in bytes.chunks(MOCK_CHUNK) {
                self.chunks.lock().expect("lock").insert(cid(c), c.to_vec());
            }
            id
        }

        /// How many object fetches have happened (the lazy-hydration assertion lever).
        pub fn fetch_count(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }

        /// How many write-back uploads have happened (the write-back-batches assertion lever).
        pub fn put_count(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }
    }

    impl BlockStore for MockStore {
        async fn get_object(&self, cid: &str) -> Result<Vec<u8>> {
            if cid.is_empty() {
                return Ok(Vec::new());
            }
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.objects
                .lock()
                .expect("lock")
                .get(cid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("mock: no object {cid}"))
        }

        async fn put_bytes(&self, bytes: &[u8], mode: u32, mtime_ms: u64) -> Result<PutResult> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            let id = cid(bytes);
            self.objects.lock().expect("lock").insert(id.clone(), bytes.to_vec());
            // Count only genuinely-missing chunks (the delta property the real store has).
            let mut uploaded = 0usize;
            {
                let mut held = self.chunks.lock().expect("lock");
                for c in bytes.chunks(MOCK_CHUNK) {
                    let cc = cid(c);
                    if held.insert(cc, c.to_vec()).is_none() {
                        uploaded += 1;
                    }
                }
            }
            Ok(PutResult {
                content: FileContent::new(id, bytes.len() as u64, mode, mtime_ms),
                uploaded_chunks: uploaded,
            })
        }
    }
}

#[cfg(test)]
pub use test_support::MockStore;
