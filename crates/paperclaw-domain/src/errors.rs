//! Domain-level error enums shared across crates.

use thiserror::Error;

/// Error surface for [`crate::TextExtractor`] implementations.
///
/// `Encrypted` is the first-class signal for password-protected PDFs —
/// the ingest use-case routes those to [`crate::IngestOutcome::SkippedEncrypted`]
/// rather than aborting the batch.
#[derive(Debug, Error)]
pub enum ExtractionError {
    /// The PDF is encrypted / password-protected.
    #[error("encrypted PDF: {hint}")]
    Encrypted {
        /// Adapter-specific hint shown to the user (e.g. "AES-256, owner password set").
        hint: String,
    },
    /// The bytes are not a parseable PDF (or another format we don't yet
    /// support — OCR territory).
    #[error("unsupported document format: {0}")]
    Unsupported(String),
    /// I/O error inside the adapter (rare — most adapters operate on
    /// already-loaded bytes).
    #[error("I/O error: {0}")]
    Io(String),
    /// Adapter is not implemented yet.
    #[error("text extraction not implemented for this adapter")]
    NotImplemented,
    /// Anything else.
    #[error("extraction failed: {0}")]
    Other(String),
}

/// Error surface for the ingest use-case.
#[derive(Debug, Error)]
pub enum IngestError {
    /// Couldn't list / read the inbox.
    #[error("inbox access failed: {0}")]
    Inbox(String),
    /// Storage write failed.
    #[error("library write failed: {0}")]
    Store(String),
    /// Classifier failure (network, parsing, …).
    #[error("classifier failed: {0}")]
    Classifier(String),
    /// Catch-all for adapter errors that don't fit the categories above.
    #[error("ingest failed: {0}")]
    Other(String),
}
