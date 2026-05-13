//! Composition root. Builds adapter instances and hands them to the
//! application services. Swap implementations here — nowhere else.

use std::path::PathBuf;
use std::sync::Arc;

use paperclaw_adapters::{
    FallbackExtractor, FsInboxSource, FsLibraryStore, PdfTextExtractor, RuleBasedClassifier,
    StubSearchIndex, SystemClock, UuidV4Generator,
};
use paperclaw_app::{IngestService, SearchService};
use paperclaw_domain::LibraryPathPolicy;

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
    /// Build the production wiring. The text extractor chain is a
    /// `FallbackExtractor(primary=PdfTextExtractor, fallback=stub OCR)` —
    /// once the M2/M3 impls land, only this function changes.
    #[must_use]
    pub fn build(config: &WiringConfig) -> Self {
        let inbox = Arc::new(FsInboxSource::new(config.inbox.clone()));
        let store = Arc::new(FsLibraryStore::new(config.library.clone()));

        let primary: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
        let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new()); // M3+ swaps in TesseractExtractor.
        let extractor = Arc::new(FallbackExtractor::new(primary, fallback));

        let classifier = Arc::new(RuleBasedClassifier::new());

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

        let search = SearchService::new(Arc::new(StubSearchIndex));

        let health = vec![
            "inbox:       FsInboxSource".to_owned(),
            "store:       FsLibraryStore".to_owned(),
            "extractor:   FallbackExtractor(Pdf -> Pdf)  [M2: real PDF, M3+: OCR]".to_owned(),
            "classifier:  RuleBasedClassifier            [M3: AnthropicClassifier]".to_owned(),
            "search:      StubSearchIndex               [M3]".to_owned(),
            "clock:       SystemClock".to_owned(),
            "ids:         UuidV4Generator".to_owned(),
        ];

        Self {
            ingest,
            search,
            health,
        }
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

    /// Human-readable lines describing which adapter implements each port.
    /// Surfaced by `paperclaw doctor`.
    #[must_use]
    pub fn health_lines(&self) -> &[String] {
        &self.health
    }
}
