//! Live single-node storage round-trip — gated, skips gracefully without a running CE node.
//!
//! This is the M1 storage claim verified against a real node's blob layer: chunk a file, delta-
//! upload the missing chunks, then fetch it back by object CID with full per-chunk + whole-file CID
//! verification. It uses the local node at `$CE_NODE_URL` (default `http://127.0.0.1:8844`) and is
//! a no-op when the node is unreachable, so it never regresses the pure-unit suite in CI sandboxes.
//!
//! Run against a live node with:  `cargo test -p ce-drive-core --test live_store -- --nocapture`

use ce_drive_core::Store;
use ce_rs::CeClient;

fn node_url() -> String {
    std::env::var("CE_NODE_URL").unwrap_or_else(|_| "http://127.0.0.1:8844".to_string())
}

#[tokio::test]
async fn store_roundtrip_and_dedup_against_live_node() {
    let client = CeClient::new(node_url());
    // Skip if there is no reachable node (CI sandbox / no node running).
    if !client.health().await.unwrap_or(false) {
        eprintln!("SKIP: no CE node at {} (set CE_NODE_URL to run)", node_url());
        return;
    }

    let store = Store::new(client.clone());

    // A multi-chunk object so chunking + dedup are exercised.
    let bytes: Vec<u8> = (0..3_500_000u32).map(|i| (i % 251) as u8).collect();
    let stored = store.put_bytes(&bytes, 0o644, 1).await.expect("put_bytes");
    assert_eq!(stored.content.size, bytes.len() as u64);

    // Fetch back via the SDK object resolution (manifest + chunks, CID-verified) and compare.
    let fetched = client.get_object(&stored.content.cid).await.expect("get_object");
    assert_eq!(fetched, bytes, "round-trip bytes must match");

    // Storing identical bytes again uploads zero new chunks (global dedup).
    let again = store.put_bytes(&bytes, 0o644, 2).await.expect("put_bytes again");
    assert_eq!(again.uploaded_chunks, 0, "identical content dedups to zero uploads");
    assert_eq!(again.content.cid, stored.content.cid, "same content => same CID");
}
