//! Classifier adapters.
//!
//! - [`RuleBasedClassifier`] — a deterministic keyword-matcher useful as
//!   the offline default and as a test fixture. M2 will likely refine
//!   the rule set.
//! - [`NotImplementedClassifier`] — explicit "not wired" placeholder. The
//!   CLI's `doctor` command surfaces this so the user knows what's live.
//!
//! M3 will add `AnthropicClassifier` here.

use async_trait::async_trait;
use paperclaw_domain::Classifier;
use paperclaw_domain::ports::ClassifierError;
use paperclaw_domain::types::{Classification, Confidence, DocumentKind, Transcript};

/// Naive keyword-driven classifier. Deterministic, free, easy to test —
/// good enough as the offline fallback and the M1 demo input.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleBasedClassifier;

impl RuleBasedClassifier {
    /// Construct the classifier. Stateless.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Version identifier the rule-based classifier writes into every
/// document's metadata sidecar. Bump alongside any change to
/// [`guess_kind`] so the agent can detect stale classifications.
const RULE_BASED_VERSION: &str = "rule-based:1";

#[async_trait]
impl Classifier for RuleBasedClassifier {
    async fn classify(&self, transcript: &Transcript) -> Result<Classification, ClassifierError> {
        let text = transcript.as_str().to_ascii_lowercase();
        let (kind, confidence) = guess_kind(&text);

        Ok(Classification {
            kind,
            confidence: Confidence::new(confidence),
            sender: None,
            subject: None,
            document_date: None,
            rationale: Some("rule-based keyword match".to_owned()),
        })
    }

    // See `paperclaw_domain::testing::StubClassifier::version` for why
    // the trait signature returns a `&self`-bound `&str` rather than a
    // `&'static str` directly.
    #[allow(clippy::unnecessary_literal_bound)]
    fn version(&self) -> &str {
        RULE_BASED_VERSION
    }
}

fn guess_kind(text: &str) -> (DocumentKind, f32) {
    // Keep the rule set short. M2 will replace this anyway.
    if text.contains("invoice") || text.contains("rechnung") {
        (DocumentKind::Invoice, 0.7)
    } else if text.contains("statement") || text.contains("kontoauszug") {
        (DocumentKind::BankStatement, 0.7)
    } else if text.contains("contract") || text.contains("vertrag") {
        (DocumentKind::Contract, 0.7)
    } else if text.contains("insurance") || text.contains("versicherung") {
        (DocumentKind::Insurance, 0.7)
    } else if text.contains("finanzamt") || text.contains("tax") {
        (DocumentKind::Tax, 0.7)
    } else if text.contains("bill") || text.contains("rechnung") || text.contains("stadtwerke") {
        (DocumentKind::Bill, 0.7)
    } else {
        (DocumentKind::Unsorted, 0.2)
    }
}

/// Explicit "not wired" classifier. Used until the real LLM-backed
/// adapter lands at M3, and as the `doctor` command's marker that the
/// pipeline isn't ready end-to-end.
#[derive(Debug, Clone, Copy, Default)]
pub struct NotImplementedClassifier;

#[async_trait]
impl Classifier for NotImplementedClassifier {
    async fn classify(&self, _transcript: &Transcript) -> Result<Classification, ClassifierError> {
        Err(ClassifierError::NotImplemented)
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn version(&self) -> &str {
        "not-implemented"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invoice_keyword_is_recognized() {
        let cls = RuleBasedClassifier::new()
            .classify(&Transcript::new("Invoice #1234 from Acme"))
            .await
            .unwrap();
        assert_eq!(cls.kind, DocumentKind::Invoice);
        assert!(!cls.confidence.is_low());
    }

    #[tokio::test]
    async fn unknown_content_falls_to_unsorted_low_confidence() {
        let cls = RuleBasedClassifier::new()
            .classify(&Transcript::new("the rain in spain"))
            .await
            .unwrap();
        assert_eq!(cls.kind, DocumentKind::Unsorted);
        assert!(cls.confidence.is_low());
    }

    #[tokio::test]
    async fn not_implemented_classifier_reports_explicitly() {
        let err = NotImplementedClassifier
            .classify(&Transcript::new("anything"))
            .await
            .unwrap_err();
        assert!(matches!(err, ClassifierError::NotImplemented));
    }
}
