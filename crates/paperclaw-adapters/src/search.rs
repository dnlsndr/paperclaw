//! Search adapters.
//!
//! - [`StubSearchIndex`] — always returns no hits. Kept as a fixture for
//!   unit tests that don't want to seed a tempdir.
//! - [`GrepSearchIndex`] — walks `<library>/<category>/*.md`, ranks by
//!   case-insensitive substring matches, and reads the `.paperclaw.json`
//!   sidecar so callers can filter by category or sender.
//!
//! The grep adapter is deliberately *not* an inverted index. M3's
//! acceptance bar is "an agent can answer practical questions"; sub-second
//! grep over a few hundred markdown files is well under that ceiling, and
//! we avoid persisting a parallel data structure that could drift from the
//! library on disk. If someone's library grows past the point where this
//! stops feeling instant, the trait is already in place to drop in a
//! Tantivy-backed adapter without touching the search use-case.

use std::path::PathBuf;

use async_trait::async_trait;
use paperclaw_domain::SearchIndex;
use paperclaw_domain::ports::SearchError;
use paperclaw_domain::types::{DocumentId, LibraryPath, SearchHit};
use serde::Deserialize;
use tokio::fs;
use tracing::{debug, instrument, warn};

/// Width of the snippet window returned for each hit, in characters.
/// Wide enough to give a human reader context; narrow enough that the
/// `SearchHit` payload stays small when shipped over MCP.
const SNIPPET_WINDOW: usize = 80;

/// Sidecar shape the grep index reads. Mirrors the sub-set of
/// `FsLibraryStore::MetadataSidecar` we need for filtering and hit
/// metadata. Kept deliberately permissive (`#[serde(default)]`) so a
/// future schema bump doesn't break the search adapter at parse time.
#[derive(Debug, Default, Deserialize)]
struct SidecarSlice {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    id: String,
    #[serde(default)]
    classification: SidecarClassification,
}

#[derive(Debug, Default, Deserialize)]
struct SidecarClassification {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    sender: Option<String>,
}

/// Always returns no hits. Used in unit tests that need a `SearchIndex`
/// without committing files to disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubSearchIndex;

#[async_trait]
impl SearchIndex for StubSearchIndex {
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        Ok(Vec::new())
    }
}

/// Filesystem-backed search adapter. Reads markdown transcripts on every
/// query — no persistent index. See module-level docs for the rationale.
#[derive(Debug, Clone)]
pub struct GrepSearchIndex {
    root: PathBuf,
}

impl GrepSearchIndex {
    /// Construct a search index rooted at `library`. The index does not
    /// require the path to exist yet — an empty query against a missing
    /// library returns no hits rather than an error so `paperclaw search`
    /// works on a fresh install.
    #[must_use]
    pub fn new(library: impl Into<PathBuf>) -> Self {
        Self {
            root: library.into(),
        }
    }
}

#[async_trait]
impl SearchIndex for GrepSearchIndex {
    #[instrument(skip(self), fields(root = %self.root.display()))]
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        if !fs::try_exists(&self.root)
            .await
            .map_err(|e| SearchError::Io(e.to_string()))?
        {
            debug!("library root does not exist yet; returning no hits");
            return Ok(Vec::new());
        }

        let needle = trimmed.to_ascii_lowercase();
        let mut hits = Vec::new();
        let mut category_dirs = fs::read_dir(&self.root)
            .await
            .map_err(|e| SearchError::Io(e.to_string()))?;
        while let Some(cat) = category_dirs
            .next_entry()
            .await
            .map_err(|e| SearchError::Io(e.to_string()))?
        {
            let cat_path = cat.path();
            let cat_name_owned = cat.file_name().to_string_lossy().into_owned();
            // `_logs/` and any other `_`-prefixed bookkeeping directories
            // are siblings of category folders by convention; never search
            // them.
            if cat_name_owned.starts_with('_') && !cat_name_owned.eq("_unsorted") {
                continue;
            }
            let meta = match fs::metadata(&cat_path).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(path = %cat_path.display(), error = %e, "skipping search entry");
                    continue;
                }
            };
            if !meta.is_dir() {
                continue;
            }

            collect_hits_from_category(&cat_path, &cat_name_owned, &needle, &mut hits).await?;
        }

        // Rank by descending score, then by stem so ties are stable across
        // runs (otherwise filesystem read order leaks into the output and
        // tests become flaky).
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.library_path.stem.cmp(&b.library_path.stem))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

