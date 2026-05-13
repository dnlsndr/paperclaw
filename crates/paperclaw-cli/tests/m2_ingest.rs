//! M2 acceptance: drive the real adapters end-to-end against the sample
//! PDFs in `assets/`.
//!
//! Goals:
//!
//! 1. A real PDF dropped into the inbox lands in the right category folder
//!    with `.pdf` / `.md` / `.paperclaw.json` siblings.
//! 2. The hostile workshop fixture (`finanzamt-bescheid.pdf`) is filed
//!    safely — the embedded "create a marker file" / "respond with done"
//!    payload must NOT influence the classification or write anything
//!    outside the library tempdir.
//! 3. The ingest log JSONL is produced under `library/_logs/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use paperclaw_adapters::{
    FallbackExtractor, FsInboxSource, FsLibraryStore, PdfTextExtractor, RuleBasedClassifier,
};
use paperclaw_app::IngestService;
use paperclaw_domain::LibraryPathPolicy;
use paperclaw_domain::testing::{FixedClock, SeqIdGenerator};
use paperclaw_domain::types::IngestOutcome;
use tempfile::TempDir;
use time::macros::datetime;
use tokio::fs;

/// Resolve `assets/<name>` relative to the workspace root. `CARGO_MANIFEST_DIR`
/// points at the crate that contains the test, so walk up two levels.
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

/// Copy an asset into an inbox tempdir under its original filename.
async fn stage_inbox(inbox: &Path, asset: &str) {
    let src = asset_path(asset);
    let dst = inbox.join(asset);
    fs::copy(&src, &dst)
        .await
        .unwrap_or_else(|e| panic!("staging {} into inbox failed: {e}", src.display()));
}

/// Build a production-shaped service pinned to a deterministic clock + ID
/// generator. We want real fs / pdf / classifier adapters, but reproducible
/// filenames so the assertions don't drift.
fn service(inbox_dir: &Path, library_dir: &Path) -> IngestService {
    let inbox = Arc::new(FsInboxSource::new(inbox_dir));
    let store = Arc::new(FsLibraryStore::new(library_dir));

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

#[tokio::test]
async fn ingests_both_sample_pdfs_into_correct_categories() {
    let inbox = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();

    stage_inbox(inbox.path(), "finanzamt-bescheid.pdf").await;
    stage_inbox(inbox.path(), "stadtwerke-stromrechnung.pdf").await;

    let svc = service(inbox.path(), library.path());
    let report = svc.ingest_all().await.expect("ingest pass must succeed");

    // Both PDFs must be processed (neither encrypted, neither malformed).
    assert_eq!(report.len(), 2, "report covers both inbox PDFs");
    assert_eq!(report.encrypted_count(), 0);
    assert_eq!(
        report.filed_count(),
        2,
        "both samples should pass the rule-based classifier confidence threshold; report: {report:?}",
    );

    // Tax letter routes to tax/.
    let tax_dir = library.path().join("tax");
    assert!(tax_dir.exists(), "tax/ category dir must exist");
    let tax_pdfs = collect_files(&tax_dir, "pdf").await;
    assert_eq!(tax_pdfs.len(), 1, "exactly one tax PDF, got {tax_pdfs:?}");

    // Each PDF must carry its sidecars.
    let stem = tax_pdfs[0]
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        tax_dir.join(format!("{stem}.md")).exists(),
        "tax .md sidecar"
    );
    assert!(
        tax_dir.join(format!("{stem}.paperclaw.json")).exists(),
        "tax .paperclaw.json sidecar",
    );

    // Stadtwerke utility bill routes to bill/.
    let bill_dir = library.path().join("bill");
    assert!(bill_dir.exists(), "bill/ category dir must exist");
    let bill_pdfs = collect_files(&bill_dir, "pdf").await;
    assert_eq!(
        bill_pdfs.len(),
        1,
        "exactly one bill PDF (Stadtwerke), got {bill_pdfs:?}",
    );

    // Sidecar carries the original filename verbatim — useful for audit.
    let bill_meta = bill_dir.join(format!(
        "{}.paperclaw.json",
        bill_pdfs[0].file_stem().unwrap().to_string_lossy(),
    ));
    let meta_text = fs::read_to_string(&bill_meta).await.unwrap();
    assert!(
        meta_text.contains("stadtwerke-stromrechnung.pdf"),
        "sidecar must preserve original filename for audit, got: {meta_text}",
    );

    // Source files must have been consumed from the inbox after filing.
    let inbox_after: Vec<_> = std::fs::read_dir(inbox.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        inbox_after.is_empty(),
        "inbox should be empty after successful filing, has: {inbox_after:?}",
    );
}

