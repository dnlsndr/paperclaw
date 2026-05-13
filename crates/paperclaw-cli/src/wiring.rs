//! Composition root. Builds adapter instances and hands them to the
//! application services. Swap implementations here — nowhere else.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use paperclaw_adapters::{
    AnthropicApiKey, AnthropicClassifier, AnthropicClassifierConfig, AnthropicTransport,
    AnthropicVisionConfig, AnthropicVisionExtractor, FallbackExtractor, FsInboxSource,
    FsLibraryStore, GrepSearchIndex, PdfTextExtractor, ReqwestTransport, RuleBasedClassifier,
    SystemClock, UuidV4Generator,
};
use paperclaw_app::{IngestService, SearchService};
use paperclaw_domain::LibraryPathPolicy;

use crate::config::{AppConfig, ClassifierChoice};

/// Where to root the inbox + library on disk.
#[derive(Debug, Clone)]
pub struct WiringConfig {
    pub inbox: PathBuf,
    pub library: PathBuf,
}

/// Holds every wired-up service. Cheap to construct; everything is
/// behind `Arc`.
pub struct Wiring {
    ingest: IngestService,
    search: SearchService,
    library: PathBuf,
    health: Vec<String>,
}

impl std::fmt::Debug for Wiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wiring")
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl Wiring {
    /// Build the production wiring.
    ///
    /// The classifier choice follows the user's [`AppConfig`]:
    ///
    /// - [`ClassifierChoice::Auto`] (default) wires the Anthropic adapter
    ///   when a key is present and falls back to the rule-based one
    ///   otherwise. Keeps offline / CI runs free.
    /// - [`ClassifierChoice::Anthropic`] forces the API path and errors
    ///   when no key is configured.
    /// - [`ClassifierChoice::RuleBased`] forces the offline path regardless
    ///   of whether a key is present. Useful for tests and demos.
    ///
    /// The text extractor chain is a
    /// `FallbackExtractor(primary=PdfTextExtractor, fallback=stub OCR)` —
    /// once the M3+ OCR impl lands, only that line changes.
    ///
    /// # Errors
    ///
    /// Fails if `classifier=anthropic` is requested but no API key is
    /// available, or if constructing the HTTP transport fails.
    pub fn build(wiring: &WiringConfig, app_config: &AppConfig) -> Result<Self> {
        let inbox = Arc::new(FsInboxSource::new(wiring.inbox.clone()));
        let store = Arc::new(FsLibraryStore::new(wiring.library.clone()));

        // Reuse a single transport across the classifier and the vision
        // extractor. The trait is stateless; sharing keeps connection
        // pooling and prompt-cache wins amortised across both paths.
        let shared_transport = build_shared_transport(app_config)?;

        let (extractor, extractor_health) = build_extractor(shared_transport.clone(), app_config);

        let (classifier, classifier_health) =
            build_classifier(shared_transport.clone(), app_config)?;

        let clock = Arc::new(SystemClock);
        let ids = Arc::new(UuidV4Generator);

        let ingest = IngestService::new(
            inbox,
            extractor,
            classifier,
            store,
            LibraryPathPolicy::new(),
            clock,
            ids,
        );

        let search_index = Arc::new(GrepSearchIndex::new(wiring.library.clone()));
        let search = SearchService::new(search_index);

        let health = vec![
            "inbox:       FsInboxSource".to_owned(),
            "store:       FsLibraryStore".to_owned(),
            format!("extractor:   {extractor_health}"),
            format!("classifier:  {classifier_health}"),
            "search:      GrepSearchIndex (markdown transcripts)".to_owned(),
            "clock:       SystemClock".to_owned(),
            "ids:         UuidV4Generator".to_owned(),
        ];

        Ok(Self {
            ingest,
            search,
            library: wiring.library.clone(),
            health,
        })
    }

    /// Borrow the ingest service.
    #[must_use]
    pub fn ingest(&self) -> &IngestService {
        &self.ingest
    }

    /// Borrow the search service.
    #[must_use]
    pub fn search(&self) -> &SearchService {
        &self.search
    }

    /// Library root on disk. Surfaced for the MCP server, which reads the
    /// library directly when serving `list_documents` / `get_document`.
    #[must_use]
    pub fn library(&self) -> &PathBuf {
        &self.library
    }

    /// Human-readable lines describing which adapter implements each port.
    /// Surfaced by `paperclaw doctor`.
    #[must_use]
    pub fn health_lines(&self) -> &[String] {
        &self.health
    }

    /// Consume the wiring and produce the MCP service bundle. We can't
    /// keep `Wiring` alive *and* hand its services to the MCP server,
    /// because `IngestService` / `SearchService` are owned and not `Copy`;
    /// taking ownership here avoids cloning the trait objects.
    #[must_use]
    pub fn into_mcp_services(self) -> crate::mcp::McpServices {
        crate::mcp::McpServices {
            ingest: self.ingest,
            search: self.search,
            library: self.library,
        }
    }
}

