//! Property / adversarial tests for sharing (`ce-cap` capability chains, per-folder scope).
//!
//! Sharing is the only authorization primitive; a bug here is a security hole. The properties:
//!
//! 1. **Attenuation can never amplify.** A re-shared (delegated) capability that claims an ability the
//!    parent did not grant, or a path scope wider than the parent's, must be REJECTED — for arbitrary
//!    ability/scope combinations and arbitrary delegation depth.
//! 2. **Path-prefix scope is exact.** An authorized action is allowed iff its path is the scope or
//!    lives strictly under it; siblings/parents are denied; `..` traversal is always denied.
//! 3. **Expiry is honored** at the second boundary, and **revocation** of any `(issuer, nonce)` in the
//!    chain denies the whole chain.
//! 4. **Malformed input** (bad hex audience, empty chain) errors rather than panics.

use std::collections::HashSet;

use ce_cap::{Caveats, Resource, SignedCapability, decode_chain, encode_chain};
use ce_drive_core::share::{Ability, Workspace, decode_grant, enforce_path_scope};
use ce_identity::{Identity, NodeId};
use proptest::prelude::*;

/// A fresh on-disk identity in a unique temp dir (the node key is 32 raw bytes).
fn id(tag: &str) -> Identity {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ce-drive-share-pt-{}-{n}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Identity::load_or_generate(&dir).unwrap()
}

