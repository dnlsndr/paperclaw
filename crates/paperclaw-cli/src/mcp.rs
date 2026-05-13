//! Minimal Model Context Protocol stdio server.
//!
//! Speaks newline-delimited JSON-RPC 2.0 over arbitrary `AsyncRead` /
//! `AsyncWrite` streams. In production those are `tokio::io::stdin()` and
//! `tokio::io::stdout()`; in tests the harness pipes
//! [`tokio::io::DuplexStream`]s through `run` so the server can be driven
//! without spawning a subprocess.
//!
//! ## What it exposes
//!
//! Five tools that cover the M3 acceptance bar ("an agent can answer a
//! real question end-to-end"):
//!
//! | Tool                | Job                                              |
//! |---------------------|--------------------------------------------------|
//! | `search_documents`  | Grep transcripts + optional category filter      |
//! | `list_documents`    | Walk the library and return per-doc metadata     |
//! | `get_document`      | Return one document's transcript + sidecar       |
//! | `ingest_inbox`      | Run an ingest pass over the user's inbox folder  |
//! | `ingest_document`   | Accept base64 bytes from the caller and ingest   |
//!
//! `ingest_document` is what lets an upstream LLM hand `PaperClaw` a file
//! directly through the tool call — useful in chat-style sessions where
//! the user has just attached a PDF or photo. The bytes never touch the
//! user's inbox folder; the use-case ingests them straight into the
//! library, sharing the same classifier + extractor chain as the inbox
//! path.
//!
//! ## Why not a third-party SDK?
//!
//! The MCP wire surface is small (initialize, tools/list, tools/call) and
//! we need full control over how `PaperClaw`'s services are threaded into
//! the tool handlers. A hand-rolled JSON-RPC loop is ~200 LOC and skips a
//! third-party dependency.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use paperclaw_app::{IngestService, SearchService};
use paperclaw_domain::types::{IngestEntry, IngestOutcome, MediaType, PendingDocument, SourcePath};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

/// MCP protocol version we negotiate. We don't speak the newer
/// experimental capabilities (resources, prompts) — just `tools`.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Server descriptor surfaced via `initialize`. Reported to the upstream
/// agent so logs and UIs can identify which ``PaperClaw`` they're talking to.
const SERVER_NAME: &str = "paperclaw";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC error codes we return. The standard ones cover everything we
/// need at M3; we don't define MCP-specific subcodes.
const ERR_PARSE: i64 = -32_700;
const ERR_INVALID_REQUEST: i64 = -32_600;
const ERR_METHOD_NOT_FOUND: i64 = -32_601;
const ERR_INVALID_PARAMS: i64 = -32_602;

/// Cap the size of an `ingest_document` upload. Mirrors the vision
/// extractor's own cap so we reject early instead of relaying a 4xx from
/// the API.
const MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;

/// Shared state for every tool handler. Cloning is cheap (everything's
/// behind `Arc`).
#[derive(Clone)]
pub struct McpServices {
    /// Ingest pipeline (wired with the production adapters).
    pub ingest: IngestService,
    /// Search facade (grep over markdown).
    pub search: SearchService,
    /// Library root on disk — `list_documents` / `get_document` read
    /// this directly without going through a port (see module docs).
    pub library: PathBuf,
}

impl std::fmt::Debug for McpServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServices")
            .field("library", &self.library)
            .finish_non_exhaustive()
    }
}

