//! The ingest use-case: inbox → classify → store → report.

use std::path::Path;
use std::sync::Arc;

use paperclaw_domain::ports::{ClassifierError, InboxError, LibraryWrite, StoreError};
use paperclaw_domain::types::{IngestEntry, IngestOutcome, IngestReport, SourceMedia};
use paperclaw_domain::{
    Classification, Classifier, Clock, Document, DocumentId, ExtractionError, IdGenerator,
    InboxSource, LibraryPathPolicy, LibraryStore, PendingDocument, SourcePath, TextExtractor,
    Transcript,
};
use thiserror::Error;
use tracing::{info, instrument, warn};

/// Errors surfaced by [`IngestService`]. These are categories the CLI
/// converts into exit codes; per-document failures end up in
/// [`paperclaw_domain::IngestReport`] instead.
#[derive(Debug, Error)]
pub enum AppError {
    /// Inbox couldn't be read.
    #[error("inbox unavailable: {0}")]
    Inbox(#[from] InboxError),

    /// Library store rejected a write (whole-batch level, not per-doc).
    #[error("library store unavailable: {0}")]
    Store(#[from] StoreError),

    /// Use-case isn't implemented yet — kept so M2/M3 work can stub
    /// services without `todo!()`.
    #[error("not implemented")]
    NotImplemented,
}

/// Orchestrates one full ingest pass over the inbox.
///
/// `IngestService` only knows trait objects — concrete adapters are
/// injected at the CLI's composition root. Per-document failures
/// (encryption, classifier transport, low confidence) are recorded in
/// the [`IngestReport`] and never abort the batch.
///
/// Cloning is cheap (every field is an `Arc` or trivially-cloneable
/// value); the per-document concurrency fan-out clones the service into
/// each spawned task.
#[derive(Clone)]
pub struct IngestService {
    inbox: Arc<dyn InboxSource>,
    extractor: Arc<dyn TextExtractor>,
    classifier: Arc<dyn Classifier>,
    store: Arc<dyn LibraryStore>,
    policy: LibraryPathPolicy,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl std::fmt::Debug for IngestService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestService").finish_non_exhaustive()
    }
}

impl IngestService {
    /// Wire the service. All ports arrive as `Arc<dyn …>` so the CLI can
    /// swap concretes (fs / in-memory / fake) without touching this
    /// constructor.
    #[must_use]
    pub fn new(
        inbox: Arc<dyn InboxSource>,
        extractor: Arc<dyn TextExtractor>,
        classifier: Arc<dyn Classifier>,
        store: Arc<dyn LibraryStore>,
        policy: LibraryPathPolicy,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            inbox,
            extractor,
            classifier,
            store,
            policy,
            clock,
            ids,
        }
    }

