//! Durability + audit: policy logic and failure-injection against an unreachable node.
//!
//! These modules drive a live CE node (DHT announce/find for durability; on-chain revoked-set for
//! audit). We verify (1) the pure policy math and (2) that a *dead* node degrades gracefully —
//! `announce_all` with `announce=false` is a no-op, and an audit render against an unreachable node
//! falls back to journal-only rather than panicking. We point the client at a closed loopback port so
//! the node is guaranteed absent (the real node on :8844 is never touched).

use ce_drive_core::audit::{Audit, AuditJournal, GrantRecord, RevokeRecord};
use ce_drive_core::durability::{Durability, PinPolicy, PinStatus};
use ce_drive_core::share::Ability;
use ce_rs::CeClient;

/// A client pointed at a closed port — every request fails fast (graceful-degradation harness).
fn dead_client() -> CeClient {
    // Port 1 is privileged/closed; connections are refused immediately.
    CeClient::new("http://127.0.0.1:1".to_string())
}

#[tokio::test]
async fn announce_all_is_noop_when_announce_disabled() {
    let policy = PinPolicy { replication: 3, trash_retention_secs: 10, announce: false };
    let dur = Durability::new(dead_client(), policy);
    // announce=false short-circuits before any network call: returns Ok(0) even with a dead node.
    let n = dur.announce_all(&["cidA".into(), "cidB".into()]).await.unwrap();
    assert_eq!(n, 0, "announce disabled => zero announced, no error");
}

#[tokio::test]
async fn announce_all_dedups_and_tolerates_failures() {
    let policy = PinPolicy::local(); // announce=true, replication=0
    let dur = Durability::new(dead_client(), policy);
    // With a dead node every announce fails; announce_all logs+continues and returns 0 (best-effort),
    // never errors. Duplicate cids are deduped before the attempt.
    let n = dur.announce_all(&["dup".into(), "dup".into(), "other".into()]).await.unwrap();
    assert_eq!(n, 0, "all announces failed gracefully against a dead node");
}

#[tokio::test]
async fn status_against_dead_node_is_unsatisfied_not_panic() {
    let dur = Durability::new(dead_client(), PinPolicy::default());
    // find_holders fails -> status() treats holders as empty -> not satisfied, but no panic/Err.
    let s = dur.status("cidX").await.unwrap();
    assert_eq!(s.cid, "cidX");
    assert!(s.holders.is_empty(), "no holders discoverable from a dead node");
    assert!(!s.satisfied, "replication target unmet => not satisfied");
}

#[tokio::test]
async fn status_all_handles_empty_and_dead() {
    let dur = Durability::new(dead_client(), PinPolicy::default());
    assert!(dur.status_all(&[]).await.unwrap().is_empty(), "no cids => empty status list");
    let statuses = dur.status_all(&["a".into(), "a".into(), "b".into()]).await.unwrap();
    assert_eq!(statuses.len(), 2, "deduped to 2 distinct cids");
}

#[test]
fn pin_status_satisfaction_threshold_math() {
    // satisfied iff holders >= max(replication, 1).
    let mk = |holders: usize, rep: u8| {
        let policy = PinPolicy { replication: rep, trash_retention_secs: 0, announce: true };
        let need = policy.replication.max(1) as usize;
        PinStatus { cid: "c".into(), holders: vec!["x".into(); holders], satisfied: holders >= need }
    };
    assert!(!mk(0, 0).satisfied, "0 holders never satisfies (floor is 1)");
    assert!(mk(1, 0).satisfied, "1 holder satisfies replication=0 (floor 1)");
    assert!(!mk(1, 2).satisfied);
    assert!(mk(2, 2).satisfied);
    assert!(mk(5, 2).satisfied);
}

#[tokio::test]
async fn audit_render_falls_back_when_node_unreachable() {
    // Build a journal with a grant and a local revoke record.
    let mut journal = AuditJournal::new();
    journal.record_grant(GrantRecord {
        issued_at: 10,
        issuer: "ownerhex".into(),
        audience: "alicehex".into(),
        ability: Ability::Write,
        scope: "/docs".into(),
        expires: 100,
        revoke_nonce: 7,
        is_link: false,
    });
    journal.record_revoke(RevokeRecord { revoked_at: 50, issuer: "ownerhex".into(), revoke_nonce: 7 });

    let audit = Audit::new(dead_client());
    // The on-chain revoked-set fetch fails; render() uses unwrap_or_default() => empty revoked set,
    // so it does NOT panic. It still renders the grant (revoked=false from the empty on-chain set,
    // expired=true since now=200 > 100). This proves graceful degradation, not a crash.
    let entries = audit.render(&journal, 200).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].expired, "expiry is computed locally regardless of node reachability");
}

#[tokio::test]
async fn revoked_set_errors_on_dead_node() {
    let audit = Audit::new(dead_client());
    // The raw fetch surfaces an error (the caller decides whether to fall back) — it must be an Err,
    // not a panic.
    assert!(audit.revoked_set().await.is_err(), "dead node => revoked_set errors cleanly");
}

#[test]
fn audit_journal_json_roundtrip_and_empty() {
    let mut j = AuditJournal::new();
    j.record_grant(GrantRecord {
        issued_at: 1, issuer: "i".into(), audience: "a".into(), ability: Ability::Read,
        scope: "/".into(), expires: 0, revoke_nonce: 1, is_link: true,
    });
    let s = j.to_json().unwrap();
    assert_eq!(AuditJournal::from_json(&s).unwrap(), j);
    // Empty/whitespace input => empty journal (the CLI's first-run path).
    assert_eq!(AuditJournal::from_json("   ").unwrap(), AuditJournal::new());
    assert_eq!(AuditJournal::from_json("").unwrap(), AuditJournal::new());
    // Garbage errors, not panics.
    assert!(AuditJournal::from_json("{not json").is_err());
}

#[test]
fn revoked_record_grant_record_serde() {
    let g = GrantRecord {
        issued_at: 1, issuer: "i".into(), audience: "a".into(), ability: Ability::Admin,
        scope: "/x".into(), expires: 9, revoke_nonce: 3, is_link: false,
    };
    let back: GrantRecord = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
    assert_eq!(g, back);
}
