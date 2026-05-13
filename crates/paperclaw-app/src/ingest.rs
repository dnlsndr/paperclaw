//! The ingest use-case: inbox → classify → store → report.

use std::path::Path;
use std::sync::Arc;

use paperclaw_domain::ports::{ClassifierError, InboxError, StoreError};
use paperclaw_domain::types::{Classification, IngestEntry, IngestOutcome, IngestReport};
use paperclaw_domain::{
    Classifier, Clock, Document, DocumentId, ExtractionError, IdGenerator, InboxSource,
    LibraryPathPolicy, LibraryStore, PendingDocument, TextExtractor, Transcript,
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
    /// Returns an [`IngestReport`] enumerating per-document outcomes.
    /// Whole-batch infrastructure failures (inbox unreadable, store
    /// completely down) bubble up as [`AppError`].
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

        let mut report = IngestReport::default();

        for doc in pending {
            let entry = self.ingest_one(doc).await;
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

    async fn ingest_one(&self, pending: PendingDocument) -> IngestEntry {
        let source = pending.source.clone();
        let outcome = self.process(&pending).await;
        IngestEntry { source, outcome }
    }

    async fn process(&self, pending: &PendingDocument) -> IngestOutcome {
        let transcript = match self.extractor.extract(&pending.bytes).await {
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

        if classification.confidence.is_low() {
            // Still file under _unsorted/ so the user can review it, but
            // mark the outcome distinctly so the CLI can summarize.
            return self
                .file(
                    pending,
                    &transcript,
                    classification,
                    /* low_conf */ true,
                )
                .await;
        }

        self.file(
            pending,
            &transcript,
            classification,
            /* low_conf */ false,
        )
        .await
    }

    async fn file(
        &self,
        pending: &PendingDocument,
        transcript: &Transcript,
        classification: Classification,
        low_confidence: bool,
    ) -> IngestOutcome {
        let ingested_at = self.clock.now();
        let id = DocumentId::from_uuid(self.ids.new_id());
        let original_stem = original_stem(pending.source.as_path());
        let target = self
            .policy
            .path_for(&classification, &original_stem, ingested_at);

        match self
            .store
            .store(
                &target,
                &pending.bytes,
                transcript,
                &classification,
                ingested_at,
                id,
            )
            .await
        {
            Ok(library_path) => {
                let document = Document {
                    id,
                    source: pending.source.clone(),
                    library_path,
                    classification: classification.clone(),
                    ingested_at,
                };
                if low_confidence {
                    IngestOutcome::SkippedLowConfidence {
                        classification: Box::new(classification),
                    }
                } else {
                    IngestOutcome::Filed {
                        document: Box::new(document),
                    }
                }
            }
            Err(e) => IngestOutcome::Failed {
                reason: format!("store failed: {e}"),
            },
        }
    }
}

fn original_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("document")
        .to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use paperclaw_domain::testing::{
        EncryptedExtractor, FixedClock, InMemoryInbox, InMemoryLibraryStore, SeqIdGenerator,
        StubClassifier, StubExtractor, assert_send, assert_sync,
    };
    use paperclaw_domain::types::{IngestOutcome, PendingDocument, SourcePath};
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
    }
}
