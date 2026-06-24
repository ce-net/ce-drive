//! A change feed / cursor API — `changes_since(cursor) -> (Vec<Change>, next_cursor)`.
//!
//! Google Drive's product is, under the hood, a *Changes* API: clients poll `changes.list` with a
//! `pageToken`, get the deltas since that token, and advance the token. It is what powers efficient
//! incremental client sync, the activity feed, and "what changed while I was away" — without
//! re-reading the entire drive on every refresh.
//!
//! CE Drive already carries everything needed for this cheaply: both op logs (the tree's [`MoveOp`]
//! log and the content [`StampedContentOp`] log) are stamped with a strict total-order [`Timestamp`]
//! (Lamport + replica). So a **cursor is simply the highest [`Timestamp`] a client has already seen**,
//! and `changes_since(cursor)` is the merge of the two logs filtered to `ts > cursor`, returned in
//! ascending order. The same cursor works across reconnects and is monotone and replica-independent.
//!
//! This module is pure: it operates on a [`DriveState`] snapshot (or any pair of op logs) and a
//! [`DriveTree`] for path derivation, so it is exercised with no live node.

use serde::{Deserialize, Serialize};

use crate::content::ContentOp;
use crate::drive::DriveState;
use crate::sync::StampedContentOp;
use crate::tree::{DriveTree, MoveOp, NodeKind, TRASH, Timestamp};

/// An opaque, monotone cursor into the change feed: the highest [`Timestamp`] already delivered.
/// `None` (the [`Cursor::beginning`]) yields every change from the start of the drive's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cursor {
    /// The last timestamp seen, or `None` to start from the beginning.
    pub last: Option<Timestamp>,
}

impl Cursor {
    /// A cursor that starts from the very beginning of history (delivers all changes).
    pub fn beginning() -> Self {
        Cursor { last: None }
    }

    /// A cursor positioned just after `ts` (the next poll returns strictly-later changes).
    pub fn at(ts: Timestamp) -> Self {
        Cursor { last: Some(ts) }
    }

    /// True if `ts` is strictly after this cursor (i.e. it is an undelivered change).
    fn admits(&self, ts: &Timestamp) -> bool {
        match &self.last {
            None => true,
            Some(seen) => ts > seen,
        }
    }
}

/// The kind of a single change in the feed. Structural moves and content edits are unified into one
/// time-ordered stream so a client sees the true causal order of everything that happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// A node was created (its first move out of limbo into a parent).
    Created { is_dir: bool },
    /// A node was moved or renamed (a subsequent move that is not into TRASH).
    Moved,
    /// A node was deleted (moved into TRASH).
    Trashed,
    /// A node's file content changed (a new version / restore).
    ContentChanged { cid: String, size: u64 },
    /// A node's content record was removed (after GC / hard delete).
    ContentRemoved,
}

/// One change in the feed: a node id, what happened, when, and (best-effort) the node's current
/// derived path. The path is `None` if the node is no longer addressable (e.g. trashed or in limbo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The total-order timestamp of the change (also the cursor advance point).
    pub ts: Timestamp,
    /// The affected node id.
    pub node_id: String,
    /// What happened.
    pub kind: ChangeKind,
    /// The node's current derived absolute path, if it is still live and addressable.
    pub path: Option<String>,
    /// The node's current bare name, if known.
    pub name: Option<String>,
}

/// The merged, time-ordered change feed over a drive's two op logs. Pure: it does not own a node.
pub struct ChangeLog<'a> {
    move_log: &'a [MoveOp],
    content_log: &'a [StampedContentOp],
    tree: &'a DriveTree,
}

impl<'a> ChangeLog<'a> {
    /// Build a feed view over a [`DriveState`]'s logs plus the live [`DriveTree`] (for path/name
    /// derivation at the current converged state).
    pub fn new(state: &'a DriveState, tree: &'a DriveTree) -> Self {
        ChangeLog { move_log: &state.move_log, content_log: &state.content_log, tree }
    }

    /// Build a feed over explicit logs (used by the multi-writer surface, which holds merged unions).
    pub fn from_logs(
        move_log: &'a [MoveOp],
        content_log: &'a [StampedContentOp],
        tree: &'a DriveTree,
    ) -> Self {
        ChangeLog { move_log, content_log, tree }
    }

    /// Return every change strictly after `cursor`, in ascending timestamp order, plus the cursor to
    /// pass next time (the highest timestamp returned, or the input cursor if nothing changed).
    ///
    /// At most `limit` changes are returned; if more remain, `next` points just after the last one
    /// returned so a follow-up poll continues from there (pagination). `limit == 0` means unbounded.
    pub fn changes_since(&self, cursor: &Cursor, limit: usize) -> (Vec<Change>, Cursor) {
        // Track, per node, whether we have already seen a move for it (to classify Created vs Moved).
        // We must scan the *whole* move log in order, not just the post-cursor slice, to know whether
        // a node existed before the cursor.
        let mut seen_before: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut merged: Vec<Change> = Vec::new();

        // Structural changes.
        for op in self.move_log {
            let first = seen_before.insert(op.child.as_str());
            if !cursor.admits(&op.ts) {
                continue;
            }
            let kind = if op.new_parent == TRASH {
                ChangeKind::Trashed
            } else if first {
                ChangeKind::Created { is_dir: matches!(op.kind, NodeKind::Dir) }
            } else {
                ChangeKind::Moved
            };
            merged.push(Change {
                ts: op.ts.clone(),
                node_id: op.child.clone(),
                kind,
                path: None, // filled below from the live tree
                name: Some(op.new_name.clone()),
            });
        }

        // Content changes.
        for stamped in self.content_log {
            if !cursor.admits(&stamped.ts) {
                continue;
            }
            let (node_id, kind) = match &stamped.op {
                ContentOp::Set { id, cid, size, .. } => {
                    (id.clone(), ChangeKind::ContentChanged { cid: cid.clone(), size: *size })
                }
                ContentOp::Restore { id, cid, .. } => {
                    (id.clone(), ChangeKind::ContentChanged { cid: cid.clone(), size: 0 })
                }
                ContentOp::Remove { id } => (id.clone(), ChangeKind::ContentRemoved),
                ContentOp::SetDoc { id, .. } => {
                    // A doc attach is a content-metadata change; surface it as a content change.
                    (id.clone(), ChangeKind::ContentChanged { cid: String::new(), size: 0 })
                }
            };
            merged.push(Change {
                ts: stamped.ts.clone(),
                node_id,
                kind,
                path: None,
                name: None,
            });
        }

        // One canonical ascending order across both logs.
        merged.sort_by(|a, b| a.ts.cmp(&b.ts));

        // Paginate.
        if limit != 0 && merged.len() > limit {
            merged.truncate(limit);
        }

        // Fill in the current derived path/name from the live tree.
        for ch in &mut merged {
            if ch.path.is_none() {
                ch.path = self.tree.path(&ch.node_id);
            }
            if ch.name.is_none() {
                ch.name = self.tree.edge(&ch.node_id).map(|e| e.name.clone());
            }
        }

        let next = merged.last().map(|c| Cursor::at(c.ts.clone())).unwrap_or_else(|| cursor.clone());
        (merged, next)
    }

