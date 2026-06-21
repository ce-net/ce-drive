//! `ce-drive` — the CLI face of CE Drive (M1 + M2).
//!
//! A single-writer drive lives on disk as a JSON [`DriveState`] (the move log + content log) under
//! the data dir; the binary replays it, applies one operation, and persists. Storage operations
//! (`add`, `sync`) talk to a local CE node via `ce-rs` (chunk → delta-upload → record). Sharing
//! (`share`) mints `ce-cap` grants from the workspace identity and journals them for `history`.
//!
//! Commands: `init | add | ls | tree | mv | rm | history | share | sync`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ce_drive_core::audit::{Audit, AuditJournal, GrantRecord};
use ce_drive_core::{Ability, Drive, DriveState, Store, Workspace};
use ce_drive_mount::store_iface::CoreStore;
use ce_identity::Identity;
use ce_rs::CeClient;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ce-drive", version, about = "CE Drive — distributed FS + Google-Drive replacement")]
struct Cli {
    /// The drive name (selects `<data_dir>/ce-drive/<name>.json`).
    #[arg(long, global = true, default_value = "default")]
    drive: String,

    /// Base URL of the local CE node (for storage/sharing).
    #[arg(long, global = true, default_value = "http://127.0.0.1:8844")]
    node: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize a new (empty) drive owned by this device.
    Init,
    /// Add (or update) a file at `dest` from a local `src` file, chunked + stored on the node.
    Add {
        /// Local file to upload.
        src: PathBuf,
        /// Destination path in the drive (e.g. `/docs/spec.md`). Defaults to `/` + filename.
        dest: Option<String>,
    },
    /// List a directory (default `/`).
    Ls {
        #[arg(default_value = "/")]
        path: String,
    },
    /// Print the whole drive as a tree.
    Tree,
    /// Move or rename a node: `mv <src> <dest>` where dest is a full path.
    Mv { src: String, dest: String },
    /// Delete a node (move to trash, recoverable).
    Rm { path: String },
    /// Show the sharing audit history (on-chain revocation cross-checked).
    History,
    /// Share a folder: mint a ce-cap grant for a recipient (or a share link).
    Share {
        /// Path scope (subtree) to share, e.g. `/docs`.
        path: String,
        /// Recipient node id hex (omit with --link for an anyone-with-link grant).
        #[arg(long)]
        to: Option<String>,
        /// Ability: read | comment | write | admin.
        #[arg(long, default_value = "read")]
        ability: String,
        /// Mint a share link (read/comment only) instead of a per-user grant.
        #[arg(long)]
        link: bool,
        /// Recipient/link-holder key for a link grant (a well-known link key).
        #[arg(long)]
        link_holder: Option<String>,
        /// Expiry in days (0 = never).
        #[arg(long, default_value_t = 0)]
        expires_days: u64,
    },
    /// Materialize the drive into a local directory (fetch every file by CID, verified).
    Sync {
        /// Local directory to materialize into.
        out: PathBuf,
    },
    /// Driverless fallback: materialize the whole drive into a real directory (works everywhere, no
    /// kernel driver — the default for CI/containers/no-admin machines).
    Materialize {
        /// Local directory to materialize into.
        out: PathBuf,
    },
    /// Driverless fallback: push local changes from a materialized directory back into the drive
    /// (re-chunk changed files, delta-upload, trash deletions). With `--watch`, repeat on an interval.
    Push {
        /// The materialized directory whose changes to sync back.
        dir: PathBuf,
        /// Keep watching and re-pushing every `--interval` seconds (poll-based watcher).
        #[arg(long)]
        watch: bool,
        /// Poll interval in seconds for `--watch`.
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// Mount the drive as a real filesystem (lazy hydrate + write-back). Requires the `fuse` feature
    /// on Linux; on macOS/Windows (and builds without `fuse`) it prints how to use `materialize`.
    Mount {
        /// Mountpoint directory.
        mountpoint: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let paths = Paths::resolve(&cli.drive)?;

    match cli.cmd {
        Cmd::Init => cmd_init(&paths).await,
        Cmd::Add { src, dest } => cmd_add(&paths, &cli.node, &src, dest).await,
        Cmd::Ls { path } => cmd_ls(&paths, &path),
        Cmd::Tree => cmd_tree(&paths),
        Cmd::Mv { src, dest } => cmd_mv(&paths, &src, &dest),
        Cmd::Rm { path } => cmd_rm(&paths, &path),
        Cmd::History => cmd_history(&paths, &cli.node).await,
        Cmd::Share { path, to, ability, link, link_holder, expires_days } => {
            cmd_share(&paths, &path, to, &ability, link, link_holder, expires_days)
        }
        Cmd::Sync { out } => cmd_sync(&paths, &cli.node, &out).await,
        Cmd::Materialize { out } => cmd_materialize(&paths, &cli.node, &out).await,
        Cmd::Push { dir, watch, interval } => cmd_push(&paths, &cli.node, &dir, watch, interval).await,
        Cmd::Mount { mountpoint } => cmd_mount(&paths, &cli.node, &mountpoint).await,
    }
}

// ---- commands ----

async fn cmd_init(paths: &Paths) -> Result<()> {
    if paths.state_file.exists() {
        bail!("drive '{}' already exists at {}", paths.name, paths.state_file.display());
    }
    let identity = paths.identity()?;
    let drive = Drive::init(&paths.name, &identity.node_id_hex());
    paths.save(&drive)?;
    println!("initialized drive '{}' (owner {})", paths.name, &identity.node_id_hex()[..16]);
    println!("state: {}", paths.state_file.display());
    Ok(())
}

async fn cmd_add(paths: &Paths, node: &str, src: &Path, dest: Option<String>) -> Result<()> {
    let mut drive = paths.load()?;
    let store = Store::new(client(node)?);
    let stored = store.put_file(src).await.context("chunk + store file")?;
    let dest = dest.unwrap_or_else(|| {
        let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        format!("/{name}")
    });
    let (parent, name) = split_path(&dest)?;
    ensure_dirs(&mut drive, &parent)?;
    drive.add_file(&parent, &name, stored.content.clone())?;
    paths.save(&drive)?;
    println!(
        "added {dest}  cid={}  size={}  ({} chunk(s) uploaded)",
        &stored.content.cid[..16.min(stored.content.cid.len())],
        stored.content.size,
        stored.uploaded_chunks
    );
    Ok(())
}

fn cmd_ls(paths: &Paths, path: &str) -> Result<()> {
    let drive = paths.load()?;
    for e in drive.ls(path)? {
        if e.is_dir {
            println!("{}/", e.name);
        } else {
            let (cid, size) = e
                .content
                .as_ref()
                .map(|c| (c.cid.clone(), c.size))
                .unwrap_or_else(|| ("-".into(), 0));
            println!("{}\t{}\t{}", e.name, size, &cid[..16.min(cid.len())]);
        }
    }
    Ok(())
}

fn cmd_tree(paths: &Paths) -> Result<()> {
    let drive = paths.load()?;
    println!("/");
    print_tree(&drive, "/", "");
    Ok(())
}

fn print_tree(drive: &Drive, path: &str, prefix: &str) {
    let Ok(entries) = drive.ls(path) else { return };
    let n = entries.len();
    for (i, e) in entries.into_iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "└── " } else { "├── " };
        if e.is_dir {
            println!("{prefix}{branch}{}/", e.name);
            let child_path = join(path, &e.name);
            let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            print_tree(drive, &child_path, &child_prefix);
        } else {
            let size = e.content.as_ref().map(|c| c.size).unwrap_or(0);
            println!("{prefix}{branch}{} ({} bytes)", e.name, size);
        }
    }
}

