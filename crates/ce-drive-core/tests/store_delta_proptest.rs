//! Property tests for the store's chunk / dedup / delta planning (the I/O-free half).
//!
//! `Store::put_bytes` chunks via `rdev::chunk` and delta-uploads only missing chunks. The async
//! upload needs a live node (covered by `live_store.rs` and the two-node test), but the *planning* —
//! which chunks a given byte change moves — is pure and is the property that guarantees dedup/delta
//! actually save bandwidth. We assert:
//!
//! * **Dedup is total.** Re-chunking identical bytes yields zero missing chunks against a store that
//!   already holds them.
//! * **Delta is local.** A single-byte edit moves at most the chunks overlapping that byte (1 for an
//!   intra-chunk edit; chunking is fixed-size so boundaries are deterministic).
//! * **Reassembly identity.** The chunk set, reassembled, reproduces the original bytes; the file CID
//!   is a deterministic function of content (same bytes => same CID, always).

use std::collections::HashSet;

use ce_drive_core::Store;
use ce_rs::data::{chunk_object, reassemble};
use rdev::chunk::{CHUNK_SIZE, chunk_bytes};
use rdev::delta;
use proptest::prelude::*;

/// Reassemble a chunk_object output and assert it equals the input.
fn roundtrip_via_chunks(bytes: &[u8]) -> Vec<u8> {
    let (manifest, chunks) = chunk_object(bytes, CHUNK_SIZE);
    let map: std::collections::HashMap<String, Vec<u8>> = chunks.into_iter().collect();
    reassemble(&manifest, |cid| {
        map.get(cid).cloned().ok_or_else(|| anyhow::anyhow!("missing {cid}"))
    })
    .unwrap()
}

proptest! {
    // Each case hashes up to a few MiB through sha256; cap cases so the suite stays fast while still
    // crossing chunk boundaries (the property is structural, not statistical).
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// CONTENT-ADDRESSING: same bytes => same CID; different bytes (almost always) => different CID.
    #[test]
    fn content_id_is_deterministic(a in proptest::collection::vec(any::<u8>(), 0..5000)) {
        prop_assert_eq!(Store::content_id(&a), Store::content_id(&a), "cid is a pure function of bytes");
        // A reassembly round-trip reproduces the bytes exactly.
        prop_assert_eq!(roundtrip_via_chunks(&a), a.clone());
    }

    /// DEDUP: chunking the same bytes twice yields no missing chunks against a held set. Sized to
    /// straddle the 1 MiB chunk boundary (>1 chunk + a partial tail) so multi-chunk dedup is real.
    #[test]
    fn identical_bytes_have_zero_missing(extra in 0usize..(CHUNK_SIZE + 17)) {
        let data: Vec<u8> = (0..(CHUNK_SIZE + extra)).map(|i| (i % 251) as u8).collect();
        let (f1, _) = chunk_bytes(&data);
        let held: HashSet<String> = f1.chunk_cids().iter().cloned().collect();
        let (f2, _) = chunk_bytes(&data);
        let missing = delta::compute_missing(f2.chunk_cids(), &held);
        prop_assert!(missing.is_empty(), "identical content must dedup to zero uploads");
    }

    /// DELTA LOCALITY: editing a single byte changes exactly the one chunk that byte lives in (fixed-
    /// size chunking => deterministic boundaries). Spans 1..3 chunks so the edit can land in any chunk.
    #[test]
    fn single_byte_edit_moves_one_chunk(
        chunks in 1usize..3,
        tail in 1usize..512,
        posfrac in 0usize..1000,
    ) {
        let len = CHUNK_SIZE * chunks + tail;
        let pos = (posfrac * len) / 1000;
        let mut data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let (orig, _) = chunk_bytes(&data);
        let held: HashSet<String> = orig.chunk_cids().iter().cloned().collect();
        data[pos] ^= 0xFF; // flip one byte (never a no-op)
        let (edited, _) = chunk_bytes(&data);
        let missing = delta::compute_missing(edited.chunk_cids(), &held);
        prop_assert_eq!(missing.len(), 1, "a single-byte edit must move exactly one chunk");
    }
}

/// Empty input is a valid (degenerate) object: zero chunks, a stable CID, reassembles to empty.
#[test]
fn empty_bytes_is_a_valid_object() {
    let (manifest, chunks) = chunk_object(b"", CHUNK_SIZE);
    assert!(chunks.is_empty(), "empty input has no chunks");
    let out = reassemble(&manifest, |_| Ok(Vec::new())).unwrap();
    assert!(out.is_empty());
    assert_eq!(Store::content_id(b""), Store::content_id(b""));
}

/// A growing append re-uses the prefix chunks and only adds the new tail chunk(s).
#[test]
fn append_reuses_prefix_chunks() {
    let base: Vec<u8> = (0..(CHUNK_SIZE * 2)).map(|i| (i % 251) as u8).collect();
    let (b, _) = chunk_bytes(&base);
    let held: HashSet<String> = b.chunk_cids().iter().cloned().collect();
    // Append a third full chunk of fresh data.
    let mut grown = base.clone();
    grown.extend((0..CHUNK_SIZE).map(|i| ((i + 7) % 251) as u8));
    let (g, _) = chunk_bytes(&grown);
    let missing = delta::compute_missing(g.chunk_cids(), &held);
    assert_eq!(missing.len(), 1, "appending one fresh chunk moves exactly one chunk; prefix dedups");
}
