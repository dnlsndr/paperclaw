//! Trait ports — the boundary the application layer talks across.
//!
//! Every port is `Send + Sync` and uses `#[async_trait]` so it stays
//! dyn-compatible behind `Arc<dyn Trait>`. Concrete impls live in
//! `paperclaw-adapters`.

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::errors::ExtractionError;
use crate::types::{
    Classification, DocumentId, IngestEntry, LibraryPath, PendingDocument, SearchHit, Transcript,
};

/// Source of pending PDFs (typically a filesystem inbox folder).
#[async_trait]
pub trait InboxSource: Send + Sync {
    /// List and load every pending document. Adapters decide ordering —
    /// the use-case does not assume any.
    async fn pending(&self) -> Result<Vec<PendingDocument>, InboxError>;
}

/// Convert PDF bytes into a [`Transcript`].
///
/// Encrypted PDFs **must** surface as [`ExtractionError::Encrypted`] so the
/// ingest pipeline can record `SkippedEncrypted` without aborting.
#[async_trait]
pub trait TextExtractor: Send + Sync {
    /// Extract text from a PDF document.
    async fn extract(&self, bytes: &[u8]) -> Result<Transcript, ExtractionError>;
}

/// Classify a transcript into a [`Classification`]. Shape is intentionally
/// minimal — expect refinement when M2 / M3 give it a real caller.
#[async_trait]
pub trait Classifier: Send + Sync {
    /// Produce a classification for the given transcript.
    async fn classify(&self, transcript: &Transcript) -> Result<Classification, ClassifierError>;
}

/// Persist an ingested document to the library.
#[async_trait]
pub trait LibraryStore: Send + Sync {
    /// Write `(pdf_bytes, transcript, classification)` under `target`.
    /// Adapters are responsible for sibling extensions (`.pdf`, `.md`,
    /// `.paperclaw.json`) and collision-suffix handling.
    async fn store(
        &self,
        target: &LibraryPath,
        pdf_bytes: &[u8],
        transcript: &Transcript,
        classification: &Classification,
        ingested_at: OffsetDateTime,
        id: DocumentId,
    ) -> Result<LibraryPath, StoreError>;

    /// Append a structured ingest entry to the library's log. Adapters
    /// typically append to a JSONL file under `library/_logs/`.
    async fn append_log(&self, entry: &IngestEntry) -> Result<(), StoreError>;
}

/// Query the library for matching documents. Shape will firm up at M3.
#[async_trait]
pub trait SearchIndex: Send + Sync {
    /// Return ranked hits for the given free-form query.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError>;
}

/// Inject time so use-case tests are deterministic.
pub trait Clock: Send + Sync {
    /// Current UTC time.
    fn now(&self) -> OffsetDateTime;
}

/// Inject document IDs so use-case tests are deterministic.
pub trait IdGenerator: Send + Sync {
    /// Mint a fresh document ID.
    fn new_id(&self) -> Uuid;
}

// ---------------------------------------------------------------------------
// Port-local error types — kept here so adapters don't import `errors` just
// to implement a trait.
// ---------------------------------------------------------------------------

/// Errors raised by [`InboxSource`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    /// Underlying I/O failure.
    #[error("inbox I/O error: {0}")]
    Io(String),
    /// Path was not configured / not found.
    #[error("inbox not available: {0}")]
    Unavailable(String),
}

/// Errors raised by [`Classifier`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum ClassifierError {
    /// Provider / network / parse failure.
    #[error("classifier transport failed: {0}")]
    Transport(String),
    /// Classifier hasn't been implemented yet.
    #[error("classifier not implemented")]
    NotImplemented,
}

/// Errors raised by [`LibraryStore`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying I/O failure.
    #[error("store I/O error: {0}")]
    Io(String),
    /// Serialization failure (metadata sidecar, log).
    #[error("serialization failed: {0}")]
    Serialization(String),
    /// Store hasn't been implemented yet.
    #[error("store not implemented")]
    NotImplemented,
}

/// Errors raised by [`SearchIndex`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// Underlying I/O failure.
    #[error("search I/O error: {0}")]
    Io(String),
    /// Search hasn't been implemented yet (M3).
    #[error("search not implemented")]
    NotImplemented,
}
