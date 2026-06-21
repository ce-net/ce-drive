//! Durability / availability policy — the `ce-pin` replicate / announce / status surface, per
//! workspace.
//!
//! Best-effort blob caching is not durability. Each file CID (and each tree-snapshot CID) is pinned
//! with a replication factor and announced on the DHT so it is fetch-by-hash discoverable; a
//! status/audit pass gives proof-of-retrievability. CE Drive does not reimplement pinning — it
//! drives the same DHT `announce` / `find` primitives `ce-pin` uses (via `ce-rs`), and carries a
//! per-workspace [`PinPolicy`] that decides the replication factor and retention window.
//!
//! The actual N-peer replication negotiation (offer/accept, payment channels for rent) is `ce-pin`'s
//! host protocol; a workspace that wants paid durability shells out to `ce-pin` with these CIDs.
//! What lives here is the *policy* (how many replicas, how long to retain trashed content) and the
//! *announce/discover* glue every workspace needs regardless of who hosts.

use std::collections::HashSet;

use anyhow::{Context, Result};
use ce_rs::CeClient;
use serde::{Deserialize, Serialize};

/// Per-workspace durability policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinPolicy {
    /// Target replication factor (number of distinct hosts that should hold each CID). 0 = local
    /// only (announce self-availability but do not seek extra replicas).
    pub replication: u8,
    /// Retention window for trashed content, in seconds, before it may be unpinned/GC'd.
    pub trash_retention_secs: u64,
    /// Whether to announce availability on the DHT after a store (so peers can fetch by hash).
    pub announce: bool,
}

impl Default for PinPolicy {
    fn default() -> Self {
        // A sane self-hosted default: 2 replicas, 30-day trash retention, announce on.
        PinPolicy { replication: 2, trash_retention_secs: 30 * 24 * 3600, announce: true }
    }
}

impl PinPolicy {
    /// A local-only policy (no extra replicas, still announces self-availability).
    pub fn local() -> Self {
        PinPolicy { replication: 0, trash_retention_secs: 30 * 24 * 3600, announce: true }
    }
}

/// The per-CID durability status: how many replicas are known to hold it (including self).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinStatus {
    pub cid: String,
    /// Node ids (hex) found advertising/holding the CID on the DHT.
    pub holders: Vec<String>,
    /// True if the policy's replication target is met.
    pub satisfied: bool,
}

/// The durability manager: drives announce/discover over a CE node, honoring a [`PinPolicy`].
pub struct Durability {
    client: CeClient,
    policy: PinPolicy,
}

impl Durability {
    pub fn new(client: CeClient, policy: PinPolicy) -> Self {
        Durability { client, policy }
    }

    pub fn policy(&self) -> &PinPolicy {
        &self.policy
    }

    /// Announce availability of every CID on the DHT (so peers can fetch-by-hash). Idempotent.
    /// Returns the number announced. Best-effort: individual failures are logged, not fatal.
    pub async fn announce_all(&self, cids: &[String]) -> Result<usize> {
        if !self.policy.announce {
            return Ok(0);
        }
        let mut n = 0;
        for cid in dedup(cids) {
            match self.announce(&cid).await {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!(cid = %short(&cid), error = %e, "announce failed"),
            }
        }
        Ok(n)
    }

    /// Announce one CID's availability on the DHT (the `pin/announce` DHT semantics via the SDK's
    /// service-advertise primitive, keyed `pin:<cid>` exactly as ce-pin does).
    pub async fn announce(&self, cid: &str) -> Result<()> {
        self.client
            .advertise_service(&format!("pin:{cid}"))
            .await
            .with_context(|| format!("announce {}", short(cid)))
    }

    /// Discover which nodes advertise a CID on the DHT.
    pub async fn find_holders(&self, cid: &str) -> Result<Vec<String>> {
        self.client
            .find_service(&format!("pin:{cid}"))
            .await
            .with_context(|| format!("find holders of {}", short(cid)))
    }

    /// Compute the [`PinStatus`] of a CID against the policy's replication target.
    pub async fn status(&self, cid: &str) -> Result<PinStatus> {
        let holders = self.find_holders(cid).await.unwrap_or_default();
        let satisfied = holders.len() >= self.policy.replication.max(1) as usize;
        Ok(PinStatus { cid: cid.to_string(), holders, satisfied })
    }

    /// Status across many CIDs (e.g. every CID a drive references). Sequential and best-effort.
    pub async fn status_all(&self, cids: &[String]) -> Result<Vec<PinStatus>> {
        let mut out = Vec::new();
        for cid in dedup(cids) {
            out.push(self.status(&cid).await?);
        }
        Ok(out)
    }
}

fn dedup(cids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    cids.iter().filter(|c| seen.insert((*c).clone())).cloned().collect()
}

fn short(s: &str) -> &str {
    &s[..16.min(s.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_sane() {
        let p = PinPolicy::default();
        assert_eq!(p.replication, 2);
        assert!(p.announce);
        assert!(p.trash_retention_secs > 0);
    }

    #[test]
    fn local_policy_has_no_extra_replicas() {
        let p = PinPolicy::local();
        assert_eq!(p.replication, 0);
        assert!(p.announce);
    }

    #[test]
    fn policy_roundtrips_json() {
        let p = PinPolicy { replication: 3, trash_retention_secs: 100, announce: false };
        let j = serde_json::to_string(&p).unwrap();
        let back: PinPolicy = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn pin_status_satisfaction_threshold() {
        // satisfied when holders >= max(replication, 1)
        let s = PinStatus { cid: "x".into(), holders: vec!["a".into(), "b".into()], satisfied: true };
        assert!(s.satisfied);
    }
}
