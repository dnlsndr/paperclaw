//! Core value types: documents, transcripts, classifications, outcomes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::slug::slugify;

/// Stable identifier for an ingested document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentId(pub Uuid);

impl DocumentId {
    /// Wrap an existing UUID. Use [`crate::IdGenerator`] to mint new ones.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Coarse category every ingested document is sorted into. The variants
/// double as folder names via [`AsRefStr`] / [`Display`].
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum DocumentKind {
    /// Outgoing or incoming commercial invoices.
    Invoice,
    /// Recurring utility / service bills.
    Bill,
    /// Legal contracts and agreements.
    Contract,
    /// Bank statements and account summaries.
    BankStatement,
    /// Insurance policies and correspondence.
    Insurance,
    /// Tax authority correspondence (e.g. Finanzamt letters).
    Tax,
    /// Classifier wasn't confident — needs human review.
    Unsorted,
    /// Anything not covered above. Carries the free-form label the
    /// classifier suggested. `#[strum(default)]` makes `EnumString` parse
    /// any unknown input into this variant; `Display`/`AsRefStr` render
    /// only the variant name, so use [`Self::folder_slug`] for filesystem
    /// output.
    #[strum(default)]
    Other(String),
}

impl DocumentKind {
    /// Folder slug used inside `library/`. Mirrors the strum kebab-case
    /// rendering for known variants and slugifies the free-form label
    /// for [`DocumentKind::Other`].
    #[must_use]
    pub fn folder_slug(&self) -> String {
        match self {
            Self::Other(label) => slugify(label),
            other => other.as_ref().to_owned(),
        }
    }
}

/// Confidence the classifier expresses about its choice, in `[0.0, 1.0]`.
/// Values outside that range are clamped on construction.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Confidence(f32);

impl Confidence {
    /// Treat anything below this as "unsorted".
    pub const LOW_CONFIDENCE_THRESHOLD: f32 = 0.5;

    /// Build a clamped confidence value.
    ///
    /// Non-finite inputs (NaN, ±infinity) collapse to `0.0`. `f32::clamp`
    /// passes NaN through, which would silently let an "unsorted-looking"
    /// classification slip past `is_low()` (since `NaN < 0.5` is false).
    /// Treating NaN as zero is conservative: the doc lands in
    /// `_unsorted/` and the user can review.
    #[must_use]
    pub fn new(value: f32) -> Self {
        if !value.is_finite() {
            return Self(0.0);
        }
        Self(value.clamp(0.0, 1.0))
    }

    /// Raw `f32` reading.
    #[must_use]
    pub const fn value(self) -> f32 {
        self.0
    }

    /// `true` when the classifier was not confident enough to file the
    /// document under its declared category.
    #[must_use]
    pub fn is_low(self) -> bool {
        self.0 < Self::LOW_CONFIDENCE_THRESHOLD
    }
}

/// Extracted text content of a PDF. Empty transcripts are a valid signal
/// (e.g. scanned PDF with no text layer) — call [`Transcript::is_empty`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript(String);

impl Transcript {
    /// Wrap an already-extracted string.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Borrow the underlying text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// `true` when there is no extracted text (or only whitespace).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// Rough quality hint: longer transcripts with diverse characters are
    /// more likely to be useful. Returns a value in `[0.0, 1.0]`.
    #[must_use]
    pub fn confidence_hint(&self) -> f32 {
        let len = self.0.trim().chars().count();
        match len {
            0 => 0.0,
            1..=64 => 0.3,
            65..=512 => 0.6,
            _ => 0.9,
        }
    }
}

/// A document discovered in the inbox, not yet ingested.
#[derive(Debug, Clone)]
pub struct PendingDocument {
    /// Original on-disk location inside the inbox.
    pub source: SourcePath,
    /// File bytes already loaded into memory.
    pub bytes: Vec<u8>,
}

/// Filesystem path within the inbox. Newtype to keep it from being
/// confused with library paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourcePath(pub PathBuf);

impl SourcePath {
    /// Build from anything path-like.
    pub fn new(p: impl Into<PathBuf>) -> Self {
        Self(p.into())
    }