/// A second handle to the same secret key (so the owner identity can both anchor a Workspace and
/// issue sub-caps directly in a test).
fn second_handle(of: &Identity) -> Identity {
    let dir = std::env::temp_dir().join(format!(
        "ce-drive-share-pt-2nd-{}-{}",
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

fn ability(i: usize) -> Ability {
    match i % 4 {
        0 => Ability::Read,
        1 => Ability::Comment,
        2 => Ability::Write,
        _ => Ability::Admin,
    }
}

/// Numeric rank of an ability (read < comment < write < admin) — the partial order delegation must
/// not increase.
fn rank(a: Ability) -> u8 {
    match a {
        Ability::Read => 0,
        Ability::Comment => 1,
        Ability::Write => 2,
        Ability::Admin => 3,
    }
}

proptest! {
    // Each case mints fresh on-disk Ed25519 identities (keygen + file I/O), so we cap cases lower
    // than the pure-CRDT proptests — the ability/scope spaces are small and fully covered at 64.
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// ATTENUATION NEVER AMPLIFIES: owner grants `parent_ability` on `/docs`; alice re-shares to bob
    /// claiming `child_ability`. The verifier must allow bob's `drive:write` action iff BOTH the
    /// parent and the child grant write (i.e. the effective ability is the min over the chain). A
    /// child claiming MORE than the parent must never let bob exceed the parent.
    #[test]
    fn delegation_cannot_exceed_parent(parent_i in 0usize..4, child_i in 0usize..4) {
        let owner = id("owner");
        let alice = id("alice");
        let bob = id("bob");
        let parent_ability = ability(parent_i);
        let child_ability = ability(child_i);

        let ws = Workspace::new(second_handle(&owner));
        let g = ws.grant(&alice.node_id_hex(), parent_ability, "/docs", 0, 1).unwrap();
        let mut chain = decode_grant(&g.token).unwrap();
        let parent_id = chain[0].id();
        // Alice mints a child cap claiming child_ability's implied set (possibly wider than parent).
        let child = SignedCapability::issue(
            &alice,
            bob.node_id(),
            child_ability.implied(),
            Resource::Any,
            Caveats { path_prefix: Some("/docs".into()), ..Default::default() },
            2,
            Some(parent_id),
        );
        chain.push(child);

        // ce-cap rule (verified in ce-cap::lib): a chain authorizes iff every child's ability set is
        // a SUBSET of its parent's (`abilities_subset`) AND the leaf grants the requested action.
        // The implied() sets are nested prefixes (read ⊂ comment ⊂ write ⊂ admin), so
        // child.implied() ⊆ parent.implied() exactly when rank(child) <= rank(parent). Therefore a
        // delegated `drive:write` succeeds iff the child did not try to widen past the parent
        // (rank(child) <= rank(parent)) AND the child itself grants write (rank(child) >= write).
        let r = ws.authorize_path(&bob.node_id_hex(), "drive:write", "/docs/x", &chain, 1000, &no_revoked());
        let not_widened = rank(child_ability) <= rank(parent_ability);
        let child_has_write = rank(child_ability) >= rank(Ability::Write);
        prop_assert_eq!(
            r.is_ok(), not_widened && child_has_write,
            "write allowed={} but parent={:?} child={:?} (delegation may never widen past parent)",
            r.is_ok(), parent_ability, child_ability
        );

        // CORE NON-AMPLIFICATION INVARIANT: bob can NEVER perform an action the PARENT (alice's grant
        // from owner) did not itself authorize, no matter what the child cap claims. So a successful
        // admin action implies the parent was admin; a successful write implies the parent had write.
        let admin = ws.authorize_path(&bob.node_id_hex(), "drive:admin", "/docs/x", &chain, 1000, &no_revoked());
        prop_assert!(
            admin.is_err() || rank(parent_ability) >= rank(Ability::Admin),
            "delegation amplified to admin beyond a non-admin parent={:?}", parent_ability
        );
        prop_assert!(
            r.is_err() || rank(parent_ability) >= rank(Ability::Write),
            "delegation amplified to write beyond a non-write parent={:?}", parent_ability
        );
    }

    /// PATH-PREFIX SCOPE is exact: with a grant on `/a/b`, only paths equal to or under `/a/b` pass
    /// enforce_path_scope; siblings and parents fail; `..` always fails.
    #[test]
    fn path_scope_is_exact(depth in 0usize..4, sub in 0usize..3) {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/a/b", 0, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();

        // In-scope path of varying depth.
        let mut in_scope = String::from("/a/b");
        for i in 0..depth {
            in_scope.push_str(&format!("/d{i}"));
        }
        prop_assert!(enforce_path_scope(&in_scope, &chain).is_ok(), "in-scope {in_scope} must pass");

        // Out-of-scope siblings/parents.
        let out = match sub {
            0 => "/a/c".to_string(),         // sibling
            1 => "/a".to_string(),           // parent
            _ => "/x/b".to_string(),         // unrelated
        };
        prop_assert!(enforce_path_scope(&out, &chain).is_err(), "out-of-scope {out} must fail");

        // `..` traversal in any position fails.
        prop_assert!(enforce_path_scope("/a/b/../../etc", &chain).is_err(), "dotdot must be rejected");
    }

    /// EXPIRY boundary is honored to the second.
    #[test]
    fn expiry_boundary(exp in 1u64..1_000_000, delta in 1u64..1000) {
        let owner = id("owner");
        let alice = id("alice");
        let ws = Workspace::new(owner);
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/docs", exp, 1).unwrap();
        let chain = decode_grant(&g.token).unwrap();
        // Strictly before expiry: ok. Strictly after: denied.
        let before = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, exp.saturating_sub(delta), &no_revoked());
        let after = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, exp + delta, &no_revoked());
        prop_assert!(before.is_ok(), "valid before expiry");
        prop_assert!(after.is_err(), "denied after expiry");
    }
}

/// REVOCATION of any cap in a multi-level chain denies the whole chain (root or leaf nonce).
#[test]
fn revocation_anywhere_in_chain_denies() {
    let owner = id("owner");
    let alice = id("alice");
    let bob = id("bob");
    let ws = Workspace::new(second_handle(&owner));
    let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 0, 100).unwrap();
    let mut chain = decode_grant(&g.token).unwrap();
    let parent_id = chain[0].id();
    let child = SignedCapability::issue(
        &alice,
        bob.node_id(),
        Ability::Read.implied(),
        Resource::Any,
        Caveats { path_prefix: Some("/docs".into()), ..Default::default() },
        200,
        Some(parent_id),
    );
    chain.push(child);

    // Baseline: bob can read.
    assert!(ws.authorize_path(&bob.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &no_revoked()).is_ok());

    // Revoke the ROOT (owner, 100): the whole chain dies.
    let mut revoked = HashSet::new();
    revoked.insert((owner.node_id(), 100u64));
    let r = ws.authorize_path(&bob.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &revoked);
    assert!(r.is_err(), "revoking the root denies the delegated leaf too");

    // Revoke only the LEAF (alice, 200): bob is denied, even though the root is fine.
    let mut revoked_leaf = HashSet::new();
    revoked_leaf.insert((alice.node_id(), 200u64));
    let r2 = ws.authorize_path(&bob.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &revoked_leaf);
    assert!(r2.is_err(), "revoking the leaf denies bob");
}