    /// Run one ingest pass over everything currently in the inbox.
    ///
    /// Spawns one tokio task per pending document. Extraction,
    /// classification and store-write run in parallel across documents;
    /// the [`paperclaw_domain::LibraryStore`] implementation serializes
    /// its own resolve-collision-and-commit critical section. Per-doc
    /// panics propagate via [`std::panic::resume_unwind`] and abort the
    /// batch — we deliberately don't try to convert a panic to
    /// [`IngestOutcome::Failed`] (panics are bugs, not document state).
    ///
    /// Returns an [`IngestReport`] enumerating per-document outcomes in
    /// input order. Whole-batch infrastructure failures (inbox
    /// unreadable, store completely down) bubble up as [`AppError`].
    ///
    /// # Errors
    ///
    /// Fails with [`AppError::Inbox`] when the inbox can't be listed.
    /// Per-document failures (encrypted PDFs, classifier transport
    /// errors, low confidence) are captured in the returned report
    /// instead of being raised.
    #[instrument(skip_all)]
    pub async fn ingest_all(&self) -> Result<IngestReport, AppError> {
        let pending = self.inbox.pending().await?;
        info!(count = pending.len(), "ingest pass starting");

        let mut handles = Vec::with_capacity(pending.len());
        for doc in pending {
            let worker = self.clone();
            handles.push(tokio::spawn(async move { worker.ingest_one(doc).await }));
        }

        let mut report = IngestReport::default();
        for handle in handles {
            let entry = match handle.await {
                Ok(entry) => entry,
                Err(join_err) if join_err.is_panic() => {
                    // Decision (DESIGN §9 panic policy): a panic inside
                    // an ingest task aborts the entire batch. Re-raise
                    // the original panic so the binary exits with the
                    // same backtrace it would have had under a serial
                    // loop.
                    std::panic::resume_unwind(join_err.into_panic());
                }
                Err(join_err) => {
                    // We never cancel tasks ourselves; a cancellation
                    // here would indicate runtime shutdown mid-batch.
                    return Err(AppError::Store(StoreError::Io(format!(
                        "ingest task ended unexpectedly: {join_err}",
                    ))));
                }
            };

            // Best-effort log append: don't take the batch down if the
            // log file is wedged. The report still carries the truth.
            if let Err(e) = self.store.append_log(&entry).await {
                warn!(error = %e, "failed to append ingest log entry");
            }
            report.entries.push(entry);
        }

        info!(
            total = report.len(),
            filed = report.filed_count(),
            encrypted = report.encrypted_count(),
            "ingest pass complete",
        );
        Ok(report)
    }

    /// Ingest a single document handed in directly (e.g. via the MCP
    /// `ingest_document` tool when an upstream LLM passes bytes through
    /// the tool call). Bypasses the [`InboxSource`] — the caller is
    /// responsible for sourcing the bytes and the use-case skips the
    /// `consume` step since no inbox copy was created.
    ///
    /// Returns the single [`IngestEntry`] describing what happened. Also
    /// appends a log line via the configured [`LibraryStore`] so MCP-
    /// driven uploads share the same audit trail as inbox-driven ones.
    pub async fn ingest_pending(&self, pending: PendingDocument) -> IngestEntry {
        let source = pending.source.clone();
        let outcome = self.process(&pending).await;
        let entry = IngestEntry { source, outcome };
        if let Err(e) = self.store.append_log(&entry).await {
            warn!(error = %e, "failed to append ingest log entry for MCP upload");
        }
        entry
    }

    async fn ingest_one(self, pending: PendingDocument) -> IngestEntry {
        let source = pending.source.clone();
        let outcome = self.process(&pending).await;

        // Once the library owns the bytes, the inbox copy is just a
        // duplicate — the next run would re-process it forever. Encrypted
        // and Failed outcomes leave the source in place so the user can
        // retry after decrypting / fixing the file.
        if outcome_should_consume_source(&outcome)
            && let Err(e) = self.inbox.consume(&source).await
        {
            warn!(
                source = %source.as_path().display(),
                error = %e,
                "failed to remove source from inbox after filing; \
                 re-run may duplicate the document",
            );
        }

        IngestEntry { source, outcome }
    }

    async fn process(&self, pending: &PendingDocument) -> IngestOutcome {
        let transcript = match self
            .extractor
            .extract(SourceMedia::from_pending(pending))
            .await
        {
            Ok(t) => t,
            Err(ExtractionError::Encrypted { hint }) => {
                warn!(
                    source = %pending.source.as_path().display(),
                    hint = %hint,
                    "skipping encrypted PDF; decrypt and re-drop it in inbox/",
                );
                return IngestOutcome::SkippedEncrypted { hint };
            }
            Err(e) => {
                return IngestOutcome::Failed {
                    reason: format!("extraction failed: {e}"),
                };
            }
        };

        let classification = match self.classifier.classify(&transcript).await {
            Ok(c) => c,
            Err(ClassifierError::NotImplemented) => {
                return IngestOutcome::Failed {
                    reason: "classifier not implemented".to_owned(),
                };
            }
            Err(e) => {
                return IngestOutcome::Failed {
                    reason: format!("classifier failed: {e}"),
                };
            }
        };

        // Low-confidence docs are still filed (the policy routes them to
        // _unsorted/) — the outcome variant just records the audit trail.
        let document = match self.file(pending, &transcript, &classification).await {
            Ok(d) => d,
            Err(e) => {
                return IngestOutcome::Failed {
                    reason: format!("store failed: {e}"),
                };
            }
        };

        if classification.confidence.is_low() {
            IngestOutcome::SkippedLowConfidence {
                classification: Box::new(classification),
            }
        } else {
            IngestOutcome::Filed {
                document: Box::new(document),
            }
        }
    }

