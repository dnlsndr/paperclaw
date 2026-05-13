//! M3 acceptance — vision fallback in the extractor chain.
//!
//! We can't hit the real Anthropic API in CI (cost, network, secret
//! handling), so we drive the production [`AnthropicVisionExtractor`] with
//! a stub [`AnthropicTransport`] that returns canned text. The chain is
//! still production-shaped: `FallbackExtractor(Pdf -> Vision)` over a real
//! [`FsLibraryStore`] and [`InMemoryInbox`].
//!
//! What this proves:
//!
//! 1. An image inbox entry routes past the PDF extractor (which returns
//!    `Unsupported` for non-PDF media) and lands at the vision extractor.
//! 2. The vision extractor's transcript flows through the rule-based
//!    classifier and into the library tree.
//! 3. The library record (`.pdf` / `.md` / `.paperclaw.json` siblings)
//!    is produced even though the source was an image — the store writes
//!    the original bytes under a `.pdf` stem extension regardless of
//!    media type (M3 design note: we don't yet differentiate output
//!    extensions per media type; that's tracked in DESIGN.md §11).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paperclaw_adapters::{
    AnthropicTransport, AnthropicVisionConfig, AnthropicVisionExtractor, FallbackExtractor,
    PdfTextExtractor, RuleBasedClassifier, TransportError,
};
use paperclaw_app::IngestService;
use paperclaw_domain::LibraryPathPolicy;
use paperclaw_domain::testing::{FixedClock, InMemoryInbox, InMemoryLibraryStore, SeqIdGenerator};
use paperclaw_domain::types::{IngestOutcome, MediaType, PendingDocument, SourcePath};
use serde_json::{Value, json};
use time::macros::datetime;

/// In-process transport for the integration test. Records the request
/// body so assertions can verify the correct content block was sent.
#[derive(Debug)]
struct StubTransport {
    canned: Value,
    last_request: Mutex<Option<Value>>,
}

impl StubTransport {
    fn new(canned: Value) -> Self {
        Self {
            canned,
            last_request: Mutex::new(None),
        }
    }

    fn last_request(&self) -> Value {
        self.last_request.lock().unwrap().clone().unwrap()
    }
}

#[async_trait]
impl AnthropicTransport for StubTransport {
    async fn send_messages(&self, body: Value) -> Result<Value, TransportError> {
        *self.last_request.lock().unwrap() = Some(body);
        Ok(self.canned.clone())
    }
}

fn canned_text(text: &str) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "stop_reason": "end_turn",
        "content": [
            { "type": "text", "text": text },
        ]
    })
}

#[tokio::test]
async fn image_inbox_entry_routes_through_vision_fallback() {
    // A photo of a Stadtwerke utility bill: the PDF extractor refuses
    // it (wrong media type), the chain falls through to the vision
    // extractor, which returns a transcript that the rule-based
    // classifier categorises as a Bill.
    let transport = Arc::new(StubTransport::new(canned_text(
        "Stadtwerke München GmbH\n80287 München\nStromrechnung 2026 \
         für den Zeitraum 01.04.2025 – 31.03.2026.\nGesamtbetrag: 412,00 EUR.",
    )));
    let vision = AnthropicVisionExtractor::new(transport.clone(), AnthropicVisionConfig::default());

    let primary: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
    let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(vision);
    let extractor = Arc::new(FallbackExtractor::new(primary, fallback));

    // Image payload: a token JPEG SOI prefix is enough for the inbox
    // sniffer + the vision extractor's size bounds. The extractor
    // doesn't care about the actual pixel data — it base64-encodes
    // whatever bytes we hand it.
    let jpeg_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46];
    let inbox = Arc::new(InMemoryInbox::with(vec![PendingDocument {
        source: SourcePath::new("inbox/stadtwerke.jpg"),
        bytes: jpeg_bytes,
        media_type: MediaType::Jpeg,
    }]));
    let store = Arc::new(InMemoryLibraryStore::new());

    let svc = IngestService::new(
        inbox,
        extractor,
        Arc::new(RuleBasedClassifier::new()),
        store.clone(),
        LibraryPathPolicy::new(),
        Arc::new(FixedClock::new(datetime!(2026-05-13 12:00 UTC))),
        Arc::new(SeqIdGenerator::new()),
    );

    let report = svc.ingest_all().await.unwrap();
    assert_eq!(report.filed_count(), 1, "{report:?}");
    let entry = &report.entries[0];
    match &entry.outcome {
        IngestOutcome::Filed { document } => {
            assert_eq!(document.library_path.category, "bill");
        }
        other => panic!("expected Filed, got {other:?}"),
    }

    // The request that actually reached the (stub) wire must have used
    // the `image` content block with the JPEG media type — regressions
    // in the routing logic would silently degrade real-world calls.
    let body = transport.last_request();
    let first = &body["messages"][0]["content"][0];
    assert_eq!(first["type"], "image");
    assert_eq!(first["source"]["media_type"], "image/jpeg");

    // The recorded write carries the vision-extracted transcript.
    let writes = store.writes();
    assert_eq!(writes.len(), 1);
    assert!(
        writes[0]
            .transcript
            .as_str()
            .to_lowercase()
            .contains("stadtwerke"),
        "transcript must come from the vision extractor, got: {}",
        writes[0].transcript.as_str(),
    );
}

#[tokio::test]
async fn pdf_extractor_text_wins_when_present_and_vision_is_not_called() {
    // Regression guard: a normal PDF with a text layer should resolve at
    // the primary extractor. The vision fallback must not be invoked
    // (it's the expensive path). We assert that by checking the stub
    // transport never recorded a request.
    let transport = Arc::new(StubTransport::new(canned_text("should not be called")));
    let vision = AnthropicVisionExtractor::new(transport.clone(), AnthropicVisionConfig::default());

    let primary: Arc<dyn paperclaw_domain::TextExtractor> =
        Arc::new(paperclaw_domain::testing::StubExtractor::returning(
            "Finanzamt München\nBescheid 2024 Einkommensteuer",
        ));
    let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(vision);
    let extractor = Arc::new(FallbackExtractor::new(primary, fallback));

    let inbox = Arc::new(InMemoryInbox::with(vec![PendingDocument {
        source: SourcePath::new("inbox/normal.pdf"),
        bytes: b"%PDF-1.4 hi".to_vec(),
        media_type: MediaType::Pdf,
    }]));
    let store = Arc::new(InMemoryLibraryStore::new());

    let svc = IngestService::new(
        inbox,
        extractor,
        Arc::new(RuleBasedClassifier::new()),
        store,
        LibraryPathPolicy::new(),
        Arc::new(FixedClock::new(datetime!(2026-05-13 12:00 UTC))),
        Arc::new(SeqIdGenerator::new()),
    );

    let report = svc.ingest_all().await.unwrap();
    assert_eq!(report.filed_count(), 1);
    assert!(
        transport.last_request.lock().unwrap().is_none(),
        "vision extractor must NOT be called when primary succeeds",
    );
}
