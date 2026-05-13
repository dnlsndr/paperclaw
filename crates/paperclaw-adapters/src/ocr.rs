//! Chain-of-responsibility wrapper for layered text extraction.
//!
//! `FallbackExtractor` runs `primary` first and only falls through to
//! `fallback` when:
//!
//! - the primary returns `Ok(transcript)` with empty content, or
//! - the primary returns a non-encryption, non-fatal error
//!   ([`ExtractionError::Unsupported`] / [`ExtractionError::NotImplemented`]).
//!
//! `Encrypted` is **never** retried — encryption is a content-level
//! decision the user has to resolve, not an extractor-stack problem. The
//! same goes for `Io` errors (which usually indicate something worse
//! than "this extractor doesn't work").
//!
//! This is the slot that OCR will plug into at M3+: ship a
//! `TesseractExtractor` as the fallback. The ingest use-case never has to
//! learn the difference.

use std::sync::Arc;

use async_trait::async_trait;
use paperclaw_domain::TextExtractor;
use paperclaw_domain::errors::ExtractionError;
use paperclaw_domain::types::Transcript;
use tracing::debug;

/// Try `primary`, then `fallback` for empty / unsupported results.
pub struct FallbackExtractor {
    primary: Arc<dyn TextExtractor>,
    fallback: Arc<dyn TextExtractor>,
}

impl std::fmt::Debug for FallbackExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackExtractor").finish_non_exhaustive()
    }
}

impl FallbackExtractor {
    /// Build a chain. `primary` runs first; `fallback` only runs when the
    /// primary produces no usable transcript.
    #[must_use]
    pub fn new(primary: Arc<dyn TextExtractor>, fallback: Arc<dyn TextExtractor>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl TextExtractor for FallbackExtractor {
    async fn extract(&self, bytes: &[u8]) -> Result<Transcript, ExtractionError> {
        match self.primary.extract(bytes).await {
            Ok(t) if !t.is_empty() => Ok(t),
            Ok(_empty) => {
                debug!("primary extractor produced empty transcript; trying fallback");
                self.fallback.extract(bytes).await
            }
            Err(e) => match e {
                fatal @ (ExtractionError::Encrypted { .. } | ExtractionError::Io(_)) => Err(fatal),
                soft => {
                    debug!(error = %soft, "primary extractor failed non-fatally; trying fallback");
                    self.fallback.extract(bytes).await
                }
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use paperclaw_domain::testing::{EncryptedExtractor, StubExtractor};

    use super::*;

    /// Always returns an empty transcript.
    struct EmptyExtractor;

    #[async_trait]
    impl TextExtractor for EmptyExtractor {
        async fn extract(&self, _bytes: &[u8]) -> Result<Transcript, ExtractionError> {
            Ok(Transcript::new(""))
        }
    }

    /// Always returns `NotImplemented` — simulates a stub primary.
    struct NotImplementedExtractor;

    #[async_trait]
    impl TextExtractor for NotImplementedExtractor {
        async fn extract(&self, _bytes: &[u8]) -> Result<Transcript, ExtractionError> {
            Err(ExtractionError::NotImplemented)
        }
    }

    #[tokio::test]
    async fn falls_through_when_primary_is_empty() {
        let chain = FallbackExtractor::new(
            Arc::new(EmptyExtractor),
            Arc::new(StubExtractor::returning("from fallback")),
        );
        let t = chain.extract(b"%PDF-1.4").await.unwrap();
        assert_eq!(t.as_str(), "from fallback");
    }

    #[tokio::test]
    async fn falls_through_when_primary_is_not_implemented() {
        let chain = FallbackExtractor::new(
            Arc::new(NotImplementedExtractor),
            Arc::new(StubExtractor::returning("OCR result")),
        );
        let t = chain.extract(b"%PDF-1.4").await.unwrap();
        assert_eq!(t.as_str(), "OCR result");
    }

    #[tokio::test]
    async fn primary_text_wins_when_present() {
        let chain = FallbackExtractor::new(
            Arc::new(StubExtractor::returning("primary text")),
            Arc::new(StubExtractor::returning("fallback should not run")),
        );
        let t = chain.extract(b"%PDF-1.4").await.unwrap();
        assert_eq!(t.as_str(), "primary text");
    }

    #[tokio::test]
    async fn encrypted_is_never_retried() {
        let chain = FallbackExtractor::new(
            Arc::new(EncryptedExtractor),
            Arc::new(StubExtractor::returning("fallback should not run")),
        );
        let err = chain.extract(b"%PDF-1.4").await.unwrap_err();
        assert!(matches!(err, ExtractionError::Encrypted { .. }));
    }
}
