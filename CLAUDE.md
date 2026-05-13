# PaperClaw — agent operating contract

This file is the contract between the human and any agent working in this
repo. Keep it short and current. Edit it whenever a rule changes.

## Project shape

PaperClaw is a Rust (edition 2024, tokio async) workspace with four
crates following ports-and-adapters / hexagonal architecture:
`paperclaw-domain` (pure types + trait ports), `paperclaw-app`
(use-cases), `paperclaw-adapters` (concrete fs / pdf / llm impls),
`paperclaw-cli` (binary + composition root + MCP stdio host).

Dependencies flow inward: `cli → app → domain`, `adapters → domain`,
`cli → adapters` (only for wiring). **Never** add a reverse edge.

`docs/DESIGN.md` is the source of truth for architecture decisions.
If you change one, update the doc in the same commit.

## Where things go

| Change                                  | Crate                  |
|-----------------------------------------|------------------------|
| New domain type, error, or trait port   | `paperclaw-domain`     |
| New use-case / cross-port orchestration | `paperclaw-app`        |
| New trait impl (fs, pdf, llm, …)        | `paperclaw-adapters`   |
| New CLI command or MCP tool             | `paperclaw-cli`        |

## Feedback loop

Default loop:

```bash
just check
```

That runs `cargo fmt --all -- --check`, `cargo clippy ... -D warnings`,
and the test suite. **Run it before declaring any change done.** If it
fails, fix the failure — never `--no-verify` your way past the hook.

Quick loop (matches pre-commit):

```bash
just check-quick
```

`just --list` enumerates the rest.

## Testing rules

- Unit tests live in the same crate as the code under test.
- Use the in-memory fakes from `paperclaw_domain::testing` (feature
  `testing`) instead of `mockall` or hand-rolled doubles.
- Inject `FixedClock` and `SeqIdGenerator` so use-case tests are
  deterministic.

## Idioms

- All trait ports are `Send + Sync`. Use `#[async_trait]` so they stay
  dyn-compatible behind `Arc<dyn ...>`.
- Errors: `thiserror` in libraries, `anyhow` in the CLI binary.
- Unfinished work surfaces as an explicit `*Error::NotImplemented`
  variant — never `todo!()` — so the type system tracks it.

## Encrypted PDFs

Encrypted PDFs are skipped by design. The adapter returns
`ExtractionError::Encrypted { hint }` and the ingest use-case records
`IngestOutcome::SkippedEncrypted`. Do not invent a separate preflight
port — the error variant is the contract.

## Inbox lifecycle

The inbox is the *source*; the library is the *truth*. Once a document
is written to the library, the inbox copy is removed via
`InboxSource::consume`. The use-case only consumes on `Filed` and
`SkippedLowConfidence` outcomes — `SkippedEncrypted` and `Failed` stay
in the inbox so the user can decrypt / fix and retry. Adapters must
refuse to follow symlinks in both `pending` and `consume`.

`FsInboxSource::pending` also sniffs the `%PDF-` magic prefix before
yielding entries — the `.pdf` extension is treated as a hint. Adapters
must use `symlink_metadata` (not `metadata`) in both `pending` and
`consume` so symlinks are skipped rather than transparently followed.

## Concurrency + panic policy

`IngestService::ingest_all` fans out **one tokio task per pending
document**. Adapters must be re-entrant; `FsLibraryStore` serializes
its commit critical section internally via a `tokio::sync::Mutex`.

**Panics inside ingest tasks abort the batch** (`resume_unwind`) — never
catch them to fabricate an `IngestOutcome::Failed`. If you wrap a parser
that can crash on adversarial input, isolate it inside
`tokio::task::spawn_blocking` and translate the resulting `JoinError` to
`ExtractionError::Other`.

## Sidecar schema

`MetadataSidecar` is versioned by a monotonically-incrementing
`schema_version` field. Readers must reject unknown versions rather
than guess. Every schema bump ships with a `paperclaw migrate` CLI
subcommand that upgrades existing sidecars in place. M1 sidecars carry
`schema_version: 1` and include: `id`, `ingested_at`,
`original_filename` (verbatim), `classifier_version`, `classification`,
`transcript_bytes`, `pdf_bytes`. Content hash (`pdf_sha256`) is deferred
— do not assume it on the sidecar yet.

