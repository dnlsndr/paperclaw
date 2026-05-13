//! Placeholder PDF text extractor.
//!
//! At M1 we only ship the trait surface; the real implementation choice
//! (pure-Rust crate vs shelling to `pdftotext`) is deferred to M2 when
//! we have real inputs to test against. Until then, the adapter returns
//! [`ExtractionError::NotImplemented`] so the type system tracks the
//! unfinished work.

use async_trait::async_trait;
use paperclaw_domain::TextExtractor;
use paperclaw_domain::errors::ExtractionError;
use paperclaw_domain::types::Transcript;

/// Production PDF text extractor (M2 will replace the body).
#[derive(Debug, Clone, Default)]
pub struct PdfTextExtractor;

impl PdfTextExtractor {
    /// Construct the extractor. Stateless today.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TextExtractor for PdfTextExtractor {
    async fn extract(&self, _bytes: &[u8]) -> Result<Transcript, ExtractionError> {
        Err(ExtractionError::NotImplemented)
    }
}
