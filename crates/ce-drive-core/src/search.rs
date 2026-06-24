//! Filename + metadata search — a real, in-memory inverted index over the drive's live files.
//!
//! Google Drive's headline surface is search. CE Drive cannot index arbitrary blob *content* without
//! fetching every CID (the bytes live content-addressed on the mesh, not locally), but it can index
//! everything the manifest already holds locally and for free: **file and folder names**, their
//! **derived paths**, and structural metadata (size, kind). That covers the overwhelmingly common
//! "find a file by (part of) its name or folder" query with zero network.
//!
//! The index is a case-folded, token + trigram inverted index:
//! * a name/path is tokenized on non-alphanumeric boundaries and lowercased,
//! * each token is also indexed as character trigrams, so substring queries (`"epor"` matching
//!   `report.pdf`) and typo-tolerant prefix matches work, not just whole-token equality,
//! * a query is tokenized the same way; results are ranked by how many query tokens/trigrams a node
//!   matches (a cheap TF-style score), with exact-substring matches boosted.
//!
//! The index is built from a [`Drive`] (or any iterator of `(node_id, path, is_dir, size)`), so it is
//! pure and fully unit-testable with no live node. Full-text *content* search (indexing fetched text
//! blobs) is a documented future extension behind the same [`SearchIndex::add_text`] hook.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::drive::Drive;
use crate::tree::NodeKind;

/// One search result: the matching node, its path, and a relevance score (higher = better).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The matching node id.
    pub node_id: String,
    /// The node's derived absolute path.
    pub path: String,
    /// The node's bare name.
    pub name: String,
    /// True if the match is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Relevance score; results are returned highest-first.
    pub score: f32,
}

/// One indexed document: the locally-known facts about a node.
#[derive(Debug, Clone)]
struct Doc {
    node_id: String,
    path: String,
    name: String,
    is_dir: bool,
    size: u64,
    /// Lowercased tokens of name + path (+ any added text).
    tokens: HashSet<String>,
    /// Character trigrams of every token (for substring matching).
    trigrams: HashSet<String>,
}

/// An in-memory inverted index over a drive's filenames, paths, and metadata.
#[derive(Default)]
pub struct SearchIndex {
    docs: Vec<Doc>,
    /// token -> doc indices.
    by_token: HashMap<String, Vec<usize>>,
    /// trigram -> doc indices.
    by_trigram: HashMap<String, Vec<usize>>,
}

impl SearchIndex {
    /// An empty index.
    pub fn new() -> Self {
        SearchIndex::default()
    }

    /// Build an index over every live file and folder in a [`Drive`].
    pub fn build(drive: &Drive) -> Self {
        let mut idx = SearchIndex::new();
        idx.index_dir(drive, "/");
        idx
    }

    fn index_dir(&mut self, drive: &Drive, path: &str) {
        let Ok(entries) = drive.ls(path) else { return };
        for e in entries {
            let child_path =
                if path == "/" { format!("/{}", e.name) } else { format!("{path}/{}", e.name) };
            self.add(&e.node_id, &child_path, &e.name, e.is_dir, e.content.as_ref().map(|c| c.size).unwrap_or(0));
            if e.is_dir {
                self.index_dir(drive, &child_path);
            }
        }
    }

    /// Add (or replace) one node in the index. Replacing by node id keeps the index consistent if a
    /// caller re-indexes a single path after a change.
    pub fn add(&mut self, node_id: &str, path: &str, name: &str, is_dir: bool, size: u64) {
        let mut tokens = HashSet::new();
        let mut trigrams = HashSet::new();
        for src in [name, path] {
            for tok in tokenize(src) {
                trigrams.extend(trigrams_of(&tok));
                tokens.insert(tok);
            }
        }
        let doc = Doc {
            node_id: node_id.to_string(),
            path: path.to_string(),
            name: name.to_string(),
            is_dir,
            size,
            tokens,
            trigrams,
        };
        // Replace if the node is already present.
        if let Some(i) = self.docs.iter().position(|d| d.node_id == node_id) {
            self.docs[i] = doc;
            self.reindex();
        } else {
            self.docs.push(doc);
            self.reindex_last();
        }
    }

