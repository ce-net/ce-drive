//! A self-contained tour of the CE Drive core model — no live CE node required.
//!
//! Run with:
//! ```text
//! cargo run -p ce-drive-core --example drive_basics
//! ```
//!
//! It exercises the high-level [`Drive`] handle end to end against an in-memory model: directories
//! and files, edit-as-new-version, rename keeping identity, trash + restore + empty-trash, the change
//! feed, filename search, and offline persistence round-trip (the same `DriveState` bytes the CLI
//! writes to disk). Everything here is pure CRDT/model logic — the only thing that needs a node is the
//! content *store* (chunk upload/download), which this example deliberately does not touch.

use ce_drive_core::content::FileContent;
use ce_drive_core::{Cursor, Drive, DriveState};

fn fc(cid: &str, size: u64, mtime_ms: u64) -> FileContent {
    FileContent::new(cid, size, 0o644, mtime_ms)
}

fn main() -> anyhow::Result<()> {
    // 1. A fresh drive owned by this device (replica id = a node-id hex in real use).
    let mut drive = Drive::init("demo", "replica-a");

    // 2. Build a small tree: /docs/spec.md and /photos/trip.jpg.
    drive.mkdir("/", "docs")?;
    drive.mkdir("/", "photos")?;
    drive.add_file("/docs", "spec.md", fc("cid-spec-v1", 1200, 1_000))?;
    drive.add_file("/photos", "trip.jpg", fc("cid-trip", 2_400_000, 1_100))?;

    println!("== listing / ==");
    for e in drive.ls("/")? {
        println!("  {}{}", e.name, if e.is_dir { "/" } else { "" });
    }

    // 3. Editing a file by the same name is a NEW VERSION on the same stable node id (not a new node).
    let spec_id = drive.tree().resolve("/docs/spec.md").expect("spec resolves");
    drive.add_file("/docs", "spec.md", fc("cid-spec-v2", 1300, 2_000))?;
    let spec = drive.content().get(&spec_id).expect("spec content");
    println!("\nspec.md now cid={} with {} prior version(s)", spec.cid, spec.versions.len());

    // 4. Rename keeps identity: the node id is unchanged after a move.
    drive.mv("/docs/spec.md", "/docs", "design.md")?;
    assert_eq!(drive.tree().resolve("/docs/design.md").as_deref(), Some(spec_id.as_str()));
    println!("renamed spec.md -> design.md (same node id {})", &spec_id[..8.min(spec_id.len())]);

    // 5. Trash + restore round-trip (recoverable delete).
    drive.rm("/photos/trip.jpg")?;
    let trip_id = drive.trash()[0].node_id.clone();
    println!("\ntrashed trip.jpg; trash holds {} item(s)", drive.trash().len());
    drive.restore(&trip_id, Some("/"), Some("trip-restored.jpg"))?;
    println!("restored to {}", drive.path_of(&trip_id).unwrap_or_default());

    // 6. Hard delete (empty-trash) — unpins the content CIDs (GC candidates).
    drive.rm("/docs/design.md")?;
    let removed = drive.empty_trash();
    println!("\nempty-trash hard-deleted {} node(s)", removed.len());

    // 7. The change feed: a cursor-paginated, time-ordered delta stream (powers incremental sync).
    let (changes, next) = drive.changes_since(&Cursor::beginning(), 0);
    println!("\nchange feed has {} event(s); next cursor lamport = {:?}",
        changes.len(), next.last.as_ref().map(|t| t.lamport));

    // 8. Filename / path search (case-folded, substring + trigram; no network).
    let idx = drive.search_index();
    for hit in idx.search("trip", 5) {
        println!("search 'trip' -> {} (score {:.1})", hit.path, hit.score);
    }

    // 9. Offline persistence round-trip: the exact bytes the CLI saves/loads.
    let state: DriveState = drive.state().clone();
    let replayed = Drive::from_state(state);
    assert_eq!(replayed.ls("/")?, drive.ls("/")?, "replay reproduces the live tree exactly");
    println!("\npersisted + replayed identically ({} ops)", drive.op_count());

    Ok(())
}