fn cmd_mv(paths: &Paths, src: &str, dest: &str) -> Result<()> {
    let mut drive = paths.load()?;
    let (dst_parent, dst_name) = split_path(dest)?;
    drive.mv(src, &dst_parent, &dst_name)?;
    paths.save(&drive)?;
    println!("moved {src} -> {dest}");
    Ok(())
}

fn cmd_rm(paths: &Paths, path: &str) -> Result<()> {
    let mut drive = paths.load()?;
    drive.rm(path)?;
    paths.save(&drive)?;
    println!("trashed {path}");
    Ok(())
}

async fn cmd_history(paths: &Paths, node: &str) -> Result<()> {
    let journal = paths.load_journal()?;
    if journal.grants.is_empty() {
        println!("no shares issued yet");
        return Ok(());
    }
    let now = now_secs();
    // Try the node for live revocation; fall back to journal-only if offline.
    let entries = match client(node) {
        Ok(c) => Audit::new(c).render(&journal, now).await.unwrap_or_else(|_| render_offline(&journal, now)),
        Err(_) => render_offline(&journal, now),
    };
    for e in entries {
        let status = if e.revoked {
            "REVOKED"
        } else if e.expired {
            "expired"
        } else {
            "active"
        };
        println!(
            "{}\t{} -> {}\t{:?}\t{}\t[{}]",
            e.record.issued_at,
            &e.record.issuer[..16.min(e.record.issuer.len())],
            &e.record.audience[..16.min(e.record.audience.len())],
            e.record.ability,
            e.record.scope,
            status
        );
    }
    Ok(())
}