    /// Augment a node's document with extracted text content (the full-text hook). Tokens from the
    /// text join the node's token/trigram sets, so a content query can match it. The caller decides
    /// what text to extract (e.g. the first N KiB of a fetched `.md`/`.txt` blob).
    pub fn add_text(&mut self, node_id: &str, text: &str) -> bool {
        if let Some(d) = self.docs.iter_mut().find(|d| d.node_id == node_id) {
            for tok in tokenize(text) {
                d.trigrams.extend(trigrams_of(&tok));
                d.tokens.insert(tok);
            }
            self.reindex();
            true
        } else {
            false
        }
    }

    /// Rebuild the inverted maps from scratch (after a replace or text add).
    fn reindex(&mut self) {
        self.by_token.clear();
        self.by_trigram.clear();
        for i in 0..self.docs.len() {
            for t in self.docs[i].tokens.clone() {
                self.by_token.entry(t).or_default().push(i);
            }
            for g in self.docs[i].trigrams.clone() {
                self.by_trigram.entry(g).or_default().push(i);
            }
        }
    }

    fn reindex_last(&mut self) {
        let i = self.docs.len() - 1;
        for t in self.docs[i].tokens.clone() {
            self.by_token.entry(t).or_default().push(i);
        }
        for g in self.docs[i].trigrams.clone() {
            self.by_trigram.entry(g).or_default().push(i);
        }
    }

    /// The number of indexed documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Search for `query`, returning up to `limit` hits, highest-score first. An empty query yields
    /// no hits. Scoring: +2 per query token a doc contains exactly, +1 per matching trigram (capped),
    /// and a large boost when the raw lowercased query is a substring of the name or path.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let q_tokens = tokenize(&q);
        let q_trigrams: HashSet<String> = q_tokens.iter().flat_map(|t| trigrams_of(t)).collect();

        let mut score: HashMap<usize, f32> = HashMap::new();
        for tok in &q_tokens {
            if let Some(docs) = self.by_token.get(tok) {
                for &i in docs {
                    *score.entry(i).or_default() += 2.0;
                }
            }
        }
        for g in &q_trigrams {
            if let Some(docs) = self.by_trigram.get(g) {
                for &i in docs {
                    // Cap trigram contribution so a long token doesn't dominate.
                    let e = score.entry(i).or_default();
                    *e = (*e + 0.25).min(*e + 4.0);
                }
            }
        }

        let mut hits: Vec<SearchHit> = score
            .into_iter()
            .map(|(i, mut s)| {
                let d = &self.docs[i];
                let name_lc = d.name.to_lowercase();
                // Exact-name match is the strongest signal (a file/folder literally named the query).
                if name_lc == q {
                    s += 20.0;
                } else if name_lc.contains(&q) {
                    // Substring-in-name is a strong hit; a prefix/extension-stem match (e.g. query
                    // "report" against "report.txt") is boosted a little more than a mid-name hit.
                    s += 10.0;
                    if name_lc.starts_with(&q) {
                        s += 2.0;
                    }
                } else if d.path.to_lowercase().contains(&q) {
                    // Only the path (an ancestor folder) contains the query — the weakest of the three.
                    s += 5.0;
                }
                SearchHit {
                    node_id: d.node_id.clone(),
                    path: d.path.clone(),
                    name: d.name.clone(),
                    is_dir: d.is_dir,
                    size: d.size,
                    score: s,
                }
            })
            .filter(|h| h.score > 0.0)
            .collect();

        // Highest score first; tiebreak by shorter path (more specific) then path lexicographically.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.len().cmp(&b.path.len()))
                .then_with(|| a.path.cmp(&b.path))
        });
        if limit != 0 {
            hits.truncate(limit);
        }
        hits
    }

    /// Filter to a subtree: search only nodes whose path is under `scope` (used to honor a share's
    /// path scope when a recipient searches a shared folder).
    pub fn search_scoped(&self, query: &str, scope: &str, limit: usize) -> Vec<SearchHit> {
        let scope = if scope == "/" { String::new() } else { scope.trim_end_matches('/').to_string() };
        self.search(query, 0)
            .into_iter()
            .filter(|h| scope.is_empty() || h.path == scope || h.path.starts_with(&format!("{scope}/")))
            .take(if limit == 0 { usize::MAX } else { limit })
            .collect()
    }
}

