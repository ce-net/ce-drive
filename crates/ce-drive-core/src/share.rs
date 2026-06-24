//! Sharing + permissions — `ce-cap` attenuating, revocable capability chains, per folder.
//!
//! Sharing is CE's only authorization primitive: a signed, attenuating capability chain. There is no
//! ACL server and no device allowlist. **The org root key *is* the workspace** — every grant chains
//! to it; membership is the set of issued capabilities, not a list.
//!
//! Per-folder abilities are opaque strings scoped by a `path_prefix` caveat naming a subtree:
//! [`Ability::Read`] (`drive:read`), [`Ability::Comment`] (`drive:comment`), [`Ability::Write`]
//! (`drive:write`), [`Ability::Admin`] (`drive:admin`). Granting `drive:write` with
//! `path_prefix = "/docs"` lets the holder mutate the tree only under `/docs/` — the same caveat
//! rdev already enforces in `fs_action`. Attenuation makes transitive read-only re-share safe
//! (Tahoe-style): a holder of `drive:read` on a folder may re-share a strictly-attenuated
//! `drive:read` of a sub-folder, never widen to write — enforced cryptographically by [`authorize`].
//!
//! Enforcement reuses [`ce_cap::authorize`] **verbatim** (the rdev `handle_inner` pattern): any host
//! serving the durable blobs / a `ce-drive serve` endpoint calls [`authorize_path`] before applying
//! a tree/content op or serving bytes, exactly as rdev's host loop does today.

use std::collections::HashSet;

use anyhow::{Context, Result};
use ce_cap::{
    Caveats, Resource, SignedCapability, authorize, decode_chain, encode_chain,
};
use ce_identity::{Identity, NodeId};
use serde::{Deserialize, Serialize};

/// The four per-folder drive abilities (opaque strings to ce-cap; meaning lives here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ability {
    /// Read files/listings (`drive:read`).
    Read,
    /// Read + comment on docs (`drive:comment`).
    Comment,
    /// Mutate the tree/content under the scope (`drive:write`).
    Write,
    /// Manage sharing under the scope (`drive:admin`).
    Admin,
}

impl Ability {
    /// The opaque ability string ce-cap matches against.
    pub fn as_str(self) -> &'static str {
        match self {
            Ability::Read => "drive:read",
            Ability::Comment => "drive:comment",
            Ability::Write => "drive:write",
            Ability::Admin => "drive:admin",
        }
    }

    /// The set of ability strings a role implies. Higher roles imply the lower read/comment ones, so
    /// a single grant covers the natural sub-actions (a writer can also read).
    pub fn implied(self) -> Vec<String> {
        let all = match self {
            Ability::Read => &["drive:read"][..],
            Ability::Comment => &["drive:read", "drive:comment"][..],
            Ability::Write => &["drive:read", "drive:comment", "drive:write"][..],
            Ability::Admin => &["drive:read", "drive:comment", "drive:write", "drive:admin"][..],
        };
        all.iter().map(|s| s.to_string()).collect()
    }

    /// Parse an ability string back to an [`Ability`] (the highest one it names).
    pub fn parse(s: &str) -> Option<Ability> {
        match s {
            "drive:read" | "read" => Some(Ability::Read),
            "drive:comment" | "comment" => Some(Ability::Comment),
            "drive:write" | "write" => Some(Ability::Write),
            "drive:admin" | "admin" => Some(Ability::Admin),
            _ => None,
        }
    }
}

/// A share grant: the encoded capability chain token plus the human-facing scope/expiry it carries.
/// `link` is the same token packaged for "anyone with the link" sharing (embedded in a URL fragment
/// by the web app); a per-user grant and a share link differ only in audience (a specific node vs. a
/// well-known link-holder key the recipient imports).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The hex capability-chain token (root-first), as produced by [`encode_chain`].
    pub token: String,
    /// The path prefix the grant is scoped to (a subtree). `/` = whole drive.
    pub scope: String,
    /// The highest ability granted.
    pub ability: Ability,
    /// Audience node id hex (the recipient). For a share link this is the link-holder key.
    pub audience: String,
    /// Unix-seconds expiry (0 = never).
    pub expires: u64,
    /// The issuer's `(issuer_hex, nonce)` revocation address — feed to on-chain `RevokeCapability`.
    pub revoke_nonce: u64,
}