When adding a new classifier impl, set `Classifier::version()` to a
stable string (`"rule-based:2"`, `"anthropic:claude-haiku-4-5"`). Bump
it whenever the rule set or model meaningfully changes — that's how the
re-classification flow tells stale entries apart.

## Prompt-injection defense

The inbox is *untrusted*. Every classifier must feed its transcript
through `paperclaw_domain::sanitize::redact` before reading it; the raw
transcript still lands on disk (the `.md` sidecar is the audit log) but
the classifier only sees the redacted view. The LLM-backed
`AnthropicClassifier` layers two more defenses on top: a system prompt
that declares document content untrusted, and a forced single-tool
response (`record_classification`) so the model can never emit
free-form output. If you add a new classifier impl, route its input
through the same sanitizer.

## Config + secrets

The CLI loads `.env` at startup via `dotenvy` and reads typed config via
the `config` crate. Recognised env vars:

- `ANTHROPIC_API_KEY` — opt into the LLM classifier. Wrapped in a
  `SecretString` with a redacting `Debug` impl. **Never log this value.**
  Expose it only at the single call site that builds the HTTP transport.
- `PAPERCLAW_CLASSIFIER` — `auto` (default; LLM when key present),
  `anthropic` (force LLM; error if no key), `rule-based` (force offline).
- `PAPERCLAW_ANTHROPIC_MODEL` — override the model ID (default
  `claude-haiku-4-5`).

## Media types

`MediaType` (in `paperclaw-domain`) is the format tag the inbox attaches
to every `PendingDocument` after sniffing magic bytes. M3 supports
`Pdf` / `Jpeg` / `Png` / `Webp`. `FsInboxSource` gates inbox entries
through both an extension whitelist and `MediaType::sniff` — a renamed
`notes.txt` with a `.pdf` extension still gets rejected at intake.

`TextExtractor::extract` takes a `SourceMedia { bytes, media_type }` so
adapters can branch on the format. `PdfTextExtractor` returns
`ExtractionError::Unsupported` for non-PDF media so the
`FallbackExtractor` chain advances to the vision-backed extractor.

## Vision fallback

`AnthropicVisionExtractor` is the fallback in the extractor chain when
an API key is present. It sends PDFs as `document` content blocks and
images as `image` content blocks. Same `AnthropicTransport` trait the
classifier uses — both share one HTTP client in production. Bytes are
capped at 5 MiB raw to stay safely under Anthropic's encoded-upload
ceiling.

## Search (grep)

`GrepSearchIndex` walks `library/<category>/*.md`, scores by
case-insensitive substring matches, returns a windowed snippet, and
honors `_unsorted/` while skipping `_logs/`. No persistent index — we
re-read the markdown on every query. If a library grows past the
point this stops feeling instant, swap in a Tantivy-backed adapter.

## MCP stdio server

`paperclaw serve-mcp` speaks newline-delimited JSON-RPC 2.0 over stdio.
The server `run` fn is generic over `AsyncRead + AsyncWrite` so
integration tests drive it via `tokio::io::duplex` instead of spawning
a subprocess.

Exposed tools:

| Tool                | What it does                                       |
|---------------------|----------------------------------------------------|
| `search_documents`  | Grep + optional category filter                    |
| `list_documents`    | Walk the library, return per-doc metadata          |
| `get_document`      | Return one document's transcript + sidecar        |
| `ingest_inbox`      | Process the user's inbox folder                    |
| `ingest_document`   | Ingest base64 bytes handed in by the caller        |

`ingest_document` is what lets an upstream LLM hand `PaperClaw` a file
through the tool call itself. The use-case calls
`IngestService::ingest_pending`, which bypasses the inbox source — the
bytes never touch the user's inbox folder.

When running the MCP server, set `PAPERCLAW_LOG=warn` so trace output
on stderr doesn't compete with structured-output channels the calling
agent may also be monitoring.

## Milestone status

See `docs/DESIGN.md` §9 for the M3 hardening status and the deferred
list (content-hash dedupe, confidence-tiered escalation, on-device OCR,
embedding-backed search).
