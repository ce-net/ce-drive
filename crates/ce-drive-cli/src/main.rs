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
    /// List the trash (deleted-but-recoverable nodes).
    Trash,
    /// Restore a trashed node back to a live location.
    Restore {
        /// The node id of the trashed item (from `trash`).
        node_id: String,
        /// Destination directory (defaults to `/`).
        #[arg(long)]
        to: Option<String>,
        /// New name (defaults to its original name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Permanently delete trashed nodes. Without `--older-than`, empties the whole trash; with it,
    /// only nodes whose newest content is older than the given seconds are hard-deleted (GC).
    EmptyTrash {
        /// Only GC nodes whose content mtime is older than this many seconds (0 = empty all).
        #[arg(long, default_value_t = 0)]
        older_than: u64,
    },
    /// Search files and folders by name/path (case-folded, substring + trigram).
    Search {
        /// The query string.
        query: String,
        /// Maximum number of hits (0 = unbounded).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show changes since a cursor (a unix-ms `lamport:replica` cursor, or empty for full history).
    Changes {
        /// Resume cursor as `lamport:replica` (omit to replay all history).
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of changes (0 = unbounded).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// List files with unresolved concurrent-edit conflicts (Dropbox-style `.conflict` copies).
    Conflicts,
    /// Compact the persisted op log to a minimal equivalent (bounds state-file size + replay time).
    Compact,
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
        Cmd::Trash => cmd_trash(&paths),
        Cmd::Restore { node_id, to, name } => cmd_restore(&paths, &node_id, to, name),
        Cmd::EmptyTrash { older_than } => cmd_empty_trash(&paths, older_than),
        Cmd::Search { query, limit } => cmd_search(&paths, &query, limit),
        Cmd::Changes { since, limit } => cmd_changes(&paths, since, limit),
        Cmd::Conflicts => cmd_conflicts(&paths),
        Cmd::Compact => cmd_compact(&paths),
    }
}

// ---- commands ----

async fn cmd_init(paths: &Paths) -> Result<()> {
    if paths.exists() {
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

fn cmd_trash(paths: &Paths) -> Result<()> {
    let drive = paths.load()?;
    let items = drive.trash();
    if items.is_empty() {
        println!("trash is empty");
        return Ok(());
    }
    for e in items {
        let size = e.content.as_ref().map(|c| c.size).unwrap_or(0);
        let kind = if e.is_dir { "dir " } else { "file" };
        println!("{}\t{kind}\t{}\t{} bytes", e.node_id, e.name, size);
    }
    println!("\nrestore with: ce-drive restore <node_id> [--to /dir] [--name new]");
    Ok(())
}

fn cmd_restore(paths: &Paths, node_id: &str, to: Option<String>, name: Option<String>) -> Result<()> {
    let mut drive = paths.load()?;
    drive.restore(node_id, to.as_deref(), name.as_deref())?;
    paths.save(&drive)?;
    let path = drive.path_of(node_id).unwrap_or_else(|| node_id.to_string());
    println!("restored {node_id} -> {path}");
    Ok(())
}

fn cmd_empty_trash(paths: &Paths, older_than: u64) -> Result<()> {
    let mut drive = paths.load()?;
    let removed = if older_than == 0 {
        drive.empty_trash()
    } else {
        drive.gc_trash(now_secs(), older_than)
    };
    paths.save(&drive)?;
    println!("hard-deleted {} node(s)", removed.len());
    for id in &removed {
        println!("  {id}");
    }
    Ok(())
}

fn cmd_search(paths: &Paths, query: &str, limit: usize) -> Result<()> {
    let drive = paths.load()?;
    let idx = drive.search_index();
    let hits = idx.search(query, limit);
    if hits.is_empty() {
        println!("no matches for '{query}'");
        return Ok(());
    }
    for h in hits {
        let kind = if h.is_dir { "dir " } else { "file" };
        println!("{:.1}\t{kind}\t{}\t({} bytes)", h.score, h.path, h.size);
    }
    Ok(())
}

fn cmd_changes(paths: &Paths, since: Option<String>, limit: usize) -> Result<()> {
    use ce_drive_core::changes::Cursor;
    use ce_drive_core::tree::Timestamp;
    let drive = paths.load()?;
    let cursor = match since {
        None => Cursor::beginning(),
        Some(s) => {
            let (l, r) = s
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("--since must be 'lamport:replica' (e.g. 42:abcd...)"))?;
            let lamport: u64 = l.parse().context("parse lamport in --since cursor")?;
            Cursor::at(Timestamp::new(lamport, r))
        }
    };
    let (changes, next) = drive.changes_since(&cursor, limit);
    for c in &changes {
        let what = match &c.kind {
            ce_drive_core::changes::ChangeKind::Created { is_dir } => {
                if *is_dir { "created-dir".to_string() } else { "created".to_string() }
            }
            ce_drive_core::changes::ChangeKind::Moved => "moved".to_string(),
            ce_drive_core::changes::ChangeKind::Trashed => "trashed".to_string(),
            ce_drive_core::changes::ChangeKind::ContentChanged { size, .. } => {
                format!("content ({size} bytes)")
            }
            ce_drive_core::changes::ChangeKind::ContentRemoved => "content-removed".to_string(),
        };
        let path = c.path.clone().or_else(|| c.name.clone()).unwrap_or_else(|| c.node_id.clone());
        println!("{}:{}\t{what}\t{path}", c.ts.lamport, &c.ts.replica[..8.min(c.ts.replica.len())]);
    }
    if let Some(ts) = &next.last {
        println!("\nnext cursor: {}:{}", ts.lamport, ts.replica);
    } else {
        println!("\n(no changes)");
    }
    Ok(())
}

fn cmd_conflicts(paths: &Paths) -> Result<()> {
    let drive = paths.load()?;
    let conflicts = drive.content_conflicts();
    if conflicts.is_empty() {
        println!("no content conflicts");
        return Ok(());
    }
    for c in conflicts {
        println!("{}  (current cid {})", c.path, &c.current_cid[..16.min(c.current_cid.len())]);
        for cc in &c.conflict_cids {
            println!("  conflict copy: {}.conflict-{}", c.path, &cc[..8.min(cc.len())]);
        }
    }
    Ok(())
}

fn cmd_compact(paths: &Paths) -> Result<()> {
    let mut drive = paths.load()?;
    let before = drive.op_count();
    drive.compact();
    let after = drive.op_count();
    paths.save(&drive)?;
    println!("compacted op log: {before} -> {after} ops");
    Ok(())
}

// ---- paths / persistence ----

struct Paths {
    name: String,
    /// The canonical compact state file (`<name>.cedrive`, bincode+zstd).
    state_file: PathBuf,
    /// A legacy pretty-JSON state file (`<name>.json`) from earlier versions, migrated on first read.
    legacy_json: PathBuf,
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
            state_file: base.join(format!("{name}.cedrive")),
            legacy_json: base.join(format!("{name}.json")),
            journal_file: base.join(format!("{name}.audit.json")),
            identity_dir,
        })
    }

    /// True if this drive already has a persisted state (compact or legacy).
    fn exists(&self) -> bool {
        self.state_file.exists() || self.legacy_json.exists()
    }

    fn identity(&self) -> Result<Identity> {
        Identity::load_or_generate(&self.identity_dir)
            .with_context(|| format!("load identity from {}", self.identity_dir.display()))
    }

    fn load(&self) -> Result<Drive> {
        let state = self.load_state()?;
        Ok(Drive::from_state(state))
    }

    /// Load a [`DriveState`], preferring the compact bincode+zstd `.cedrive` file and transparently
    /// migrating a legacy pretty-JSON `.json` state on first read (so older drives keep working).
    fn load_state(&self) -> Result<DriveState> {
        // Preferred: compact bincode+zstd.
        if self.state_file.exists() {
            let bytes = std::fs::read(&self.state_file)
                .with_context(|| format!("read {}", self.state_file.display()))?;
            return decode_state(&bytes);
        }
        // Legacy migration: a pretty-JSON state from an earlier version.
        if self.legacy_json.exists() {
            let s = std::fs::read_to_string(&self.legacy_json)
                .with_context(|| format!("read {}", self.legacy_json.display()))?;
            let state: DriveState = serde_json::from_str(&s).context("parse legacy JSON drive state")?;
            return Ok(state);
        }
        bail!(
            "drive '{}' not found — run `ce-drive init` (looked at {})",
            self.name,
            self.state_file.display()
        )
    }

    fn save(&self, drive: &Drive) -> Result<()> {
        self.save_state(drive.state())
    }

    /// Persist a [`DriveState`] as bincode+zstd (per project standards: deterministic, compact). The
    /// write is atomic (temp + fsync + rename). A legacy `.json` file is removed after a successful
    /// migration so the drive is single-sourced going forward.
    fn save_state(&self, state: &DriveState) -> Result<()> {
        let bytes = encode_state(state)?;
        write_atomic(&self.state_file, &bytes)?;
        if self.legacy_json.exists() {
            let _ = std::fs::remove_file(&self.legacy_json);
        }
        Ok(())
    }

    /// A minimal clone of the persistence handle (the unmount callback is `move` and outlives the
    /// borrow of `self`). Only the fields needed to save are carried.
    #[cfg(feature = "fuse")]
    fn clone_for_save(&self) -> Paths {
        Paths {
            name: self.name.clone(),
            state_file: self.state_file.clone(),
            legacy_json: self.legacy_json.clone(),
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

/// A magic+version header so a `.cedrive` file is self-describing and a truncated/garbage file is
/// rejected with a clear error rather than a confusing bincode failure.
const CEDRIVE_MAGIC: &[u8; 8] = b"CEDRIVE1";

/// Encode a [`DriveState`] as `magic || bincode(state)` compressed with zstd level 3 (the project's
/// persistence standard: deterministic, compact). The magic is prepended *before* compression so the
/// decoder can sanity-check the decompressed payload.
fn encode_state(state: &DriveState) -> Result<Vec<u8>> {
    let body = bincode::serialize(state).context("bincode-encode drive state")?;
    let mut framed = Vec::with_capacity(body.len() + CEDRIVE_MAGIC.len());
    framed.extend_from_slice(CEDRIVE_MAGIC);
    framed.extend_from_slice(&body);
    zstd::stream::encode_all(framed.as_slice(), 3).context("zstd-compress drive state")
}

/// Decode a `.cedrive` blob produced by [`encode_state`]. Validates the magic header so a corrupt or
/// foreign file fails loudly instead of silently deserializing garbage.
fn decode_state(bytes: &[u8]) -> Result<DriveState> {
    let framed = zstd::stream::decode_all(bytes).context("zstd-decompress drive state")?;
    if framed.len() < CEDRIVE_MAGIC.len() || &framed[..CEDRIVE_MAGIC.len()] != CEDRIVE_MAGIC {
        bail!("not a valid .cedrive file (bad magic header) — refusing to load corrupt state");
    }
    bincode::deserialize(&framed[CEDRIVE_MAGIC.len()..]).context("bincode-decode drive state")
}

/// Atomically write `bytes` to `path`: write a sibling temp file, fsync it, then rename over the
/// target (rename is atomic on POSIX). A crash mid-write leaves either the old file or the new file
/// fully intact, never a truncated one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes).with_context(|| format!("write {}", tmp.display()))?;
        f.flush().with_context(|| format!("flush {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("fsync {}", tmp.display()))?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ce_drive_core::content::FileContent;

    fn sample_state() -> DriveState {
        let mut d = Drive::init("t", "replica-x");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "a.md", FileContent::new("cid-a", 10, 0o644, 1)).unwrap();
        d.state().clone()
    }

    #[test]
    fn encode_decode_roundtrips() {
        let state = sample_state();
        let bytes = encode_state(&state).unwrap();
        // It is real binary (zstd), not JSON.
        assert_ne!(bytes.first(), Some(&b'{'));
        let back = decode_state(&bytes).unwrap();
        // A re-encode is byte-identical (deterministic), and the replayed tree matches.
        assert_eq!(encode_state(&back).unwrap(), bytes);
        assert_eq!(
            Drive::from_state(back).ls("/docs").unwrap(),
            Drive::from_state(state).ls("/docs").unwrap()
        );
    }

    #[test]
    fn decode_rejects_garbage_and_bad_magic() {
        // Random bytes are not valid zstd -> error (not a panic).
        assert!(decode_state(b"not zstd at all").is_err());
        // Valid zstd but without our magic header -> rejected by the magic check.
        let no_magic = zstd::stream::encode_all(&b"hello world payload"[..], 3).unwrap();
        let err = decode_state(&no_magic).unwrap_err().to_string();
        assert!(err.contains("magic") || err.contains("decode"), "got: {err}");
    }

    #[test]
    fn write_atomic_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.cedrive");
        write_atomic(&p, b"first").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        // Overwrite atomically; no leftover .tmp file.
        write_atomic(&p, b"second-longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second-longer");
        assert!(!p.with_extension("tmp").exists(), "temp file is renamed away");
    }

    #[test]
    fn split_path_rejects_root() {
        assert!(split_path("/").is_err());
        assert_eq!(split_path("/a/b.txt").unwrap(), ("/a".to_string(), "b.txt".to_string()));
        assert_eq!(split_path("/top.txt").unwrap(), ("/".to_string(), "top.txt".to_string()));
    }
}