/// A workspace handle for minting and authorizing per-folder shares. The workspace root key is the
/// org/owner identity; every grant this device issues roots at it (self-delegation) or extends a
/// chain rooted at a configured org root.
pub struct Workspace {
    identity: Identity,
    /// Configured accepted roots (besides this identity, which is always implicitly accepted).
    accepted_roots: Vec<NodeId>,
}

impl Workspace {
    /// A workspace anchored at `identity` (the org/owner key = the workspace root).
    pub fn new(identity: Identity) -> Self {
        Workspace { identity, accepted_roots: Vec::new() }
    }

    /// Add a configured org root key (the enterprise CA anchor) this workspace honors in addition to
    /// its own identity.
    pub fn with_root(mut self, root: NodeId) -> Self {
        self.accepted_roots.push(root);
        self
    }

    /// This workspace's root node id hex.
    pub fn root_id_hex(&self) -> String {
        self.identity.node_id_hex()
    }

    /// Mint a per-folder [`Grant`] to `audience_hex` for `ability`, scoped to `scope` (a path
    /// prefix / subtree), expiring at `expires_unix` (0 = never). Self-issued (root = this
    /// workspace), the same form a personal/zero-config workspace uses.
    pub fn grant(
        &self,
        audience_hex: &str,
        ability: Ability,
        scope: &str,
        expires_unix: u64,
        nonce: u64,
    ) -> Result<Grant> {
        let audience = parse_node_id(audience_hex)?;
        let caveats = Caveats {
            not_after: expires_unix,
            path_prefix: Some(normalize_scope(scope)),
            ..Default::default()
        };
        let cap = SignedCapability::issue(
            &self.identity,
            audience,
            ability.implied(),
            Resource::Any,
            caveats,
            nonce,
            None,
        );
        Ok(Grant {
            token: encode_chain(&[cap]),
            scope: normalize_scope(scope),
            ability,
            audience: audience_hex.to_string(),
            expires: expires_unix,
            revoke_nonce: nonce,
        })
    }

    /// Mint a share-link grant: a `drive:read` (or `:comment`) capability scoped to `scope`,
    /// addressed to `link_holder_hex` (a well-known link key the recipient imports). "Anyone with
    /// the link" = whoever holds the link key embedded in the URL fragment.
    pub fn share_link(
        &self,
        link_holder_hex: &str,
        ability: Ability,
        scope: &str,
        expires_unix: u64,
        nonce: u64,
    ) -> Result<Grant> {
        // Share links are read-only or comment-only by policy (never write/admin).
        let ability = match ability {
            Ability::Write | Ability::Admin => Ability::Read,
            a => a,
        };
        self.grant(link_holder_hex, ability, scope, expires_unix, nonce)
    }

    /// Authorize an `action` (an ability string) by `requester` against `path`, given a presented
    /// capability `chain`, at time `now` (unix seconds), consulting the on-chain `revoked` set.
    /// Reuses [`ce_cap::authorize`] verbatim, then enforces the `path_prefix` caveat against `path`
    /// (with a `..` guard) — exactly the rdev `fs_action` discipline.
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_path(
        &self,
        requester_hex: &str,
        action: &str,
        path: &str,
        chain: &[SignedCapability],
        now: u64,
        revoked: &HashSet<(NodeId, u64)>,
    ) -> Result<(), String> {
        let requester = parse_node_id(requester_hex).map_err(|e| e.to_string())?;
        let self_id = self.identity.node_id();
        let is_revoked = |issuer: &NodeId, nonce: u64| revoked.contains(&(*issuer, nonce));
        authorize(
            &self_id,
            &self.accepted_roots,
            &[],
            now,
            &requester,
            action,
            chain,
            &is_revoked,
        )?;
        // Path-prefix caveat + traversal guard (the action-layer enforcement ce-cap delegates).
        enforce_path_scope(path, chain)?;
        Ok(())
    }
}