/// JSON-RPC request envelope. We accept missing `params` (some clients
/// omit it on no-arg calls) and treat `id` as optional — its absence
/// marks the message as a notification, which we silently acknowledge.
#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl Response {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Drive the server loop. Reads JSON-RPC messages line-by-line from
/// `reader`, dispatches them, and writes responses to `writer`. Returns
/// when EOF is reached on the read side (the caller closed stdin) or an
/// I/O error occurs.
///
/// # Errors
///
/// Bubbles up I/O errors from reading or writing the underlying streams.
/// Parse / dispatch errors are turned into JSON-RPC `error` responses on
/// the wire and do *not* surface here.
pub async fn run<R, W>(reader: R, mut writer: W, services: Arc<McpServices>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    info!("paperclaw MCP server starting (protocol {PROTOCOL_VERSION})");
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("reading MCP request line")?
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => dispatch(&services, req).await,
            Err(e) => Some(Response::err(
                Value::Null,
                ERR_PARSE,
                format!("invalid JSON-RPC frame: {e}"),
            )),
        };
        if let Some(resp) = response {
            let mut bytes = serde_json::to_vec(&resp).context("encoding MCP response")?;
            bytes.push(b'\n');
            writer
                .write_all(&bytes)
                .await
                .context("writing MCP response")?;
            writer.flush().await.context("flushing MCP response")?;
        }
    }
    debug!("MCP stdin closed; server exiting cleanly");
    Ok(())
}

/// Dispatch a single request. Returns `None` for notifications (no `id`)
/// since the JSON-RPC spec says we must not respond to them.
async fn dispatch(services: &McpServices, req: Request) -> Option<Response> {
    let is_notification = req.id.is_none();
    let id = req.id.clone().unwrap_or(Value::Null);

    if req.jsonrpc != "2.0" && !req.jsonrpc.is_empty() {
        // Some clients omit jsonrpc; we accept both. Anything else is
        // wrong.
        if is_notification {
            return None;
        }
        return Some(Response::err(
            id,
            ERR_INVALID_REQUEST,
            format!("unsupported jsonrpc version: {}", req.jsonrpc),
        ));
    }

    let result = match req.method.as_str() {
        "initialize" => Ok(handle_initialize()),
        "notifications/initialized" => {
            return None;
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(services, &req.params).await,
        // Tools-only server. Resources / prompts subsystems exist in MCP
        // but we don't implement them at M3 — return method-not-found so
        // a polite client moves on.
        other => Err(DispatchError {
            code: ERR_METHOD_NOT_FOUND,
            message: format!("method not supported: {other}"),
        }),
    };

    if is_notification {
        return None;
    }

    match result {
        Ok(value) => Some(Response::ok(id, value)),
        Err(DispatchError { code, message }) => Some(Response::err(id, code, message)),
    }
}

#[derive(Debug)]
struct DispatchError {
    code: i64,
    message: String,
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
        "instructions":
            "`PaperClaw` exposes a small library of classified personal paperwork. \
             Use `search_documents` to find documents by keyword, `list_documents` \
             to enumerate a category, `get_document` for the full transcript, \
             `ingest_inbox` to process the user's inbox folder, and `ingest_document` \
             to hand the server a single file directly (base64-encoded).",
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "search_documents",
                "description":
                    "Full-text grep over the markdown transcripts in the library. \
                     Returns ranked hits with a snippet around the match. Optional \
                     `category` narrows the search to one folder (e.g. \"tax\", \
                     \"bill\").",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string", "description": "Search string." },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
                        "category": {
                            "type": "string",
                            "description":
                                "Optional category filter, e.g. \"tax\". \
                                 Matches the folder name under library/.",
                        }
                    }
                }
            },
            {
                "name": "list_documents",
                "description":
                    "Enumerate documents in the library, newest sidecars first. \
                     Returns metadata (id, category, sender, classifier_version) \
                     but not the full transcript.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "category": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
                    }
                }
            },
            {
                "name": "get_document",
                "description":
                    "Return the full transcript and sidecar metadata for a single \
                     document, identified by its category folder and file stem.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["category", "stem"],
                    "properties": {
                        "category": { "type": "string" },
                        "stem": { "type": "string" }
                    }
                }
            },
            {
                "name": "ingest_inbox",
                "description":
                    "Process every PDF / image currently in the user's inbox folder. \
                     Returns a summary of filed / skipped / failed counts.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }
            },
            {
                "name": "ingest_document",
                "description":
                    "Ingest a single document handed in by the calling agent. The \
                     bytes are passed inline as base64 — useful when the upstream \
                     LLM has already received a PDF or image from the user and \
                     wants `PaperClaw` to file it without first writing it to disk. \
                     Supported media types: application/pdf, image/jpeg, image/png, \
                     image/webp (auto-detected from magic bytes).",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["filename", "content_base64"],
                    "properties": {
                        "filename": {
                            "type": "string",
                            "description":
                                "Original filename, preserved verbatim in the sidecar.",
                        },
                        "content_base64": {
                            "type": "string",
                            "description":
                                "Document bytes, base64-encoded (RFC 4648 standard \
                                 alphabet, padded). Max 5 MiB raw.",
                        }
                    }
                }
            }
        ]
    })
}