    /// Borrow the inner path.
    #[must_use]
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

/// Target path within the library, including category folder and
/// extensionless stem. Adapters compose `.pdf` / `.md` / `.paperclaw.json`
/// siblings off the same stem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LibraryPath {
    /// Category folder slug (e.g. `invoices`).
    pub category: String,
    /// File stem without extension (e.g. `2026-03-15_acme-co_inv-1234`).
    pub stem: String,
}

impl LibraryPath {
    /// Render the stem inside the category folder. Adapters append the
    /// concrete extension.
    #[must_use]
    pub fn relative_stem(&self) -> PathBuf {
        PathBuf::from(&self.category).join(&self.stem)
    }
}

/// Classifier output for a single document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    /// Category the classifier picked.
    pub kind: DocumentKind,
    /// Confidence in the choice.
    pub confidence: Confidence,
    /// Sender / counterparty as the classifier sees it (slugified later).
    pub sender: Option<String>,
    /// Short subject for the filename (slugified later).
    pub subject: Option<String>,
    /// Date encoded in the document, if any.
    pub document_date: Option<time::Date>,
    /// Free-form reasoning the classifier wants to record.
    pub rationale: Option<String>,
}

/// Final on-disk record of an ingested document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Stable ID minted at ingest time.
    pub id: DocumentId,
    /// Original inbox path before move/rename.
    pub source: SourcePath,
    /// Where it landed inside the library.
    pub library_path: LibraryPath,
    /// The classifier's decision.
    pub classification: Classification,
    /// When ingest finished.
    pub ingested_at: OffsetDateTime,
}

/// Outcome of attempting to ingest one PDF.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestOutcome {
    /// Document was classified and filed successfully.
    Filed {
        /// Resulting filed document.
        document: Box<Document>,
    },
    /// PDF was encrypted; user must decrypt and re-drop it.
    SkippedEncrypted {
        /// Hint surfaced by the extractor.
        hint: String,
    },
    /// Classifier confidence was below threshold; routed to `_unsorted/`.
    SkippedLowConfidence {
        /// The low-confidence classification, kept for audit.
        classification: Box<Classification>,
    },
    /// Anything else that prevented ingest.
    Failed {
        /// Human-readable reason.
        reason: String,
    },
}

/// Aggregate result of an ingest batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestReport {
    /// One entry per input file, in processing order.
    pub entries: Vec<IngestEntry>,
}

impl IngestReport {
    /// `true` if no files were processed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total entries (filed + skipped + failed).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Count of files successfully filed.
    #[must_use]
    pub fn filed_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.outcome, IngestOutcome::Filed { .. }))
            .count()
    }

    /// Count of encrypted PDFs skipped.
    #[must_use]
    pub fn encrypted_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.outcome, IngestOutcome::SkippedEncrypted { .. }))
            .count()
    }
}

/// Single entry in an [`IngestReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestEntry {
    /// Where the file came from.
    pub source: SourcePath,
    /// What happened to it.
    pub outcome: IngestOutcome,
}

/// A search result returned by [`crate::SearchIndex`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// The document that matched.
    pub document_id: DocumentId,
    /// Where its files live.
    pub library_path: LibraryPath,
    /// Optional snippet around the match.
    pub snippet: Option<String>,
    /// Relevance score; higher is better.
    pub score: f32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn confidence_clamps_into_unit_interval() {
        assert!((Confidence::new(-0.5).value() - 0.0).abs() < f32::EPSILON);
        assert!((Confidence::new(1.5).value() - 1.0).abs() < f32::EPSILON);
        assert!(Confidence::new(0.2).is_low());
        assert!(!Confidence::new(0.9).is_low());
    }

    #[test]
    fn confidence_treats_non_finite_as_zero() {
        // Without the NaN guard, f32::clamp passes NaN through and the
        // resulting `Confidence` would be "not low" — see `is_low`.
        let nan = Confidence::new(f32::NAN);
        assert!(nan.is_low(), "NaN must read as low confidence");
        let pos_inf = Confidence::new(f32::INFINITY);
        let neg_inf = Confidence::new(f32::NEG_INFINITY);
        assert!(pos_inf.is_low());
        assert!(neg_inf.is_low());
    }

    #[test]
    fn document_kind_folder_slug_uses_kebab_case() {
        assert_eq!(DocumentKind::BankStatement.folder_slug(), "bank-statement");
        assert_eq!(DocumentKind::Tax.folder_slug(), "tax");
        assert_eq!(
            DocumentKind::Other("Other Stuff!".into()).folder_slug(),
            "other-stuff",
        );
    }

    #[test]
    fn transcript_emptiness_treats_whitespace_as_empty() {
        assert!(Transcript::new("   \n\t").is_empty());
        assert!(!Transcript::new("hello").is_empty());
    }
}
