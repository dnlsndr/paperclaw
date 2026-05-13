//! Filename + library-path policy. Lives in the domain so adapters can't
//! drift from the on-disk convention.

use time::{Date, OffsetDateTime, format_description::FormatItem, macros::format_description};

use crate::slug::slugify;
use crate::types::{Classification, LibraryPath};

const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// Upper bound on the rendered stem length (without extension).
///
/// Sized to stay well under the 255-byte path-component limit on ext4 and
/// the 260-char total path limit on default Windows. The three sibling
/// extensions (`.pdf` / `.md` / `.paperclaw.json`) add up to 15 chars, so
/// 120 leaves room for category folders and library roots.
const MAX_STEM_LEN: usize = 120;

/// Per-component cap before assembly. Date prefix is 11 chars including
/// the trailing separator, so two ~50-char components fit comfortably
/// under [`MAX_STEM_LEN`].
const MAX_COMPONENT_LEN: usize = 50;

/// Stable, deterministic mapping from `(classification, original_name,
/// ingest_time)` to a [`LibraryPath`].
///
/// Filename rule: `YYYY-MM-DD_<sender>_<subject>` where the date prefers
/// the document's own date and falls back to the ingest date. Sender and
/// subject are slugified to `[a-z0-9-]+`; missing pieces fall back to the
/// original file stem.
///
/// Low-confidence classifications route to `_unsorted/`.
#[derive(Debug, Clone, Default)]
pub struct LibraryPathPolicy;

impl LibraryPathPolicy {
    /// Construct a fresh policy. Stateless today; left as a struct so we
    /// can swap rules in later without changing call sites.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Compute the library path for a given classification.
    ///
    /// `original_stem` is the inbox file's stem (without extension) and
    /// is used as a fallback when the classifier didn't supply a sender
    /// or subject.
    ///
    /// The `&self` receiver is kept (rather than dropped to an associated
    /// function) because the policy is going to grow state (per-category
    /// overrides, alternate slug rules) in M2/M3.
    #[must_use]
    #[allow(clippy::unused_self)]
    pub fn path_for(
        &self,
        classification: &Classification,
        original_stem: &str,
        ingested_at: OffsetDateTime,
    ) -> LibraryPath {
        let category = if classification.confidence.is_low() {
            "_unsorted".to_owned()
        } else {
            classification.kind.folder_slug()
        };

        let date = classification
            .document_date
            .unwrap_or_else(|| ingested_at.date());

        let stem = build_stem(date, classification, original_stem);

        LibraryPath { category, stem }
    }
}

fn build_stem(date: Date, classification: &Classification, original_stem: &str) -> String {
    let date_str = date
        .format(DATE_FORMAT)
        .unwrap_or_else(|_| "0000-00-00".to_owned());

    let sender = classification
        .sender
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slugify(original_stem));
    let sender = trim_component(&sender);

    let subject = classification
        .subject
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .as_deref()
        .map(trim_component);

    let stem = match subject {
        Some(sub) => format!("{date_str}_{sender}_{sub}"),
        None => format!("{date_str}_{sender}"),
    };

    // Belt-and-braces guard: even if a component squeaked past
    // `trim_component`, the final stem must fit `MAX_STEM_LEN`.
    if stem.len() <= MAX_STEM_LEN {
        return stem;
    }
    let mut truncated: String = stem.chars().take(MAX_STEM_LEN).collect();
    while truncated.ends_with('-') || truncated.ends_with('_') {
        truncated.pop();
    }
    truncated
}

fn trim_component(s: &str) -> String {
    if s.len() <= MAX_COMPONENT_LEN {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(MAX_COMPONENT_LEN).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::types::{Confidence, DocumentKind};

    fn classification(
        kind: DocumentKind,
        confidence: f32,
        sender: Option<&str>,
        subject: Option<&str>,
        date: Option<Date>,
    ) -> Classification {
        Classification {
            kind,
            confidence: Confidence::new(confidence),
            sender: sender.map(str::to_owned),
            subject: subject.map(str::to_owned),
            document_date: date,
            rationale: None,
        }
    }

    #[test]
    fn confident_classification_routes_into_category_folder() {
        let policy = LibraryPathPolicy::new();
        let cls = classification(
            DocumentKind::Invoice,
            0.9,
            Some("Acme & Co."),
            Some("Invoice #1234"),
            Some(time::macros::date!(2026 - 03 - 15)),
        );
        let path = policy.path_for(&cls, "scan-001", datetime!(2026-05-13 12:00 UTC));
        assert_eq!(path.category, "invoice");
        assert_eq!(path.stem, "2026-03-15_acme-co_invoice-1234");
    }

    #[test]
    fn low_confidence_routes_to_unsorted() {
        let policy = LibraryPathPolicy::new();
        let cls = classification(DocumentKind::Invoice, 0.2, None, None, None);
        let path = policy.path_for(&cls, "scan-001", datetime!(2026-05-13 12:00 UTC));
        assert_eq!(path.category, "_unsorted");
        assert!(path.stem.starts_with("2026-05-13_"));
    }

    #[test]
    fn missing_sender_falls_back_to_original_stem() {
        let policy = LibraryPathPolicy::new();
        let cls = classification(DocumentKind::Bill, 0.9, None, Some("Strom"), None);
        let path = policy.path_for(&cls, "Some_Original FILE", datetime!(2026-05-13 12:00 UTC));
        assert_eq!(path.stem, "2026-05-13_some-original-file_strom");
    }

    #[test]
    fn long_sender_and_subject_get_capped() {
        let policy = LibraryPathPolicy::new();
        let very_long = "a".repeat(200);
        let cls = classification(
            DocumentKind::Invoice,
            0.9,
            Some(&very_long),
            Some(&very_long),
            None,
        );
        let path = policy.path_for(&cls, "fallback", datetime!(2026-05-13 12:00 UTC));
        assert!(
            path.stem.len() <= 120,
            "stem must respect filename-length cap, got {} chars: {}",
            path.stem.len(),
            path.stem,
        );
        // Date prefix preserved.
        assert!(path.stem.starts_with("2026-05-13_"));
    }
}
