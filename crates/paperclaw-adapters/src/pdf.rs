//! PDF text extractor backed by [`pdf_extract`].
//!
//! The crate is pure-Rust (built on `lopdf`) so we avoid a system
//! dependency on poppler/`pdftotext`. It is, however, synchronous and has
//! been observed to panic on adversarial input — so per the design doc's
//! M2 hardening line we:
//!
//! - Run the parse on a [`tokio::task::spawn_blocking`] worker, which
//!   converts panics into a [`JoinError`] we can translate to
//!   [`ExtractionError::Other`] instead of taking the batch down.
//! - Wrap the whole thing in [`tokio::time::timeout`] (30s) so a
//!   pathological PDF can't wedge the per-document task.
//! - Detect encrypted documents via the `Encrypted` error variant and the
//!   `/Encrypt` magic in the raw bytes as a belt-and-braces backstop, and
//!   surface them as [`ExtractionError::Encrypted`].
//!
//! [`JoinError`]: tokio::task::JoinError

use std::time::Duration;

use async_trait::async_trait;
use paperclaw_domain::TextExtractor;
use paperclaw_domain::errors::ExtractionError;
use paperclaw_domain::types::{SourceMedia, Transcript};
use tracing::warn;

/// Maximum wall-clock time the extractor will spend on a single document.
/// Past this the per-doc task is abandoned and the ingest pipeline records
/// the failure rather than wedging the whole batch.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Production PDF text extractor.
#[derive(Debug, Clone, Default)]
pub struct PdfTextExtractor;

impl PdfTextExtractor {
    /// Construct the extractor. Stateless.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TextExtractor for PdfTextExtractor {
    async fn extract(&self, source: SourceMedia<'_>) -> Result<Transcript, ExtractionError> {
        // Images route past this extractor through `FallbackExtractor`; we
        // surface a soft Unsupported so the chain can try the next link
        // (the vision-backed extractor) without aborting the batch.
        if !source.media_type.is_pdf() {
            return Err(ExtractionError::Unsupported(format!(
                "PdfTextExtractor cannot read {:?}",
                source.media_type,
            )));
        }
        let bytes = source.bytes;
        if !bytes.starts_with(b"%PDF-") {
            return Err(ExtractionError::Unsupported(
                "missing %PDF- magic prefix".to_owned(),
            ));
        }

        // Clone bytes into the blocking task — `pdf_extract` runs sync and
        // we don't want to hold the borrow across the .await.
        let owned = bytes.to_vec();
        let join = tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&owned));

        let join_result = match tokio::time::timeout(EXTRACT_TIMEOUT, join).await {
            Ok(r) => r,
            Err(_elapsed) => {
                return Err(ExtractionError::Other(format!(
                    "extractor timed out after {}s",
                    EXTRACT_TIMEOUT.as_secs(),
                )));
            }
        };

        let parse_result = match join_result {
            Ok(r) => r,
            Err(join_err) if join_err.is_panic() => {
                // Decision (DESIGN §9 panic policy): adapters that wrap a
                // parser which can crash on adversarial input must isolate
                // it inside spawn_blocking and translate the resulting
                // JoinError to ExtractionError::Other. Panics here are NOT
                // a programmer bug in PaperClaw — they're hostile input.
                warn!("pdf_extract panicked on input; recording as extraction failure");
                return Err(ExtractionError::Other(
                    "PDF parser crashed on this document".to_owned(),
                ));
            }
            Err(join_err) => {
                return Err(ExtractionError::Io(format!(
                    "blocking task ended unexpectedly: {join_err}",
                )));
            }
        };

        match parse_result {
            Ok(text) => Ok(Transcript::new(text)),
            Err(e) => Err(translate_extract_error(&e, bytes)),
        }
    }
}

