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
    Classification, DocumentId, IngestEntry, LibraryPath, PendingDocument, SearchHit, SourceMedia,
    Transcript,
};

/// Source of pending PDFs (typically a filesystem inbox folder).
#[async_trait]
pub trait InboxSource: Send + Sync {
    /// List and load every pending document. Adapters decide ordering —
    /// the use-case does not assume any.
    async fn pending(&self) -> Result<Vec<PendingDocument>, InboxError>;

    /// Remove a document from the inbox after it has been successfully
    /// filed into the library. Adapters that back onto persistent storage
    /// delete the underlying file; in-memory adapters drop their entry.
    ///
    /// The use-case only calls `consume` after a write to the library has
    /// committed, so a failure here means the source remains in the inbox
    /// and the next run will see it again. Adapters must surface an error
    /// (rather than swallow it) so callers can log it.
    async fn consume(&self, source: &crate::types::SourcePath) -> Result<(), InboxError>;
}

/// Convert document bytes into a [`Transcript`].
///
/// The trait takes a [`SourceMedia`] (bytes + detected [`crate::MediaType`])
/// so an adapter can branch on format without re-sniffing the prefix. PDFs
/// route to text-layer extractors; image media types route to vision-backed
/// extractors. The [`crate::adapters`]-style `FallbackExtractor` chains
/// implementations so the use-case stays unaware of the strategy.
///
/// Encrypted PDFs **must** surface as [`ExtractionError::Encrypted`] so the
/// ingest pipeline can record `SkippedEncrypted` without aborting.
#[async_trait]
pub trait TextExtractor: Send + Sync {
    /// Extract text from a document.
    async fn extract(&self, source: SourceMedia<'_>) -> Result<Transcript, ExtractionError>;
}

/// Classify a transcript into a [`Classification`]. Shape is intentionally
/// minimal — expect refinement when M2 / M3 give it a real caller.
#[async_trait]
pub trait Classifier: Send + Sync {
    /// Produce a classification for the given transcript.
    async fn classify(&self, transcript: &Transcript) -> Result<Classification, ClassifierError>;

    /// Stable identifier for the classifier (kind + version / model).
    /// The use-case persists this in each document's metadata sidecar so
    /// the agent can later tell whether a doc needs re-classifying after
    /// a rule-set or model upgrade. Examples: `"rule-based:1"`,
    /// `"anthropic-claude-haiku-4-5"`.
    fn version(&self) -> &str;
}

/// Persist an ingested document to the library.
#[async_trait]
pub trait LibraryStore: Send + Sync {
    /// Write a document to the library. Adapters are responsible for
    /// sibling extensions (`.pdf`, `.md`, `.paperclaw.json`) and
    /// collision-suffix handling. Returns the resolved [`LibraryPath`]
    /// (which may carry a `-2`, `-3`, … suffix if the original collided).
    async fn store(&self, write: &LibraryWrite<'_>) -> Result<LibraryPath, StoreError>;

    /// Append a structured ingest entry to the library's log. Adapters
    /// typically append to a JSONL file under `library/_logs/`.
    async fn append_log(&self, entry: &IngestEntry) -> Result<(), StoreError>;
}

/// Bundle of inputs for a single [`LibraryStore::store`] call.
///
/// Grouped into a struct so future sidecar fields (content hash at M3,
/// page count, …) don't keep pushing the trait signature around.
#[derive(Debug)]
pub struct LibraryWrite<'a> {
    /// Computed target path (category + stem).
    pub target: &'a LibraryPath,
    /// Raw PDF bytes — adapter writes them under `.pdf`.
    pub pdf_bytes: &'a [u8],
    /// Extracted transcript — adapter writes it under `.md`.
    pub transcript: &'a Transcript,
    /// Classification result — embedded in the metadata sidecar.
    pub classification: &'a Classification,
    /// Original filename (with extension) from the inbox. Preserved
    /// verbatim — *not* slugified — so an audit can trace a library
    /// document back to its source file.
    pub original_filename: &'a str,
    /// `Classifier::version()` of whoever classified this document.
    /// Persisted to the sidecar so re-classification flows can detect
    /// stale entries.
    pub classifier_version: &'a str,
    /// Wall-clock time when ingest committed this document.
    pub ingested_at: OffsetDateTime,
    /// Stable document identifier minted at ingest.
    pub id: DocumentId,
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