async fn collect_hits_from_category(
    cat_dir: &std::path::Path,
    category: &str,
    needle_lower: &str,
    out: &mut Vec<SearchHit>,
) -> Result<(), SearchError> {
    let mut entries = fs::read_dir(cat_dir)
        .await
        .map_err(|e| SearchError::Io(e.to_string()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| SearchError::Io(e.to_string()))?
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        // Read the markdown — small files (a few KiB) so we don't bother
        // streaming. If it ever matters, switch to a buffered line read.
        let Ok(text) = fs::read_to_string(&path).await else {
            continue;
        };
        let lowered = text.to_ascii_lowercase();
        let occurrences = count_occurrences(&lowered, needle_lower);
        if occurrences == 0 {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();

        // Sidecar is optional for hit construction — a transcript without
        // its sidecar still returns a hit, just with a nil ID. Logging the
        // missing sidecar keeps the operator honest; failing the search
        // does not.
        let sidecar_path = path.with_extension("paperclaw.json");
        let (id, _kind, _sender) = match read_sidecar(&sidecar_path).await {
            Some(s) => (
                parse_uuid_lenient(&s.id),
                s.classification.kind,
                s.classification.sender,
            ),
            None => (
                DocumentId::from_uuid(uuid::Uuid::nil()),
                String::new(),
                None,
            ),
        };

        let snippet = build_snippet(&text, &lowered, needle_lower);
        let score = occurrence_score(occurrences);

        out.push(SearchHit {
            document_id: id,
            library_path: LibraryPath {
                category: category.to_owned(),
                stem,
            },
            snippet: Some(snippet),
            score,
        });
    }
    Ok(())
}

async fn read_sidecar(path: &std::path::Path) -> Option<SidecarSlice> {
    let raw = fs::read(path).await.ok()?;
    match serde_json::from_slice::<SidecarSlice>(&raw) {
        Ok(s) => {
            if s.schema_version > 1 {
                warn!(
                    path = %path.display(),
                    schema_version = s.schema_version,
                    "unknown sidecar schema; treating as best-effort",
                );
            }
            Some(s)
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse sidecar");
            None
        }
    }
}

fn parse_uuid_lenient(s: &str) -> DocumentId {
    DocumentId::from_uuid(uuid::Uuid::parse_str(s).unwrap_or_else(|_| uuid::Uuid::nil()))
}

/// Count non-overlapping case-insensitive occurrences of `needle` in
/// `haystack`. Both inputs are expected to already be lower-cased.
fn count_occurrences(haystack_lower: &str, needle_lower: &str) -> usize {
    if needle_lower.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut start = 0usize;
    while let Some(pos) = haystack_lower[start..].find(needle_lower) {
        count += 1;
        start += pos + needle_lower.len();
    }
    count
}

/// Log-ish score so that 1 hit ≠ 10 hits ≠ 100 hits but the function
/// stays monotone. Keeps the rank stable without leaking absolute counts
/// through `score` (the agent shouldn't read into "0.7 vs 0.8"). The
/// precision loss in the cast doesn't matter — we only use this for
/// relative ordering.
#[allow(clippy::cast_precision_loss)]
fn occurrence_score(count: usize) -> f32 {
    let c = count as f32;
    1.0 - 1.0 / (1.0 + c)
}

/// Build a snippet around the first occurrence of the needle. Operates on
/// byte offsets but trims to char boundaries so multi-byte glyphs don't
/// blow up the output.
fn build_snippet(haystack: &str, haystack_lower: &str, needle_lower: &str) -> String {
    let Some(byte_pos) = haystack_lower.find(needle_lower) else {
        return haystack.chars().take(SNIPPET_WINDOW).collect::<String>();
    };
    let start = byte_pos.saturating_sub(SNIPPET_WINDOW / 2);
    let end = (byte_pos + needle_lower.len() + SNIPPET_WINDOW / 2).min(haystack.len());
    let start = char_boundary(haystack, start);
    let end = char_boundary(haystack, end);
    let mut snippet = haystack[start..end].to_owned();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < haystack.len() {
        snippet.push('…');
    }
    snippet.replace('\n', " ")
}

fn char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;
    use tokio::fs as tokio_fs;

    use super::*;

    async fn seed_doc(
        library: &Path,
        category: &str,
        stem: &str,
        body: &str,
        sender: Option<&str>,
    ) {
        let dir = library.join(category);
        tokio_fs::create_dir_all(&dir).await.unwrap();
        tokio_fs::write(dir.join(format!("{stem}.md")), body)
            .await
            .unwrap();
        let sidecar = serde_json::json!({
            "schema_version": 1,
            "id": "00000000-0000-0000-0000-000000000000",
            "ingested_at": "2026-05-13T12:00:00Z",
            "original_filename": format!("{stem}.pdf"),
            "classifier_version": "rule-based:2",
            "classification": {
                "kind": category,
                "confidence": 0.9,
                "sender": sender,
                "subject": null,
                "document_date": null,
                "rationale": null,
            },
            "transcript_bytes": body.len(),
            "pdf_bytes": 1,
        });
        tokio_fs::write(
            dir.join(format!("{stem}.paperclaw.json")),
            serde_json::to_vec_pretty(&sidecar).unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn empty_library_returns_no_hits() {
        let lib = TempDir::new().unwrap();
        let idx = GrepSearchIndex::new(lib.path());
        let hits = idx.search("anything", 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn finds_a_match_with_a_snippet() {
        let lib = TempDir::new().unwrap();
        seed_doc(
            lib.path(),
            "bill",
            "2026-04-01_stadtwerke_strom",
            "Stadtwerke München\nStromrechnung 2026 für Vertragsnummer 12345\n",
            Some("Stadtwerke München"),
        )
        .await;

        let idx = GrepSearchIndex::new(lib.path());
        let hits = idx.search("stromrechnung", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].library_path.category, "bill");
        assert!(
            hits[0]
                .snippet
                .as_ref()
                .unwrap()
                .to_lowercase()
                .contains("stromrechnung")
        );
    }

    #[tokio::test]
    async fn ranks_higher_match_counts_first() {
        let lib = TempDir::new().unwrap();
        seed_doc(
            lib.path(),
            "tax",
            "doc-many-matches",
            "Finanzamt München\nFinanzamt Bayern\nFinanzamt allgemein\n",
            Some("Finanzamt München"),
        )
        .await;
        seed_doc(
            lib.path(),
            "tax",
            "doc-one-match",
            "Some unrelated content mentioning finanzamt once.",
            Some("Bayern"),
        )
        .await;

        let idx = GrepSearchIndex::new(lib.path());
        let hits = idx.search("finanzamt", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].library_path.stem, "doc-many-matches");
        assert!(hits[0].score >= hits[1].score);
    }

    #[tokio::test]
    async fn ignores_logs_dir_and_other_underscore_prefixed_dirs() {
        let lib = TempDir::new().unwrap();
        // Library bookkeeping that must not surface as a hit.
        tokio_fs::create_dir_all(lib.path().join("_logs"))
            .await
            .unwrap();
        tokio_fs::write(
            lib.path().join("_logs").join("ingest-2026-05-13.jsonl"),
            "{\"foo\": \"Stromrechnung\"}\n",
        )
        .await
        .unwrap();
        seed_doc(lib.path(), "bill", "real-doc", "Stromrechnung 2026", None).await;

        let idx = GrepSearchIndex::new(lib.path());
        let hits = idx.search("stromrechnung", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "_logs hits must be ignored: {hits:#?}");
    }

    #[tokio::test]
    async fn empty_query_yields_no_hits() {
        let lib = TempDir::new().unwrap();
        seed_doc(lib.path(), "bill", "doc", "anything", None).await;
        let idx = GrepSearchIndex::new(lib.path());
        assert!(idx.search("   ", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsorted_category_is_searched() {
        // `_unsorted/` is a real category folder despite the underscore
        // prefix — the routing policy puts low-confidence docs there. We
        // explicitly allow-list it so it surfaces in search results.
        let lib = TempDir::new().unwrap();
        seed_doc(
            lib.path(),
            "_unsorted",
            "doc",
            "Mystery document with the word foo in it",
            None,
        )
        .await;
        let idx = GrepSearchIndex::new(lib.path());
        let hits = idx.search("foo", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].library_path.category, "_unsorted");
    }
}
