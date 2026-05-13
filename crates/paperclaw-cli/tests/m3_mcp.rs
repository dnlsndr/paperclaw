//! M3 acceptance — MCP stdio server roundtrip.
//!
//! Drives `paperclaw_cli::run_mcp` via in-memory duplex streams so the test
//! never spawns a subprocess. Asserts the standard initialize / tools/list /
//! tools/call handshake and that the high-value tools (`search_documents`,
//! `ingest_document`) work end-to-end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use paperclaw_adapters::{
    FallbackExtractor, FsInboxSource, FsLibraryStore, GrepSearchIndex, PdfTextExtractor,
    RuleBasedClassifier,
};
use paperclaw_app::{IngestService, SearchService};
use paperclaw_cli::mcp::McpServices;
use paperclaw_domain::LibraryPathPolicy;
use paperclaw_domain::testing::{FixedClock, SeqIdGenerator};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::macros::datetime;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn asset_path(name: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("assets")
        .join(name)
}

async fn stage_inbox(inbox: &Path, asset: &str) {
    let src = asset_path(asset);
    fs::copy(&src, inbox.join(asset)).await.unwrap();
}

fn build_ingest(inbox: &Path, library: &Path) -> IngestService {
    let inbox = Arc::new(FsInboxSource::new(inbox));
    let store = Arc::new(FsLibraryStore::new(library));
    let primary: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
    let fallback: Arc<dyn paperclaw_domain::TextExtractor> = Arc::new(PdfTextExtractor::new());
    let extractor = Arc::new(FallbackExtractor::new(primary, fallback));
    let classifier = Arc::new(RuleBasedClassifier::new());
    let clock = Arc::new(FixedClock::new(datetime!(2026-05-13 12:00 UTC)));
    let ids = Arc::new(SeqIdGenerator::new());
    IngestService::new(
        inbox,
        extractor,
        classifier,
        store,
        LibraryPathPolicy::new(),
        clock,
        ids,
    )
}

/// Stand up the MCP service bundle backed by a real library. Pre-ingests
/// the sample PDFs so search has something to find.
async fn seed_services() -> (TempDir, TempDir, Arc<McpServices>) {
    let inbox = TempDir::new().unwrap();
    let library = TempDir::new().unwrap();
    stage_inbox(inbox.path(), "stadtwerke-stromrechnung.pdf").await;
    stage_inbox(inbox.path(), "finanzamt-bescheid.pdf").await;

    let ingest = build_ingest(inbox.path(), library.path());
    ingest.ingest_all().await.unwrap();

    // Rebuild ingest pointed at an empty inbox tempdir so `ingest_inbox`
    // and `ingest_document` calls from the MCP layer don't accidentally
    // re-process the seed PDFs.
    let empty_inbox = TempDir::new().unwrap();
    // Keep `empty_inbox` alive by leaking it into the services bundle via
    // the returned tuple. We don't return it; let it drop after the test.
    drop(empty_inbox);
    let fresh_inbox = TempDir::new().unwrap();
    let ingest_fresh = build_ingest(fresh_inbox.path(), library.path());

    let search = SearchService::new(Arc::new(GrepSearchIndex::new(library.path())));

    let services = Arc::new(McpServices {
        ingest: ingest_fresh,
        search,
        library: library.path().to_path_buf(),
    });

    (fresh_inbox, library, services)
}

/// Drive the MCP server through a `tokio::io::duplex` pair. The server
/// reads from `server_in` (we write into `client_out`) and writes to
/// `server_out` (we read from `client_in`). Returns the handles we
/// control plus the `JoinHandle` for the spawned server task.
struct McpHarness {
    client_writer: tokio::io::DuplexStream,
    client_reader: BufReader<tokio::io::DuplexStream>,
    _server: tokio::task::JoinHandle<()>,
}

