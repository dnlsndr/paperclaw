//! In-memory fakes for unit tests, gated behind the `testing` feature.
//!
//! These let other crates write deterministic, dependency-free tests
//! against the trait surface without pulling in `mockall` or hand-rolling
//! doubles. Use `paperclaw-domain = { ..., features = ["testing"] }` in
//! `dev-dependencies`.

use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::errors::ExtractionError;
use crate::ports::{
    Classifier, ClassifierError, Clock, IdGenerator, InboxError, InboxSource, LibraryStore,
    SearchError, SearchIndex, StoreError, TextExtractor,
};
use crate::types::{
    Classification, Confidence, DocumentId, DocumentKind, IngestEntry, LibraryPath,
    PendingDocument, SearchHit, SourcePath, Transcript,
};

/// Compile-time helper: assert a type is `Send`. Consumers call this in
/// their own `const _: fn() = || { assert_send::<MyService>(); };` shims
/// to catch `!Send` regressions early.
pub fn assert_send<T: Send>() {}

/// Compile-time helper: assert a type is `Sync`.
pub fn assert_sync<T: Sync>() {}

/// Clock that always returns the same instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub OffsetDateTime);

impl FixedClock {
    /// Build a clock pinned to `t`.
    #[must_use]
    pub const fn new(t: OffsetDateTime) -> Self {
        Self(t)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

/// ID generator that hands out sequential UUIDs (v4 namespace bytes set
/// from the counter). Pure-deterministic so tests can assert on IDs.
#[derive(Debug, Default)]
pub struct SeqIdGenerator {
    counter: Mutex<u128>,
}

impl SeqIdGenerator {
    /// Fresh generator starting at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counter: Mutex::new(0),
        }
    }
}

impl IdGenerator for SeqIdGenerator {
    fn new_id(&self) -> Uuid {
        let mut guard = self
            .counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard += 1;
        Uuid::from_u128(*guard)
    }
}

/// In-memory inbox seeded with pre-loaded documents.
///
/// Tracks `consume` calls so use-case tests can assert the post-ingest
/// inbox state without poking at internal fields.
#[derive(Debug, Default)]
pub struct InMemoryInbox {
    docs: Mutex<Vec<PendingDocument>>,
    consumed: Mutex<Vec<SourcePath>>,
}

impl InMemoryInbox {
    /// Empty inbox.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            docs: Mutex::new(Vec::new()),
            consumed: Mutex::new(Vec::new()),
        }
    }

    /// Seeded inbox.
    #[must_use]
    pub fn with(docs: Vec<PendingDocument>) -> Self {
        Self {
            docs: Mutex::new(docs),
            consumed: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot of every source path that `consume` has been called for.
    #[must_use]
    pub fn consumed(&self) -> Vec<SourcePath> {
        let guard = self
            .consumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    }
}

#[async_trait]
impl InboxSource for InMemoryInbox {
    async fn pending(&self) -> Result<Vec<PendingDocument>, InboxError> {
        let mut guard = self
            .docs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(std::mem::take(&mut *guard))
    }

    async fn consume(&self, source: &SourcePath) -> Result<(), InboxError> {
        let mut guard = self
            .consumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(source.clone());
        Ok(())
    }
}

/// Extractor that always returns the same transcript.
#[derive(Debug, Clone)]
pub struct StubExtractor(pub Transcript);

impl StubExtractor {
    /// Build an extractor that returns `text` for every input.
    #[must_use]
    pub fn returning(text: impl Into<String>) -> Self {
        Self(Transcript::new(text))
    }
}

#[async_trait]
impl TextExtractor for StubExtractor {
    async fn extract(&self, _bytes: &[u8]) -> Result<Transcript, ExtractionError> {
        Ok(self.0.clone())
    }
}

/// Extractor that always reports the input PDF as encrypted.
#[derive(Debug, Clone, Default)]
pub struct EncryptedExtractor;

#[async_trait]
impl TextExtractor for EncryptedExtractor {
    async fn extract(&self, _bytes: &[u8]) -> Result<Transcript, ExtractionError> {
        Err(ExtractionError::Encrypted {
            hint: "test fixture: encrypted PDF".to_owned(),
        })
    }
}

/// Classifier that returns a configured [`Classification`] for any input.
#[derive(Debug, Clone)]
pub struct StubClassifier(pub Classification);

impl StubClassifier {
    /// Build a classifier returning a confident invoice classification.
    /// Useful as a default test fixture.
    #[must_use]
    pub fn confident_invoice() -> Self {
        Self(Classification {
            kind: DocumentKind::Invoice,
            confidence: Confidence::new(0.95),
            sender: Some("test-sender".to_owned()),
            subject: Some("test-subject".to_owned()),
            document_date: None,
            rationale: None,
        })
    }

    /// Build a classifier returning a low-confidence classification.
    #[must_use]
    pub fn low_confidence() -> Self {
        Self(Classification {
            kind: DocumentKind::Unsorted,
            confidence: Confidence::new(0.1),
            sender: None,
            subject: None,
            document_date: None,
            rationale: Some("test fixture: low confidence".to_owned()),
        })
    }
}

#[async_trait]
impl Classifier for StubClassifier {
    async fn classify(&self, _transcript: &Transcript) -> Result<Classification, ClassifierError> {
        Ok(self.0.clone())
    }
}

/// In-memory library store. Records every write for test assertions.
#[derive(Debug, Default)]
pub struct InMemoryLibraryStore {
    writes: Mutex<Vec<WrittenDocument>>,
    log: Mutex<Vec<IngestEntry>>,
}

/// A single recorded write into [`InMemoryLibraryStore`].
#[derive(Debug, Clone)]
pub struct WrittenDocument {
    /// Target library path.
    pub path: LibraryPath,
    /// PDF bytes that would have been written to disk.
    pub pdf_bytes: Vec<u8>,
    /// Transcript that would have been written.
    pub transcript: Transcript,
    /// Classification that would have been written.
    pub classification: Classification,
    /// Document ID minted by the use-case.
    pub id: DocumentId,
}

impl InMemoryLibraryStore {
    /// Fresh store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot every recorded write so far.
    #[must_use]
    pub fn writes(&self) -> Vec<WrittenDocument> {
        let guard = self
            .writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    }

    /// Snapshot every recorded log entry so far.
    #[must_use]
    pub fn log_entries(&self) -> Vec<IngestEntry> {
        let guard = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    }
}

#[async_trait]
impl LibraryStore for InMemoryLibraryStore {
    async fn store(
        &self,
        target: &LibraryPath,
        pdf_bytes: &[u8],
        transcript: &Transcript,
        classification: &Classification,
        _ingested_at: OffsetDateTime,
        id: DocumentId,
    ) -> Result<LibraryPath, StoreError> {
        let mut guard = self
            .writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(WrittenDocument {
            path: target.clone(),
            pdf_bytes: pdf_bytes.to_vec(),
            transcript: transcript.clone(),
            classification: classification.clone(),
            id,
        });
        Ok(target.clone())
    }

    async fn append_log(&self, entry: &IngestEntry) -> Result<(), StoreError> {
        let mut guard = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(entry.clone());
        Ok(())
    }
}

/// Search index stub that always returns an empty result.
#[derive(Debug, Clone, Default)]
pub struct EmptySearchIndex;

#[async_trait]
impl SearchIndex for EmptySearchIndex {
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        Ok(Vec::new())
    }
}