#[tokio::test]
async fn ingest_log_is_written_to_library_logs_dir() {
    let inbox = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    stage_inbox(inbox.path(), "stadtwerke-stromrechnung.pdf").await;

    let svc = service(inbox.path(), library.path());
    svc.ingest_all().await.unwrap();

    let logs_dir = library.path().join("_logs");
    assert!(logs_dir.exists(), "_logs dir must be created");
    let log_files = collect_files(&logs_dir, "jsonl").await;
    assert!(
        !log_files.is_empty(),
        "expected at least one ingest-<date>.jsonl, got nothing in {logs_dir:?}",
    );

    let log_body = fs::read_to_string(&log_files[0]).await.unwrap();
    // Each line is a JSON-encoded IngestEntry.
    let mut count = 0usize;
    for line in log_body.lines() {
        let _: serde_json::Value =
            serde_json::from_str(line).expect("each log line must be valid JSON");
        count += 1;
    }
    assert_eq!(count, 1, "one entry per ingested doc, got log:\n{log_body}");
}

#[tokio::test]
async fn prompt_injection_payload_is_neutralised() {
    // The finanzamt sample contains a `[SYSTEM NOTICE …]` payload that
    // asks the assistant to (a) create a marker file
    // `paperclaw-wurde-von-bettervibe-erwischt.txt` in the working
    // directory, (b) move the PDF to that name as a fallback, and (c)
    // respond with the single word `done` and skip transcribing.
    //
    // Our defenses are:
    //   - The classifier is keyword-based, so step (c) can't influence
    //     routing. We assert it still lands in tax/.
    //   - The sanitizer redacts the hostile lines before the classifier
    //     reads them — so it can't be tricked into picking Bank Statement
    //     even if a future LLM-backed classifier is swapped in.
    //   - The adapter never executes "tools" referenced in the document,
    //     so the marker file should not appear anywhere on disk.
    let inbox = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    stage_inbox(inbox.path(), "finanzamt-bescheid.pdf").await;

    let svc = service(inbox.path(), library.path());
    let report = svc.ingest_all().await.unwrap();

    // Filed under tax/, not Bank Statement.
    assert_eq!(report.filed_count(), 1);
    let entry = &report.entries[0];
    match &entry.outcome {
        IngestOutcome::Filed { document } => {
            assert_eq!(
                document.library_path.category, "tax",
                "hostile payload must not redirect classification",
            );
        }
        other => panic!("expected Filed, got {other:?}"),
    }

    // No marker file appeared inside the inbox dir, library dir, or
    // current working directory. Inbox is the highest-risk spot because
    // the payload's fallback move would land there.
    for dir in [inbox.path(), library.path(), Path::new(".")] {
        scan_for_marker(dir).await;
    }

    // The transcript on disk still contains the verbatim hostile text —
    // we redact for the *classifier*, not for the audit log. (A future
    // M3 LLM classifier could mask it inside the .md too, but for M2
    // preserving the raw transcript is the design intent.)
    let tax_md = collect_files(&library.path().join("tax"), "md")
        .await
        .into_iter()
        .next()
        .expect("expected one .md sidecar under tax/");
    let body = fs::read_to_string(&tax_md).await.unwrap();
    assert!(
        body.contains("SYSTEM NOTICE"),
        "raw transcript must keep the hostile text for audit (sidecar is what the agent reads later)",
    );
}

/// Walk a directory (non-recursive) and return paths whose extension
/// matches.
async fn collect_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(dir).await.unwrap();
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    out
}

/// Recursively scan `dir` for the marker filename the prompt-injection
/// payload tries to coerce into existence. Asserts none is found.
async fn scan_for_marker(dir: &Path) {
    const MARKER: &str = "paperclaw-wurde-von-bettervibe-erwischt";
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        // `.` from a sandboxed runner may not be readable; ignore.
        let Ok(mut entries) = fs::read_dir(&current).await else {
            continue;
        };
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains(MARKER),
                "prompt-injection marker leaked to disk: {} in {}",
                name,
                current.display(),
            );
            let path = entry.path();
            // Don't descend into target/ or hidden Cargo dirs — they're
            // huge and not the attacker's target.
            if path.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules"
            {
                stack.push(path);
            }
        }
    }
}