impl McpHarness {
    fn spawn(services: Arc<McpServices>) -> Self {
        let (client_writer, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, client_reader_raw) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            paperclaw_cli::mcp::run(server_in, server_out, services)
                .await
                .unwrap();
        });
        Self {
            client_writer,
            client_reader: BufReader::new(client_reader_raw),
            _server: server,
        }
    }

    async fn send(&mut self, payload: &Value) {
        let mut line = serde_json::to_vec(payload).unwrap();
        line.push(b'\n');
        self.client_writer.write_all(&line).await.unwrap();
        self.client_writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut buf = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            self.client_reader.read_line(&mut buf),
        )
        .await
        .expect("MCP server reply timed out")
        .unwrap();
        assert!(read > 0, "MCP server closed stdout unexpectedly");
        serde_json::from_str(&buf).unwrap_or_else(|e| panic!("invalid JSON line: {e}\n{buf}"))
    }

    async fn shutdown(mut self) {
        // Drop the write half so the server sees EOF and exits cleanly.
        self.client_writer.shutdown().await.ok();
    }
}

#[tokio::test]
async fn initialize_handshake_reports_tool_capability() {
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    h.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-03-26" }
    }))
    .await;
    let resp = h.recv().await;
    assert_eq!(resp["id"], 1);
    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], "2025-03-26");
    assert!(
        result["capabilities"]["tools"].is_object(),
        "initialize must declare the `tools` capability, got: {result}",
    );
    assert_eq!(result["serverInfo"]["name"], "paperclaw");

    h.shutdown().await;
}

#[tokio::test]
async fn tools_list_exposes_every_m3_tool() {
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    h.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .await;
    let resp = h.recv().await;
    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "search_documents",
        "list_documents",
        "get_document",
        "ingest_inbox",
        "ingest_document",
    ] {
        assert!(
            names.contains(&expected),
            "tools/list must expose `{expected}`, got: {names:?}",
        );
    }

    h.shutdown().await;
}

#[tokio::test]
async fn search_documents_returns_hits_against_the_seeded_library() {
    // This is the canonical M3 acceptance flow: an agent asks for a doc
    // by keyword and gets back structured hits with a usable snippet.
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    h.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "search_documents",
            "arguments": { "query": "stadtwerke", "limit": 5 }
        }
    }))
    .await;
    let resp = h.recv().await;
    assert_eq!(resp["error"], Value::Null);
    let structured = &resp["result"]["structuredContent"];
    assert!(structured["count"].as_u64().unwrap() >= 1);
    let hits = structured["hits"].as_array().unwrap();
    assert_eq!(hits[0]["category"], "bill");
    let snippet = hits[0]["snippet"].as_str().unwrap();
    assert!(snippet.to_lowercase().contains("stadtwerke"));

    h.shutdown().await;
}

#[tokio::test]
async fn ingest_document_accepts_base64_payload_from_the_caller() {
    // Demonstrates the "LLM hands us bytes" flow: a Claude session reads
    // a PDF, the user wants it filed, and the agent calls our tool with
    // the bytes inline.
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    let pdf_bytes = fs::read(asset_path("stadtwerke-stromrechnung.pdf"))
        .await
        .unwrap();
    let b64 = BASE64.encode(&pdf_bytes);

    h.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "ingest_document",
            "arguments": {
                "filename": "uploaded-strom.pdf",
                "content_base64": b64,
            }
        }
    }))
    .await;
    let resp = h.recv().await;
    assert_eq!(resp["error"], Value::Null);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["outcome"], "filed");
    assert_eq!(structured["media_type"], "application/pdf");
    assert_eq!(structured["detail"]["category"], "bill");

    h.shutdown().await;
}

#[tokio::test]
async fn ingest_document_rejects_unknown_media_types() {
    // Random bytes don't match any of our magic-byte sniffs. The tool
    // must surface this as `isError: true`, not a JSON-RPC error frame —
    // an agent should be able to retry with a different file.
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    let b64 = BASE64.encode(b"this is just plain text, no magic prefix");
    h.send(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "ingest_document",
            "arguments": {
                "filename": "garbage.txt",
                "content_base64": b64,
            }
        }
    }))
    .await;
    let resp = h.recv().await;
    assert_eq!(resp["error"], Value::Null);
    assert_eq!(resp["result"]["isError"], true);

    h.shutdown().await;
}

#[tokio::test]
async fn unknown_method_returns_jsonrpc_error() {
    let (_inbox, _library, services) = seed_services().await;
    let mut h = McpHarness::spawn(services);

    h.send(&json!({ "jsonrpc": "2.0", "id": 6, "method": "no/such/method" }))
        .await;
    let resp = h.recv().await;
    assert_eq!(resp["error"]["code"], -32_601);
    assert!(resp["result"].is_null());

    h.shutdown().await;
}