/// Build the shared HTTP transport once. `None` when no API key is
/// configured — the rule-based classifier and the no-op extractor
/// fallback work fine in that mode.
///
/// We move the exposed secret across the CLI ↔ adapter crate boundary
/// at this single call site; the rest of the binary only sees redacting
/// newtypes (see `SecretString` Debug impls on both sides).
fn build_shared_transport(app_config: &AppConfig) -> Result<Option<Arc<dyn AnthropicTransport>>> {
    let Some(cli_key) = app_config.anthropic_api_key.as_ref() else {
        return Ok(None);
    };
    let adapter_key = AnthropicApiKey::new(cli_key.expose().to_owned());
    let transport =
        ReqwestTransport::new(adapter_key).context("building Anthropic HTTP transport")?;
    let transport: Arc<dyn AnthropicTransport> = Arc::new(transport);
    Ok(Some(transport))
}

/// Compose the extractor chain. When a key is present we layer the vision
/// extractor behind the text-layer one so scanned PDFs and image inbox
/// entries still produce a transcript. Without a key we fall back to a
/// degenerate chain (PDF → PDF) — the second slot is reserved for the
/// future Tesseract OCR adapter.
fn build_extractor(
    transport: Option<Arc<dyn AnthropicTransport>>,
    app_config: &AppConfig,
) -> (Arc<dyn paperclaw_domain::TextExtractor>, String) {
    let primary: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());

    if let Some(t) = transport {
        let vision = AnthropicVisionExtractor::new(
            t,
            AnthropicVisionConfig {
                model: app_config.anthropic_model.clone(),
            },
        );
        let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(vision);
        let extractor: Arc<dyn paperclaw_domain::TextExtractor> =
            Arc::new(FallbackExtractor::new(primary, fallback));
        (
            extractor,
            format!(
                "FallbackExtractor(Pdf -> AnthropicVision({}))",
                app_config.anthropic_model
            ),
        )
    } else {
        // Without a key, the chain has nothing useful to fall back to.
        // The second slot stays a Pdf extractor so the trait surface
        // matches the production shape; M3+ will swap in Tesseract.
        let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
        let extractor: Arc<dyn paperclaw_domain::TextExtractor> =
            Arc::new(FallbackExtractor::new(primary, fallback));
        (
            extractor,
            "FallbackExtractor(Pdf -> Pdf)     [no ANTHROPIC_API_KEY → no vision fallback]"
                .to_owned(),
        )
    }
}

/// Decide which classifier to wire and return a one-line health string
/// describing the choice. Errors when the user forced `anthropic` but no
/// key is available — surfacing that early is better than silently
/// falling back, because the user's intent was explicit.
fn build_classifier(
    transport: Option<Arc<dyn AnthropicTransport>>,
    app_config: &AppConfig,
) -> Result<(Arc<dyn paperclaw_domain::Classifier>, String)> {
    let key_available = transport.is_some();

    let use_anthropic = match app_config.classifier {
        ClassifierChoice::Auto => key_available,
        ClassifierChoice::Anthropic => {
            anyhow::ensure!(
                key_available,
                "PAPERCLAW_CLASSIFIER=anthropic was set but ANTHROPIC_API_KEY is missing. \
                 Add it to .env or unset PAPERCLAW_CLASSIFIER to fall back to rule-based.",
            );
            true
        }
        ClassifierChoice::RuleBased => false,
    };

    if use_anthropic {
        // Unwrap is safe: `use_anthropic` only goes true after the
        // transport is present (the `Anthropic` arm above bails when
        // missing).
        let transport = transport.context("internal: anthropic path selected without transport")?;
        let config = AnthropicClassifierConfig {
            model: app_config.anthropic_model.clone(),
        };
        let model = config.model.clone();
        let classifier: Arc<dyn paperclaw_domain::Classifier> =
            Arc::new(AnthropicClassifier::new(transport, config));
        Ok((classifier, format!("AnthropicClassifier({model})")))
    } else {
        let classifier: Arc<dyn paperclaw_domain::Classifier> =
            Arc::new(RuleBasedClassifier::new());
        let note = if key_available {
            "RuleBasedClassifier             [forced via PAPERCLAW_CLASSIFIER]"
        } else {
            "RuleBasedClassifier             [no ANTHROPIC_API_KEY in env/.env]"
        };
        Ok((classifier, note.to_owned()))
    }
}