fn render_offline(journal: &AuditJournal, now: u64) -> Vec<ce_drive_core::audit::AuditEntry> {
    journal
        .grants
        .iter()
        .map(|g| ce_drive_core::audit::AuditEntry {
            revoked: journal.revokes.iter().any(|r| r.issuer == g.issuer && r.revoke_nonce == g.revoke_nonce),
            expired: g.expires != 0 && now > g.expires,
            record: g.clone(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn cmd_share(
    paths: &Paths,
    path: &str,
    to: Option<String>,
    ability: &str,
    link: bool,
    link_holder: Option<String>,
    expires_days: u64,
) -> Result<()> {
    // Verify the path exists in the drive before sharing it.
    let drive = paths.load()?;
    if drive.tree().resolve(path).is_none() {
        bail!("no such path in drive: {path}");
    }
    let identity = paths.identity()?;
    let issuer_hex = identity.node_id_hex();
    let ws = Workspace::new(identity);
    let ability = Ability::parse(ability).ok_or_else(|| anyhow::anyhow!("unknown ability '{ability}' (read|comment|write|admin)"))?;
    let expires = if expires_days == 0 { 0 } else { now_secs() + expires_days * 24 * 3600 };
    let nonce = now_secs(); // unique-enough per issuer; the revocation address

    let (grant, is_link) = if link {
        let holder = link_holder.ok_or_else(|| anyhow::anyhow!("--link requires --link-holder <key-hex>"))?;
        (ws.share_link(&holder, ability, path, expires, nonce)?, true)
    } else {
        let to = to.ok_or_else(|| anyhow::anyhow!("provide --to <node-id-hex> or use --link"))?;
        (ws.grant(&to, ability, path, expires, nonce)?, false)
    };

    // Journal the grant so `history` can render it.
    let mut journal = paths.load_journal()?;
    journal.record_grant(GrantRecord::from_grant(&issuer_hex, now_secs(), &grant, is_link));
    paths.save_journal(&journal)?;

    println!("granted {:?} on {} to {}", grant.ability, grant.scope, grant.audience);
    if expires != 0 {
        println!("expires: {expires} (unix)");
    }
    println!("revoke with on-chain RevokeCapability for issuer={} nonce={}", &issuer_hex[..16], grant.revoke_nonce);
    println!("token: {}", grant.token);
    Ok(())
}

async fn cmd_sync(paths: &Paths, node: &str, out: &Path) -> Result<()> {
    let drive = paths.load()?;
    let store = Store::new(client(node)?);
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let mut count = 0usize;
    materialize(&drive, &store, "/", out, &mut count).await?;
    println!("materialized {count} file(s) into {}", out.display());
    Ok(())
}

/// Recursively materialize a drive subtree into `out`, fetching each file by CID (verified) via the
/// store. Directories become real directories. Refetches the manifest from the node's blob store.
async fn materialize(drive: &Drive, store: &Store, path: &str, out: &Path, count: &mut usize) -> Result<()> {
    for e in drive.ls(path)? {
        let target = out.join(&e.name);
        if e.is_dir {
            std::fs::create_dir_all(&target)?;
            let child = join(path, &e.name);
            Box::pin(materialize(drive, store, &child, &target, count)).await?;
        } else if let Some(content) = &e.content {
            // Resolve the manifest blob (its hash is the object CID indirection put_blob stored).
            // We re-chunk-by-fetch: get the object by reassembling its manifest. The manifest blob
            // was published in store.put_bytes; fetch it by reconstructing from the file CID via the
            // node's object resolution (get_object resolves manifest+chunks). We use the SDK's
            // get_object which handles the manifest indirection and per-chunk CID verification.
            let bytes = store
                .client()
                .get_object(&content.cid)
                .await
                .with_context(|| format!("fetch {}", e.name))?;
            std::fs::write(&target, &bytes)?;
            *count += 1;
        }
    }
    Ok(())
}

async fn cmd_materialize(paths: &Paths, node: &str, out: &Path) -> Result<()> {
    let drive = paths.load()?;
    let store = CoreStore::new(Store::new(client(node)?));
    let report = ce_drive_mount::materialize(&drive, &store, out).await?;
    println!(
        "materialized {} file(s), {} dir(s), {} bytes into {}",
        report.files,
        report.dirs,
        report.bytes,
        out.display()
    );
    Ok(())
}

async fn cmd_push(paths: &Paths, node: &str, dir: &Path, watch: bool, interval: u64) -> Result<()> {
    let mut drive = paths.load()?;
    let store = CoreStore::new(Store::new(client(node)?));
    if watch {
        println!("watching {} (every {interval}s) — Ctrl-C to stop", dir.display());
        // Persist after each push; never stop (until the process is signalled).
        ce_drive_mount::watch(
            &mut drive,
            &store,
            dir,
            std::time::Duration::from_secs(interval.max(1)),
            |d, report| {
                if report.changed > 0 || report.removed > 0 {
                    if let Err(e) = paths.save(d) {
                        eprintln!("warning: failed to persist drive state: {e:#}");
                    }
                    println!(
                        "pushed: {} changed, {} removed, {} chunk(s) uploaded",
                        report.changed, report.removed, report.uploaded_chunks
                    );
                }
            },
            || false,
        )
        .await?;
        Ok(())
    } else {
        let report = ce_drive_mount::push(&mut drive, &store, dir).await?;
        paths.save(&drive)?;
        println!(
            "pushed: {} changed, {} removed, {} chunk(s) uploaded",
            report.changed, report.removed, report.uploaded_chunks
        );
        Ok(())
    }
}

async fn cmd_mount(paths: &Paths, node: &str, mountpoint: &Path) -> Result<()> {
    let drive = paths.load()?;
    let store = CoreStore::new(Store::new(client(node)?));
    let vfs = ce_drive_mount::Vfs::new(drive, store);

    #[cfg(feature = "fuse")]
    {
        use ce_drive_mount::linux_fuser;
        std::fs::create_dir_all(mountpoint)
            .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;
        println!("mounting drive '{}' at {} (Ctrl-C / umount to stop)", paths.name, mountpoint.display());
        // The fuser mount blocks; on unmount persist the (possibly dirty) model back to disk.
        let paths_for_save = paths.clone_for_save();
        linux_fuser::mount(vfs, mountpoint, &paths.name, move |state| {
            if let Err(e) = paths_for_save.save_state(&state) {
                eprintln!("warning: failed to persist drive state on unmount: {e:#}");
            }
        })?;
        Ok(())
    }

    #[cfg(not(feature = "fuse"))]
    {
        let _ = (vfs, mountpoint);
        bail!(
            "this build has no kernel mount driver (the `fuse` feature is off, or this OS adapter is a \
             stub). Use the driverless fallback instead:\n  \
             ce-drive --drive {} materialize {}\n  \
             # edit/build natively, then:\n  \
             ce-drive --drive {} push {} --watch\n\
             On Linux, rebuild with `--features fuse` (libfuse required) for a real mount.",
            paths.name,
            mountpoint.display(),
            paths.name,
            mountpoint.display(),
        )
    }
}

// ---- paths / persistence ----

struct Paths {
    name: String,
    state_file: PathBuf,
    journal_file: PathBuf,
    identity_dir: PathBuf,
}

impl Paths {
    fn resolve(name: &str) -> Result<Self> {
        let base = std::env::var_os("CE_DRIVE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs_next::data_dir().map(|d| d.join("ce-drive")))
            .unwrap_or_else(|| PathBuf::from(".ce-drive"));
        std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;
        let identity_dir = std::env::var_os("CE_IDENTITY_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs_next::data_dir().map(|d| d.join("ce").join("identity")))
            .unwrap_or_else(|| base.join("identity"));
        Ok(Paths {
            name: name.to_string(),
            state_file: base.join(format!("{name}.json")),
            journal_file: base.join(format!("{name}.audit.json")),
            identity_dir,
        })
    }

    fn identity(&self) -> Result<Identity> {
        Identity::load_or_generate(&self.identity_dir)
            .with_context(|| format!("load identity from {}", self.identity_dir.display()))
    }

    fn load(&self) -> Result<Drive> {
        let bytes = std::fs::read(&self.state_file)
            .with_context(|| format!("drive '{}' not found — run `ce-drive init` (looked at {})", self.name, self.state_file.display()))?;
        let state: DriveState = serde_json::from_slice(&bytes).context("parse drive state")?;
        Ok(Drive::from_state(state))
    }

    fn save(&self, drive: &Drive) -> Result<()> {
        self.save_state(drive.state())
    }

    /// Persist a [`DriveState`] directly (the mount-unmount path hands back a state, not a `Drive`).
    fn save_state(&self, state: &DriveState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).context("serialize drive state")?;
        write_atomic(&self.state_file, &bytes)
    }

    /// A minimal clone of the persistence handle (the unmount callback is `move` and outlives the
    /// borrow of `self`). Only the fields needed to save are carried.
    #[cfg(feature = "fuse")]
    fn clone_for_save(&self) -> Paths {
        Paths {
            name: self.name.clone(),
            state_file: self.state_file.clone(),
            journal_file: self.journal_file.clone(),
            identity_dir: self.identity_dir.clone(),
        }
    }

    fn load_journal(&self) -> Result<AuditJournal> {
        match std::fs::read_to_string(&self.journal_file) {
            Ok(s) => AuditJournal::from_json(&s),
            Err(_) => Ok(AuditJournal::new()),
        }
    }

    fn save_journal(&self, journal: &AuditJournal) -> Result<()> {
        write_atomic(&self.journal_file, journal.to_json()?.as_bytes())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

// ---- helpers ----

fn client(node: &str) -> Result<CeClient> {
    Ok(CeClient::new(node.to_string()))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Create every missing directory along an absolute path (mkdir -p), so `add /a/b/c.txt` works
/// without a separate mkdir step (the natural Drive behavior).
fn ensure_dirs(drive: &mut Drive, dir: &str) -> Result<()> {
    if dir == "/" || drive.tree().resolve(dir).is_some() {
        return Ok(());
    }
    let mut cur = String::from("/");
    for comp in dir.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
        let child = join(&cur, comp);
        if drive.tree().resolve(&child).is_none() {
            drive.mkdir(&cur, comp)?;
        }
        cur = child;
    }
    Ok(())
}

/// Split an absolute path into `(parent, name)`. Errors on `/` (no name).
fn split_path(path: &str) -> Result<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some((p, n)) if !n.is_empty() => (if p.is_empty() { "/" } else { p }, n),
        _ => bail!("invalid destination path '{path}' (expected /dir/name)"),
    };
    Ok((parent.to_string(), name.to_string()))
}

/// Join a directory path and a child name into a normalized absolute path.
fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}
