//! M3 acceptance — search.
//!
//! Real end-to-end: ingest the two sample PDFs from `assets/` through the
//! production adapters, then run live queries through `SearchService`
//! backed by `GrepSearchIndex`. Asserts that an agent could ask "find the
//! Stadtwerke bill" or "which docs mention Einkommensteuer" and get a
//! useful answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use paperclaw_adapters::{
    FallbackExtractor, FsInboxSource, FsLibraryStore, GrepSearchIndex, PdfTextExtractor,
    RuleBasedClassifier,
};
use paperclaw_app::{IngestService, SearchService};
use paperclaw_domain::LibraryPathPolicy;
use paperclaw_domain::testing::{FixedClock, SeqIdGenerator};
use tempfile::TempDir;
use time::macros::datetime;
use tokio::fs;

fn asset_path(name: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("assets")
        .join(name)
}

async fn stage_inbox(inbox: &Path, asset: &str) {
    let src = asset_path(asset);
    fs::copy(&src, inbox.join(asset))
        .await
        .unwrap_or_else(|e| panic!("staging {} failed: {e}", src.display()));
}

fn build_ingest(inbox: &Path, library: &Path) -> IngestService {
    let inbox = Arc::new(FsInboxSource::new(inbox));
    let store = Arc::new(FsLibraryStore::new(library));

    let primary: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
    let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
    let extractor = Arc::new(FallbackExtractor::new(primary, fallback));

    let classifier = Arc::new(RuleBasedClassifier::new());
    let clock = Arc::new(FixedClock::new(datetime!(2026-05-13 12:00 UTC)));
    let ids = Arc::new(SeqIdGenerator::new());

    IngestService::new(
        inbox,
        extractor,
        classifier,
        store,
        LibraryPathPolicy::new(),
        clock,
        ids,
    )
}

async fn seed_library() -> (TempDir, TempDir) {
    let inbox = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    stage_inbox(inbox.path(), "finanzamt-bescheid.pdf").await;
    stage_inbox(inbox.path(), "stadtwerke-stromrechnung.pdf").await;
    let svc = build_ingest(inbox.path(), library.path());
    svc.ingest_all().await.expect("ingest must succeed");
    (inbox, library)
}

#[tokio::test]
async fn finds_the_stadtwerke_bill_by_substring() {
    // "Find the invoice for that gadget from three months ago" → in our
    // workshop corpus the closest analogue is "find the Stadtwerke bill"
    // — same shape, agent passes a substring and gets a structured hit.
    let (_inbox, library) = seed_library().await;
    let svc = SearchService::new(Arc::new(GrepSearchIndex::new(library.path())));

    let hits = svc.query("stadtwerke", 10).await.unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(hits[0].library_path.category, "bill");
    let snippet = hits[0]
        .snippet
        .as_deref()
        .expect("hit should carry a snippet");
    assert!(
        snippet.to_lowercase().contains("stadtwerke"),
        "snippet should contain the match, got: {snippet}",
    );
}

#[tokio::test]
async fn finds_the_finanzamt_letter_by_german_keyword() {
    // Locale-realistic query: the user's mental model is "Einkommensteuer
    // Bescheid", not "tax letter".
    let (_inbox, library) = seed_library().await;
    let svc = SearchService::new(Arc::new(GrepSearchIndex::new(library.path())));

    let hits = svc.query("Einkommensteuer", 10).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].library_path.category, "tax");
}

#[tokio::test]
async fn ranks_match_density_first() {
    // Both PDFs mention "2024" in passing, but the tax letter references
    // the year multiple times (Bescheid für 2024, Einkommensteuer 2024, …).
    // Ranking by occurrence count should put it first.
    let (_inbox, library) = seed_library().await;
    let svc = SearchService::new(Arc::new(GrepSearchIndex::new(library.path())));

    let hits = svc.query("2024", 10).await.unwrap();
    assert!(!hits.is_empty());
    // First hit (highest score) should be the tax letter.
    assert_eq!(hits[0].library_path.category, "tax");
}

#[tokio::test]
async fn empty_library_returns_no_hits_without_error() {
    let library = TempDir::new().unwrap();
    let svc = SearchService::new(Arc::new(GrepSearchIndex::new(library.path())));
    let hits = svc.query("anything", 10).await.unwrap();
    assert!(hits.is_empty());
}