async fn handle_tools_call(services: &McpServices, params: &Value) -> Result<Value, DispatchError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| DispatchError {
            code: ERR_INVALID_PARAMS,
            message: "tools/call requires `name`".to_owned(),
        })?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    debug!(tool = name, "MCP tool call");

    let result = match name {
        "search_documents" => tool_search_documents(services, &arguments).await,
        "list_documents" => tool_list_documents(services, &arguments).await,
        "get_document" => tool_get_document(services, &arguments).await,
        "ingest_inbox" => tool_ingest_inbox(services).await,
        "ingest_document" => tool_ingest_document(services, &arguments).await,
        other => {
            return Err(DispatchError {
                code: ERR_METHOD_NOT_FOUND,
                message: format!("unknown tool: {other}"),
            });
        }
    };

    Ok(match result {
        Ok(value) => json!({
            "content": [
                { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }
            ],
            "isError": false,
            "structuredContent": value,
        }),
        Err(message) => {
            warn!(tool = name, %message, "MCP tool returned an error");
            json!({
                "content": [
                    { "type": "text", "text": message }
                ],
                "isError": true,
            })
        }
    })
}

async fn tool_search_documents(services: &McpServices, args: &Value) -> Result<Value, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `query`".to_owned())?;
    let limit = clamp_to_usize(args.get("limit").and_then(Value::as_u64).unwrap_or(10));
    let category_filter = args
        .get("category")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Ask for extra results so a category filter still has a reasonable
    // pool to slice from. Bounded so a runaway limit doesn't explode the
    // intermediate Vec.
    let probe_limit = limit.saturating_mul(4).min(200).max(limit);
    let hits = services
        .search
        .query(query, probe_limit)
        .await
        .map_err(|e| format!("search failed: {e}"))?;

    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        if let Some(ref filter) = category_filter
            && &hit.library_path.category != filter
        {
            continue;
        }
        out.push(json!({
            "document_id": hit.document_id.to_string(),
            "category": hit.library_path.category,
            "stem": hit.library_path.stem,
            "snippet": hit.snippet,
            "score": hit.score,
        }));
        if out.len() >= limit {
            break;
        }
    }
    Ok(json!({
        "query": query,
        "count": out.len(),
        "hits": out,
    }))
}