/// THREE-LEVEL valid attenuation: owner -> alice (write /docs) -> bob (read /docs/sub) -> carol
/// (read /docs/sub/deep). Each strictly narrower; carol can read deep but not /docs/sub/other.
#[test]
fn three_level_narrowing_chain() {
    let owner = id("owner");
    let alice = id("alice");
    let bob = id("bob");
    let carol = id("carol");
    let ws = Workspace::new(second_handle(&owner));

    let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 0, 1).unwrap();
    let mut chain = decode_grant(&g.token).unwrap();

    let p1 = chain[0].id();
    let lvl2 = SignedCapability::issue(
        &alice, bob.node_id(), Ability::Read.implied(), Resource::Any,
        Caveats { path_prefix: Some("/docs/sub".into()), ..Default::default() }, 2, Some(p1),
    );
    chain.push(lvl2);
    let p2 = chain[1].id();
    let lvl3 = SignedCapability::issue(
        &bob, carol.node_id(), Ability::Read.implied(), Resource::Any,
        Caveats { path_prefix: Some("/docs/sub/deep".into()), ..Default::default() }, 3, Some(p2),
    );
    chain.push(lvl3);

    assert!(ws.authorize_path(&carol.node_id_hex(), "drive:read", "/docs/sub/deep/x", &chain, 1000, &no_revoked()).is_ok());
    // carol cannot read a sibling of the deepest scope.
    assert!(ws.authorize_path(&carol.node_id_hex(), "drive:read", "/docs/sub/other", &chain, 1000, &no_revoked()).is_err());
    // carol certainly cannot write (level 2 already dropped to read-only).
    assert!(ws.authorize_path(&carol.node_id_hex(), "drive:write", "/docs/sub/deep/x", &chain, 1000, &no_revoked()).is_err());
}

/// MALFORMED INPUT: a non-hex / wrong-length audience errors (no panic), and an empty chain is denied.
#[test]
fn malformed_inputs_error_not_panic() {
    let owner = id("owner");
    let ws = Workspace::new(owner);
    // grant() with a bad audience hex errors.
    assert!(ws.grant("not-hex!!", Ability::Read, "/docs", 0, 1).is_err());
    assert!(ws.grant("dead", Ability::Read, "/docs", 0, 1).is_err(), "too-short hex rejected");

    // authorize_path with an empty chain is denied (not a panic).
    let alice = id("alice");
    let r = ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &[], 1000, &no_revoked());
    assert!(r.is_err(), "empty chain must be denied");

    // A garbage grant token fails to decode.
    assert!(decode_grant("zzz-not-a-token").is_err());
}

/// Scope normalization: trailing slashes / whitespace / missing leading slash all normalize to the
/// same canonical scope, so a grant on "docs/" and "/docs" enforce identically.
#[test]
fn scope_normalization_is_canonical() {
    let owner = id("owner");
    let alice = id("alice");
    let ws = Workspace::new(owner);
    for raw in ["/docs", "docs", "docs/", "  /docs/  ", "//docs//"] {
        let g = ws.grant(&alice.node_id_hex(), Ability::Read, raw, 0, 1).unwrap();
        assert_eq!(g.scope, "/docs", "scope {raw:?} normalized to /docs");
        let chain = decode_grant(&g.token).unwrap();
        assert!(ws.authorize_path(&alice.node_id_hex(), "drive:read", "/docs/x", &chain, 1000, &no_revoked()).is_ok());
    }
    // Root scope matches everything.
    let g = ws.grant(&alice.node_id_hex(), Ability::Read, "/", 0, 1).unwrap();
    assert_eq!(g.scope, "/");
    let chain = decode_grant(&g.token).unwrap();
    assert!(ws.authorize_path(&alice.node_id_hex(), "drive:read", "/anything/deep", &chain, 1000, &no_revoked()).is_ok());
}

/// Grant token round-trips through encode/decode_chain unchanged (wire-format stability).
#[test]
fn grant_token_roundtrips() {
    let owner = id("owner");
    let alice = id("alice");
    let ws = Workspace::new(owner);
    let g = ws.grant(&alice.node_id_hex(), Ability::Write, "/docs", 42, 7).unwrap();
    let chain = decode_grant(&g.token).unwrap();
    let reencoded = encode_chain(&chain);
    let chain2 = decode_chain(&reencoded).unwrap();
    assert_eq!(chain.len(), chain2.len());
    assert_eq!(chain[0].id(), chain2[0].id(), "cap id stable across encode/decode");
}