    /// The current head cursor (just past the highest timestamp in either log). A client that wants
    /// only *future* changes opens here rather than replaying history.
    pub fn head(&self) -> Cursor {
        let m = self.move_log.iter().map(|o| &o.ts).max();
        let c = self.content_log.iter().map(|o| &o.ts).max();
        let head = match (m, c) {
            (Some(a), Some(b)) => Some(a.max(b).clone()),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
        Cursor { last: head }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::FileContent;
    use crate::drive::Drive;

    fn fc(cid: &str, size: u64) -> FileContent {
        FileContent::new(cid, size, 0o644, 1)
    }

    #[test]
    fn feed_orders_creates_then_edits_then_trash() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.add_file("/docs", "a.md", fc("cid1", 10)).unwrap();
        d.add_file("/docs", "a.md", fc("cid2", 20)).unwrap(); // edit
        d.rm("/docs/a.md").unwrap();

        let state = d.state().clone();
        let feed = ChangeLog::new(&state, d.tree());
        let (changes, next) = feed.changes_since(&Cursor::beginning(), 0);
        // Created docs, Created a.md, ContentChanged cid1, ContentChanged cid2, Trashed a.md.
        let kinds: Vec<&ChangeKind> = changes.iter().map(|c| &c.kind).collect();
        assert!(matches!(kinds[0], ChangeKind::Created { is_dir: true }));
        assert!(matches!(kinds[1], ChangeKind::Created { is_dir: false }));
        assert!(changes.iter().any(|c| matches!(&c.kind, ChangeKind::ContentChanged { cid, .. } if cid == "cid2")));
        assert!(matches!(changes.last().unwrap().kind, ChangeKind::Trashed));
        // The next cursor is the head; a re-poll yields nothing.
        let (again, _) = feed.changes_since(&next, 0);
        assert!(again.is_empty(), "polling at the returned cursor yields no new changes");
    }

    #[test]
    fn cursor_paginates_and_is_resumable() {
        let mut d = Drive::init("w", "a");
        for i in 0..10 {
            d.add_file("/", &format!("f{i}.txt"), fc(&format!("cid{i}"), i)).unwrap();
        }
        let state = d.state().clone();
        let feed = ChangeLog::new(&state, d.tree());
        let all = feed.changes_since(&Cursor::beginning(), 0).0;
        // Page 1: at most 5 changes.
        let (page1, c1) = feed.changes_since(&Cursor::beginning(), 5);
        assert_eq!(page1.len(), 5);
        // Page 2: resume from c1. No overlap: every page-2 ts is strictly greater than every page-1 ts.
        let (page2, _) = feed.changes_since(&c1, 5);
        assert!(!page2.is_empty());
        let max1 = page1.iter().map(|c| c.ts.clone()).max().unwrap();
        assert!(page2.iter().all(|c| c.ts > max1));
        // Drain the whole feed page by page and confirm it equals one unbounded read (no loss/dup).
        let mut drained = Vec::new();
        let mut cur = Cursor::beginning();
        loop {
            let (page, next) = feed.changes_since(&cur, 5);
            if page.is_empty() {
                break;
            }
            drained.extend(page);
            cur = next;
        }
        assert_eq!(drained.len(), all.len(), "paged drain == unbounded read");
        assert_eq!(drained, all, "paged drain is identical and in order");
    }

    #[test]
    fn head_skips_history() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "old.txt", fc("c", 1)).unwrap();
        let state = d.state().clone();
        let feed = ChangeLog::new(&state, d.tree());
        let head = feed.head();
        // From head, no past changes.
        assert!(feed.changes_since(&head, 0).0.is_empty());
    }

    #[test]
    fn move_is_classified_as_moved_not_created() {
        let mut d = Drive::init("w", "a");
        d.add_file("/", "a.txt", fc("c", 1)).unwrap();
        let cur = {
            let state = d.state().clone();
            let feed = ChangeLog::new(&state, d.tree());
            feed.head()
        };
        d.mv("/a.txt", "/", "b.txt").unwrap();
        let state = d.state().clone();
        let feed = ChangeLog::new(&state, d.tree());
        let (changes, _) = feed.changes_since(&cur, 0);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].kind, ChangeKind::Moved));
        assert_eq!(changes[0].path.as_deref(), Some("/b.txt"));
    }
}