/// Enforce the leaf capability's `path_prefix` caveat against `path`, rejecting `..` traversal. The
/// caveat confines writes/reads to a subtree; an action that cannot honor it must reject. Mirrors
/// rdev's `fs_action` path safety.
pub fn enforce_path_scope(path: &str, chain: &[SignedCapability]) -> Result<(), String> {
    // Reject traversal and any platform path separator that could smuggle a component boundary the
    // `/`-split below would miss. CE Drive paths are `/`-separated; a backslash is never a legitimate
    // separator in a scope check, and a NUL/control char is always an injection attempt.
    if path.contains('\\') {
        return Err("path contains a backslash separator".into());
    }
    if path.chars().any(|c| (c as u32) < 0x20) {
        return Err("path contains a control character".into());
    }
    for comp in path.split('/') {
        if comp == ".." || comp == "." {
            return Err("path escapes the scope with a '.'/'..' component".into());
        }
    }
    if let Some(prefix) = chain.last().and_then(|c| c.cap.caveats.path_prefix.as_ref()) {
        let norm = normalize_scope(path);
        let pfx = normalize_scope(prefix);
        // `/` matches everything; otherwise the path must be the prefix or live *under* it. The
        // `{pfx}/` boundary check prevents a sibling-prefix bypass ("/docs" must not match
        // "/docs-secret"). CE Drive treats names case-sensitively (the underlying CRDT keys exact
        // bytes), so the scope check is byte-exact by design — see the module docs.
        let ok = pfx == "/" || norm == pfx || norm.starts_with(&format!("{pfx}/"));
        if !ok {
            return Err(format!("path '{norm}' is outside the granted scope '{pfx}'"));
        }
    }
    Ok(())
}

/// Decode a grant token to its capability chain (for presenting on a request or inspecting).
pub fn decode_grant(token: &str) -> Result<Vec<SignedCapability>> {
    decode_chain(token).context("decode grant token")
}