async fn tool_list_documents(services: &McpServices, args: &Value) -> Result<Value, String> {
    let category_filter = args.get("category").and_then(Value::as_str);
    let limit = clamp_to_usize(args.get("limit").and_then(Value::as_u64).unwrap_or(50));

    let mut entries = Vec::new();
    let library = &services.library;
    if !fs::try_exists(library)
        .await
        .map_err(|e| format!("library missing: {e}"))?
    {
        return Ok(json!({ "count": 0, "documents": [] }));
    }

    let mut category_dirs = fs::read_dir(library)
        .await
        .map_err(|e| format!("read_dir failed: {e}"))?;
    while let Some(cat) = category_dirs
        .next_entry()
        .await
        .map_err(|e| format!("read_dir failed: {e}"))?
    {
        let cat_name = cat.file_name().to_string_lossy().into_owned();
        if cat_name.starts_with('_') && cat_name != "_unsorted" {
            continue;
        }
        if let Some(filter) = category_filter
            && cat_name != filter
        {
            continue;
        }
        let Ok(meta) = fs::metadata(cat.path()).await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }

        let mut docs = fs::read_dir(cat.path())
            .await
            .map_err(|e| format!("read_dir failed: {e}"))?;
        while let Some(doc) = docs
            .next_entry()
            .await
            .map_err(|e| format!("read_dir failed: {e}"))?
        {
            let path = doc.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !file_name.ends_with(".paperclaw.json") {
                continue;
            }
            let stem = file_name.trim_end_matches(".paperclaw.json").to_owned();
            let Ok(raw) = fs::read(&path).await else {
                continue;
            };
            let Ok(sidecar) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            entries.push(json!({
                "document_id": sidecar.get("id").cloned().unwrap_or(Value::Null),
                "category": cat_name.clone(),
                "stem": stem,
                "ingested_at": sidecar.get("ingested_at").cloned().unwrap_or(Value::Null),
                "original_filename": sidecar.get("original_filename").cloned().unwrap_or(Value::Null),
                "classifier_version": sidecar.get("classifier_version").cloned().unwrap_or(Value::Null),
                "classification": sidecar.get("classification").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    // Newest ingest first so an agent asking "list my latest tax letters"
    // gets a useful answer without paging.
    entries.sort_by(|a, b| {
        b.get("ingested_at")
            .and_then(Value::as_str)
            .cmp(&a.get("ingested_at").and_then(Value::as_str))
    });
    entries.truncate(limit);
    Ok(json!({ "count": entries.len(), "documents": entries }))
}

async fn tool_get_document(services: &McpServices, args: &Value) -> Result<Value, String> {
    let category = args
        .get("category")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `category`".to_owned())?;
    let stem = args
        .get("stem")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `stem`".to_owned())?;

    // Defense in depth: refuse any path component that would escape the
    // library root. Stem and category come from the caller, so a `../`
    // tucked inside either would otherwise reach an arbitrary file.
    if has_path_separator(category) || has_path_separator(stem) {
        return Err("category and stem must be plain filename components (no slashes)".to_owned());
    }

    let dir = services.library.join(category);
    let md_path = dir.join(format!("{stem}.md"));
    let meta_path = dir.join(format!("{stem}.paperclaw.json"));

    let md = fs::read_to_string(&md_path)
        .await
        .map_err(|e| format!("transcript not found: {e}"))?;
    let meta_raw = fs::read(&meta_path)
        .await
        .map_err(|e| format!("sidecar not found: {e}"))?;
    let meta: Value =
        serde_json::from_slice(&meta_raw).map_err(|e| format!("sidecar parse: {e}"))?;

    Ok(json!({
        "category": category,
        "stem": stem,
        "transcript": md,
        "sidecar": meta,
    }))
}

async fn tool_ingest_inbox(services: &McpServices) -> Result<Value, String> {
    let report = services
        .ingest
        .ingest_all()
        .await
        .map_err(|e| format!("ingest failed: {e}"))?;
    Ok(render_report(&report.entries))
}

async fn tool_ingest_document(services: &McpServices, args: &Value) -> Result<Value, String> {
    let filename = args
        .get("filename")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `filename`".to_owned())?;
    let payload = args
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `content_base64`".to_owned())?;

    if has_path_separator(filename) {
        return Err("filename must be a plain name, no slashes".to_owned());
    }

    let bytes = BASE64
        .decode(payload.as_bytes())
        .map_err(|e| format!("content_base64 is not valid base64: {e}"))?;
    if bytes.is_empty() {
        return Err("content_base64 decoded to zero bytes".to_owned());
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "upload is {} bytes, max {} bytes",
            bytes.len(),
            MAX_UPLOAD_BYTES,
        ));
    }
    let Some(media_type) = MediaType::sniff(&bytes) else {
        return Err(
            "could not detect a supported media type (pdf / jpeg / png / webp) from magic bytes"
                .to_owned(),
        );
    };

    // Synthesise a source path that mirrors where the file *would* have
    // lived if it had been dropped in the inbox. The use-case never tries
    // to consume this path (we use `ingest_pending`), so there's no risk
    // of touching the real filesystem.
    let synthetic_source = SourcePath::new(PathBuf::from(format!("mcp://upload/{filename}")));
    let pending = PendingDocument {
        source: synthetic_source,
        bytes,
        media_type,
    };

    let entry = services.ingest.ingest_pending(pending).await;
    Ok(render_entry(filename, media_type, &entry))
}

fn render_report(entries: &[IngestEntry]) -> Value {
    let mut filed_total = 0u32;
    let mut encrypted = 0u32;
    let mut low_conf = 0u32;
    let mut errored = 0u32;
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let (variant, detail) = describe_outcome(&entry.outcome);
        match entry.outcome {
            IngestOutcome::Filed { .. } => filed_total += 1,
            IngestOutcome::SkippedEncrypted { .. } => encrypted += 1,
            IngestOutcome::SkippedLowConfidence { .. } => low_conf += 1,
            IngestOutcome::Failed { .. } => errored += 1,
        }
        rows.push(json!({
            "source": entry.source.as_path().display().to_string(),
            "outcome": variant,
            "detail": detail,
        }));
    }
    json!({
        "processed": entries.len(),
        "filed": filed_total,
        "encrypted": encrypted,
        "low_confidence": low_conf,
        "failed": errored,
        "entries": rows,
    })
}

fn render_entry(filename: &str, media: MediaType, entry: &IngestEntry) -> Value {
    let (variant, detail) = describe_outcome(&entry.outcome);
    json!({
        "filename": filename,
        "media_type": media.http_media_type(),
        "outcome": variant,
        "detail": detail,
    })
}

fn describe_outcome(outcome: &IngestOutcome) -> (&'static str, Value) {
    match outcome {
        IngestOutcome::Filed { document } => (
            "filed",
            json!({
                "id": document.id.to_string(),
                "category": document.library_path.category,
                "stem": document.library_path.stem,
                "kind": document.classification.kind.folder_slug(),
                "confidence": document.classification.confidence.value(),
                "sender": document.classification.sender,
                "subject": document.classification.subject,
            }),
        ),
        IngestOutcome::SkippedEncrypted { hint } => ("skipped_encrypted", json!({ "hint": hint })),
        IngestOutcome::SkippedLowConfidence { classification } => (
            "skipped_low_confidence",
            json!({
                "kind": classification.kind.folder_slug(),
                "confidence": classification.confidence.value(),
            }),
        ),
        IngestOutcome::Failed { reason } => ("failed", json!({ "reason": reason })),
    }
}

/// Reject any user-supplied path component that contains a separator —
/// stem / category / filename are meant to be plain names, never paths.
fn has_path_separator(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.contains("..")
}

/// Clamp a JSON-supplied `u64` down to `usize`. The MCP schema caps
/// `limit` at 200, but a misbehaving client could send an absurd value;
/// saturating is safer than panicking.
fn clamp_to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn path_separator_guard_catches_traversal() {
        assert!(has_path_separator("../etc/passwd"));
        assert!(has_path_separator("foo/bar"));
        assert!(has_path_separator(r"foo\bar"));
        assert!(!has_path_separator("2026-05-13_finanzamt_steuer"));
    }

    #[test]
    fn render_report_counts_outcomes_correctly() {
        let entries = vec![
            IngestEntry {
                source: SourcePath::new("a.pdf"),
                outcome: IngestOutcome::SkippedEncrypted {
                    hint: "test".into(),
                },
            },
            IngestEntry {
                source: SourcePath::new("b.pdf"),
                outcome: IngestOutcome::Failed {
                    reason: "boom".into(),
                },
            },
        ];
        let value = render_report(&entries);
        assert_eq!(value["processed"], 2);
        assert_eq!(value["encrypted"], 1);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["filed"], 0);
    }
}