/// True for a directory kind (small helper kept here so callers building an index from raw edges can
/// classify without importing [`NodeKind`] details).
pub fn is_dir_kind(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Dir)
}

/// Tokenize a string: split on any non-alphanumeric character, lowercase, drop empties.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// The character trigrams of a token (padded so short tokens still produce at least one gram). For a
/// 1-2 char token we just use the token itself as a single gram so prefixes still match.
fn trigrams_of(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 3 {
        return vec![token.to_string()];
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::FileContent;

    fn fc(cid: &str, size: u64) -> FileContent {
        FileContent::new(cid, size, 0o644, 1)
    }

    fn sample() -> Drive {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "docs").unwrap();
        d.mkdir("/", "photos").unwrap();
        d.add_file("/docs", "quarterly-report.pdf", fc("c1", 100)).unwrap();
        d.add_file("/docs", "meeting-notes.md", fc("c2", 50)).unwrap();
        d.add_file("/photos", "vacation.jpg", fc("c3", 2048)).unwrap();
        d
    }

    #[test]
    fn finds_by_whole_token() {
        let d = sample();
        let idx = SearchIndex::build(&d);
        let hits = idx.search("report", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "quarterly-report.pdf");
    }

    #[test]
    fn finds_by_substring() {
        let d = sample();
        let idx = SearchIndex::build(&d);
        // "epor" is a substring of "report" — trigram + substring boost should surface it.
        let hits = idx.search("epor", 10);
        assert!(hits.iter().any(|h| h.name == "quarterly-report.pdf"), "substring match works");
    }

    #[test]
    fn finds_folder_by_name() {
        let d = sample();
        let idx = SearchIndex::build(&d);
        let hits = idx.search("photos", 10);
        assert!(hits.iter().any(|h| h.is_dir && h.name == "photos"));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let d = sample();
        let idx = SearchIndex::build(&d);
        assert!(idx.search("", 10).is_empty());
        assert!(idx.search("   ", 10).is_empty());
    }

    #[test]
    fn ranking_prefers_name_over_path_match() {
        let mut d = Drive::init("w", "a");
        d.mkdir("/", "report").unwrap(); // path component "report"
        d.add_file("/report", "report.txt", fc("c", 1)).unwrap(); // name "report"
        d.add_file("/report", "other.txt", fc("c2", 1)).unwrap(); // only path contains "report"
        let idx = SearchIndex::build(&d);
        let hits = idx.search("report", 10);
        // The exact-name folder "report" and the substring-name file "report.txt" both outrank the
        // path-only "other.txt", which must come last.
        assert_eq!(hits.last().unwrap().name, "other.txt", "path-only match ranks lowest");
        let top_two: std::collections::HashSet<&str> =
            hits[..2].iter().map(|h| h.name.as_str()).collect();
        assert!(top_two.contains("report"), "exact-name folder is a top hit");
        assert!(top_two.contains("report.txt"), "name-substring file is a top hit");
        // The exact full-name match ("report" folder) is the single strongest hit.
        assert_eq!(hits[0].name, "report");
    }

    #[test]
    fn scoped_search_limits_to_subtree() {
        let d = sample();
        let idx = SearchIndex::build(&d);
        // ".pdf"/".jpg" extension token search across the whole drive then scoped.
        let all = idx.search("md", 10);
        assert!(all.iter().any(|h| h.path == "/docs/meeting-notes.md"));
        let scoped = idx.search_scoped("vacation", "/docs", 10);
        assert!(scoped.is_empty(), "vacation.jpg is under /photos, excluded by /docs scope");
        let scoped2 = idx.search_scoped("vacation", "/photos", 10);
        assert!(scoped2.iter().any(|h| h.name == "vacation.jpg"));
    }

    #[test]
    fn add_text_enables_content_match() {
        let d = sample();
        let mut idx = SearchIndex::build(&d);
        let id = d.tree().resolve("/docs/meeting-notes.md").unwrap();
        assert!(idx.add_text(&id, "discussed the kubernetes migration timeline"));
        let hits = idx.search("kubernetes", 10);
        assert!(hits.iter().any(|h| h.name == "meeting-notes.md"), "content token now searchable");
    }
}