/// Normalize a path scope: ensure a single leading slash, no trailing slash (except root `/`).
fn normalize_scope(scope: &str) -> String {
    let trimmed = scope.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn parse_node_id(hex_str: &str) -> Result<NodeId> {
    let bytes = hex::decode(hex_str.trim()).context("node id is not valid hex")?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("node id must be 32 bytes (64 hex chars)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn id(tag: &str) -> Identity {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ce-drive-share-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Identity::load_or_generate(&dir).unwrap()
    }

    /// A second handle to the same on-disk key (an `Identity` for the owner is consumed by
    /// `Workspace::new`, but some tests also need the owner to issue a sub-cap directly). The node
    /// key is 32 raw secret bytes, so we materialize a sibling key dir with the same secret.
    fn second_handle(of: &Identity) -> Identity {
        let dir = std::env::temp_dir().join(format!(
            "ce-drive-share-2nd-{}-{}",
            std::process::id(),
            hex::encode(of.node_id())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("node.key");
        if !key_path.exists() {
            std::fs::write(&key_path, of.secret_bytes()).unwrap();
        }
        Identity::load_or_generate(&dir).unwrap()
    }

    fn no_revoked() -> HashSet<(NodeId, u64)> {
        HashSet::new()
    }

    #[test]
    fn self_issued_write_grant_authorizes_in_scope() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        // Write under /docs is allowed.
        assert!(ws.authorize_path(&alice.node_id_hex(), "drive:write", "/docs/spec.md", &chain, 1000, &no_revoked()).is_ok());
        // Read is implied by write.
        assert!(ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/spec.md", &chain, 1000, &no_revoked()).is_ok());
    }

    #[test]
    fn write_grant_denied_outside_scope() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        // A write to /secrets is outside the granted /docs subtree.
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:write", "/secrets/x", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains("outside the granted scope"));
    }

    #[test]
    fn dotdot_traversal_is_rejected() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:write", "/docs/../secrets", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains(".."));
    }

    #[test]
    fn read_grant_denied_for_write_action() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:write", "/docs/x", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains("does not grant"));
    }

    #[test]
    fn transitive_read_only_reshare_cannot_widen_to_write() {
        // owner -> alice (read on /docs). alice re-shares to bob, trying to widen to write.
        let owner = id("owner");
        let alice = id("alice");
        let bob = id("bob");
        let ws = Workspace::new(second_handle(&owner));
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 0, 1).unwrap();
        let mut chain = decode_grant(&g.token).unwrap();
        // alice issues a child capability widening to write (an attack the verifier must reject).
        let parent = chain[0].id();
        let widened = SignedCapability::issue(
            &alice,
            bob.node_id(),
            vec!["drive:read".into(), "drive:write".into()],
            Resource::Any,
            Caveats { path_prefix: Some("/docs".into()), ..Default::default() },
            2,
            Some(parent),
        );
        chain.push(widened);
        let r = ws.authorize_path(&bob.node_id_hex(), "drive:write", "/docs/x", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains("exceed the parent"), "attenuation must block widening");
    }

    #[test]
    fn valid_transitive_read_only_subfolder_reshare_works() {
        // owner -> alice (read on /docs). alice re-shares read of /docs/sub to bob — strictly narrower.
        let owner = id("owner");
        let alice = id("alice");
        let bob = id("bob");
        let ws = Workspace::new(second_handle(&owner));
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 0, 1).unwrap();
        let mut chain = decode_grant(&g.token).unwrap();
        let parent = chain[0].id();
        let narrower = SignedCapability::issue(
            &alice,
            bob.node_id(),
            vec!["drive:read".into()],
            Resource::Any,
            Caveats { path_prefix: Some("/docs/sub".into()), ..Default::default() },
            2,
            Some(parent),
        );
        chain.push(narrower);
        assert!(ws.authorize_path(&bob.node_id_hex(), "drive:read", "/docs/sub/a.md", &chain, 1000, &no_revoked()).is_ok());
        // bob still cannot read outside the narrowed sub-scope.
        assert!(ws.authorize_path(&bob.node_id_hex(), "drive:read", "/docs/other.md", &chain, 1000, &no_revoked()).is_err());
    }

    #[test]
    fn expired_grant_is_denied() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 500, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains("expired"));
    }

    #[test]
    fn revoked_grant_is_denied() {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 0, 42).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        let mut revoked = HashSet::new();
        revoked.insert((owner_node_id(&ws), 42u64));
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &revoked);
        assert!(r.unwrap_err().contains("revoked"));
    }

    #[test]
    fn share_link_forces_read_only() {
        let owner = id("owner");
        let link = id("link");
        let ws = Workspace::new(owner);
        // Asking for write on a share link downgrades to read.
        let g = ws.share_link(&link.node_id_hex(), Ability::Write, "/pub", 0, 7).unwrap();
        assert_eq!(g.ability, Ability::Read);
        let chain = decode_grant(&g.token).unwrap();
        assert!(ws.authorize_path(&link.node_id_hex(), "drive:read", "/pub/a", &chain, 1000, &no_revoked()).is_ok());
        assert!(ws.authorize_path(&link.node_id_hex(), "drive:write", "/pub/a", &chain, 1000, &no_revoked()).is_err());
    }

    fn owner_node_id(ws: &Workspace) -> NodeId {
        ws.identity.node_id()
    }

    #[test]
    fn sibling_prefix_does_not_bypass_scope() {
        // A grant on "/docs" must NOT authorize "/docs-secret/..." (a sibling sharing the prefix).
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        let r = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs-secret/x", &chain, 1000, &no_revoked());
        assert!(r.unwrap_err().contains("outside the granted scope"), "sibling-prefix must not bypass");
        // The legitimate child is allowed.
        assert!(ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &no_revoked()).is_ok());
    }

    #[test]
    fn backslash_and_control_chars_are_rejected() {
        let chain: Vec<SignedCapability> = Vec::new();
        assert!(enforce_path_scope("/docs\\..\\secret", &chain).is_err(), "backslash rejected");
        assert!(enforce_path_scope("/docs/\u{0000}x", &chain).is_err(), "NUL rejected");
        assert!(enforce_path_scope("/docs/./x", &chain).is_err(), "single-dot component rejected");
        assert!(enforce_path_scope("/docs/x", &chain).is_ok(), "clean path accepted");
    }
}