/// Map a [`pdf_extract::OutputError`] onto our [`ExtractionError`].
///
/// `pdf_extract` does not expose a typed `Encrypted` variant; it folds the
/// case into a `PdfError(LopdfError::Decryption)`. We match on the Debug
/// rendering rather than reach into private types — fragile, but the
/// belt-and-braces `/Encrypt` byte-level check below catches anything the
/// string match misses.
fn translate_extract_error(err: &pdf_extract::OutputError, bytes: &[u8]) -> ExtractionError {
    let rendered = format!("{err:?}").to_ascii_lowercase();
    let mentions_encryption =
        rendered.contains("decrypt") || rendered.contains("encrypt") || rendered.contains("crypt");

    if mentions_encryption || pdf_bytes_look_encrypted(bytes) {
        return ExtractionError::Encrypted {
            hint: "PDF requires a password (decrypt it and re-drop in inbox/)".to_owned(),
        };
    }

    ExtractionError::Unsupported(format!("pdf-extract failed: {err}"))
}

/// Cheap heuristic: a PDF that contains the `/Encrypt` indirect-reference
/// keyword is almost certainly password-protected. The keyword can appear
/// inside a content stream as well, but for inbox-level routing the false
/// positive rate is acceptable: misclassified docs land in `SkippedEncrypted`
/// and the user can drop them back after confirming.
fn pdf_bytes_look_encrypted(bytes: &[u8]) -> bool {
    // Limit the scan to the first 64 KiB — the trailer / /Encrypt entry
    // lives in the document catalog near the start or end of the file.
    let head_window = &bytes[..bytes.len().min(64 * 1024)];
    let tail_start = bytes.len().saturating_sub(64 * 1024);
    let tail_window = &bytes[tail_start..];
    contains(head_window, b"/Encrypt") || contains(tail_window, b"/Encrypt")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use paperclaw_domain::types::MediaType;

    use super::*;

    fn pdf_source(bytes: &[u8]) -> SourceMedia<'_> {
        SourceMedia::new(bytes, MediaType::Pdf)
    }

    #[tokio::test]
    async fn rejects_non_pdf_bytes_as_unsupported() {
        let err = PdfTextExtractor::new()
            .extract(pdf_source(b"not a pdf at all"))
            .await
            .unwrap_err();
        assert!(matches!(err, ExtractionError::Unsupported(_)));
    }

    #[tokio::test]
    async fn surfaces_garbage_pdf_as_unsupported_not_panic() {
        // `%PDF-` prefix passes the magic-byte gate but the body is junk;
        // we want a recorded failure, not a wedged task or a panic.
        let mut bytes = b"%PDF-1.4 ".to_vec();
        bytes.extend(std::iter::repeat_n(0u8, 256));
        let err = PdfTextExtractor::new()
            .extract(pdf_source(&bytes))
            .await
            .unwrap_err();
        match err {
            ExtractionError::Unsupported(_) | ExtractionError::Other(_) => {}
            other => panic!("expected Unsupported/Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_media_type_yields_soft_unsupported_for_fallback() {
        // FallbackExtractor relies on Unsupported being a soft error so the
        // chain advances to the vision extractor. Anything stronger here
        // would short-circuit the chain and leave images unprocessed.
        let err = PdfTextExtractor::new()
            .extract(SourceMedia::new(&[0xFF, 0xD8, 0xFF, 0xE0], MediaType::Jpeg))
            .await
            .unwrap_err();
        assert!(matches!(err, ExtractionError::Unsupported(_)));
    }

    #[test]
    fn encryption_byte_sniff_matches_pdf_with_encrypt_entry() {
        let bytes = b"%PDF-1.4\n1 0 obj\n<< /Encrypt 2 0 R >>\nendobj\n";
        assert!(pdf_bytes_look_encrypted(bytes));
    }

    #[test]
    fn encryption_byte_sniff_does_not_match_plain_pdf() {
        let bytes = b"%PDF-1.4\nhello world\n";
        assert!(!pdf_bytes_look_encrypted(bytes));
    }
}