    async fn file(
        &self,
        pending: &PendingDocument,
        transcript: &Transcript,
        classification: &Classification,
    ) -> Result<Document, StoreError> {
        let ingested_at = self.clock.now();
        let id = DocumentId::from_uuid(self.ids.new_id());
        let source_path = pending.source.as_path();
        let original_stem = original_stem(source_path);
        let original_filename = original_filename(&pending.source);
        let target = self
            .policy
            .path_for(classification, &original_stem, ingested_at);

        let library_path = self
            .store
            .store(&LibraryWrite {
                target: &target,
                pdf_bytes: &pending.bytes,
                transcript,
                classification,
                original_filename: &original_filename,
                classifier_version: self.classifier.version(),
                ingested_at,
                id,
            })
            .await?;

        Ok(Document {
            id,
            source: pending.source.clone(),
            library_path,
            classification: classification.clone(),
            ingested_at,
        })
    }
}

fn original_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("document")
        .to_owned()
}

fn original_filename(source: &SourcePath) -> String {
    source
        .as_path()
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("document.pdf")
        .to_owned()
}

fn outcome_should_consume_source(outcome: &IngestOutcome) -> bool {
    matches!(
        outcome,
        IngestOutcome::Filed { .. } | IngestOutcome::SkippedLowConfidence { .. },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use paperclaw_domain::testing::{
        EncryptedExtractor, FixedClock, InMemoryInbox, InMemoryLibraryStore, SeqIdGenerator,
        StubClassifier, StubExtractor, assert_send, assert_sync,
    };
    use paperclaw_domain::types::{IngestOutcome, MediaType, PendingDocument, SourcePath};
    use time::macros::datetime;

    use super::*;

    // Compile-time guarantees: the service stays `Send + Sync` so it can
    // ride inside `tokio::spawn` and Axum-style handlers down the road.
    const _: fn() = || {
        assert_send::<IngestService>();
        assert_sync::<IngestService>();
    };

    fn pending(name: &str) -> PendingDocument {
        PendingDocument {
            source: SourcePath::new(std::path::PathBuf::from(format!("inbox/{name}.pdf"))),
            bytes: b"%PDF-1.4 fake".to_vec(),
            media_type: MediaType::Pdf,
        }
    }

    fn service(
        inbox: Arc<InMemoryInbox>,
        extractor: Arc<dyn TextExtractor>,
        classifier: Arc<dyn Classifier>,
        store: Arc<InMemoryLibraryStore>,
    ) -> IngestService {
        IngestService::new(
            inbox,
            extractor,
            classifier,
            store,
            LibraryPathPolicy::new(),
            Arc::new(FixedClock::new(datetime!(2026-05-13 12:00 UTC))),
            Arc::new(SeqIdGenerator::new()),
        )
    }

    #[tokio::test]
    async fn files_a_confident_document() {
        let inbox = Arc::new(InMemoryInbox::with(vec![pending("acme-invoice")]));
        let store = Arc::new(InMemoryLibraryStore::new());

        let svc = service(
            inbox.clone(),
            Arc::new(StubExtractor::returning("Invoice from Acme")),
            Arc::new(StubClassifier::confident_invoice()),
            store.clone(),
        );

        let report = svc.ingest_all().await.unwrap();

        assert_eq!(report.filed_count(), 1);
        assert_eq!(report.encrypted_count(), 0);
        assert_eq!(store.writes().len(), 1);
        let write = &store.writes()[0];
        assert_eq!(write.path.category, "invoice");
        assert!(write.path.stem.starts_with("2026-05-13_"));
        // Sidecar fields land on the store.
        assert_eq!(write.original_filename, "acme-invoice.pdf");
        assert_eq!(write.classifier_version, "stub");

        // Successful filings must remove the source from the inbox so
        // re-runs don't duplicate the document.
        let consumed = inbox.consumed();
        assert_eq!(consumed.len(), 1);
        assert_eq!(
            consumed[0].as_path().file_name().unwrap(),
            "acme-invoice.pdf",
        );
    }

    #[tokio::test]
    async fn skips_encrypted_pdfs_without_aborting_the_batch() {
        let inbox = Arc::new(InMemoryInbox::with(vec![
            pending("encrypted-bill"),
            pending("normal-bill"),
        ]));
        let store = Arc::new(InMemoryLibraryStore::new());

        // First doc encrypted, second doc fine. With a real extractor
        // chain we'd swap mid-stream — for the use-case test we keep it
        // simple: every doc goes through one extractor, so both fail
        // with Encrypted. We still assert the batch *completed* and that
        // both outcomes are SkippedEncrypted (proving no early abort).
        let svc = service(
            inbox.clone(),
            Arc::new(EncryptedExtractor),
            Arc::new(StubClassifier::confident_invoice()),
            store.clone(),
        );

        let report = svc.ingest_all().await.unwrap();

        assert_eq!(report.len(), 2, "batch must not abort on encrypted PDF");
        assert_eq!(report.encrypted_count(), 2);
        assert_eq!(report.filed_count(), 0);
        for entry in &report.entries {
            assert!(matches!(
                entry.outcome,
                IngestOutcome::SkippedEncrypted { .. }
            ));
        }
        // Logged each entry, including the skips.
        assert_eq!(store.log_entries().len(), 2);
        // Encrypted PDFs must stay in the inbox so the user can decrypt
        // and re-drop them.
        assert!(
            inbox.consumed().is_empty(),
            "encrypted PDFs must not be consumed; user needs to retry",
        );
    }

    #[tokio::test]
    async fn low_confidence_routes_to_unsorted() {
        let inbox = Arc::new(InMemoryInbox::with(vec![pending("unclear-doc")]));
        let store = Arc::new(InMemoryLibraryStore::new());

        let svc = service(
            inbox.clone(),
            Arc::new(StubExtractor::returning("???")),
            Arc::new(StubClassifier::low_confidence()),
            store.clone(),
        );

        let report = svc.ingest_all().await.unwrap();
        assert_eq!(report.len(), 1);
        assert!(matches!(
            report.entries[0].outcome,
            IngestOutcome::SkippedLowConfidence { .. },
        ));
        // The PDF was still written, just under _unsorted/.
        assert_eq!(store.writes()[0].path.category, "_unsorted");
        // Low-confidence files DID get written to library/_unsorted, so the
        // inbox copy should be consumed — otherwise we'd refile it forever.
        assert_eq!(inbox.consumed().len(), 1);
    }

    #[tokio::test]
    async fn fans_out_concurrent_tasks_and_preserves_input_order() {
        let names = ["alpha", "bravo", "charlie", "delta"];
        let inbox = Arc::new(InMemoryInbox::with(
            names.iter().map(|n| pending(n)).collect(),
        ));
        let store = Arc::new(InMemoryLibraryStore::new());

        let svc = service(
            inbox.clone(),
            Arc::new(StubExtractor::returning("Invoice")),
            Arc::new(StubClassifier::confident_invoice()),
            store.clone(),
        );

        let report = svc.ingest_all().await.unwrap();
        assert_eq!(report.filed_count(), names.len());

        // Report order matches input order even though tasks ran
        // concurrently and may have completed out of order.
        for (entry, expected) in report.entries.iter().zip(names.iter()) {
            let got = entry
                .source
                .as_path()
                .file_stem()
                .unwrap()
                .to_string_lossy();
            assert_eq!(got, *expected);
        }
    }
}
