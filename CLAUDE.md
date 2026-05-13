# PaperClaw — agent operating contract

This file is the contract between the human and any agent working in this
repo. Keep it short and current. Edit it whenever a rule changes.

## Project shape

PaperClaw is a Rust workspace with **four crates** following clean
(ports-and-adapters / hexagonal) architecture:

```
crates/
├── paperclaw-domain      pure types + trait ports, no I/O
├── paperclaw-app         use-cases orchestrating ports
├── paperclaw-adapters    concrete fs / pdf / classifier impls
└── paperclaw-cli         binary, composition root, future MCP stdio host
```

Dependencies flow inward: `cli → app → domain`, `adapters → domain`,
`cli → adapters` (only for wiring). **Never** add a reverse edge.

The design doc at `docs/DESIGN.md` is the source of truth for architecture
decisions. If you change one, update the doc in the same commit.

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

Other useful targets: `just fmt`, `just lint`, `just test`, `just doc`,
`just hack`, `just deny`, `just doctor`.

## Testing rules

- Unit tests live in the same crate as the code under test.
- Use the in-memory fakes from `paperclaw_domain::testing` (feature
  `testing`) instead of `mockall` or hand-rolled doubles.
- Inject `FixedClock` and `SeqIdGenerator` so use-case tests are
  deterministic.
- Tests opt out of `clippy::unwrap_used` already — feel free to `.unwrap()`
  on test fixtures.

## Idioms

- All trait ports are `Send + Sync`. Use `#[async_trait]` so they stay
  dyn-compatible behind `Arc<dyn ...>`.
- Errors: `thiserror` in libraries, `anyhow` in the CLI binary.
- No `unsafe_code` (forbidden workspace-wide).
- No `println!` / `eprintln!` outside tests — use `tracing`.
- No `todo!()` in shipped code; return an explicit `*Error::NotImplemented`
  variant so the type system tracks unfinished work.

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
yielding entries — the `.pdf` extension is treated as a hint.

## Concurrency + panic policy

`IngestService::ingest_all` fans out **one tokio task per pending
document**. Adapters must be re-entrant; `FsLibraryStore` serializes
its commit critical section internally via a `tokio::sync::Mutex`.

**Panics inside ingest tasks abort the batch** (`resume_unwind`). Do
not catch panics to make them into `IngestOutcome::Failed`. If you ship
an adapter wrapping a parser that can crash on adversarial input,
isolate it inside `tokio::task::spawn_blocking` and translate the
resulting `JoinError` to an `ExtractionError::Other`.

## Sidecar schema

`MetadataSidecar` is versioned by a monotonically-incrementing
`schema_version` field. Readers must reject unknown versions rather
than guess. Every schema bump ships with a `paperclaw migrate` CLI
subcommand that upgrades existing sidecars in place. M1 sidecars carry
`schema_version: 1` and include: `id`, `ingested_at`,
`original_filename` (verbatim), `classifier_version`, `classification`,
`transcript_bytes`, `pdf_bytes`. Content hash (`pdf_sha256`) lands at M3.

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

## Hardening checklist — what landed in M2 vs deferred

`docs/DESIGN.md` §9 is the source of truth; this list is a short prompt
for the next agent.

Landed in M2:

- Real PDF extractor (`pdf-extract`) wrapped in `spawn_blocking` + a
  30s `tokio::time::timeout`. A malformed PDF can't wedge the batch.
- `AnthropicClassifier` with Haiku 4.5, prompt caching on the system
  prompt, head+tail transcript truncation, JSON-schema-constrained
  tool-use response, and a 280-char rationale cap.
- Transcript sanitizer (`paperclaw_domain::sanitize::redact`) wired
  into both classifier impls.
- `.env` loading and `SecretString` redaction for the API key.

Deferred to M3:

- Content-hash dedupe (`pdf_sha256` in the sidecar; skip re-classify
  on hash match).
- Confidence-driven escalation Haiku → Sonnet → Opus.
- Real search index (still `StubSearchIndex`).
- OCR fallback in `FallbackExtractor`.

## Out of scope for now

OCR, MCP server, search indexing, content-hash dedupe — all stubbed at
M2 and built out in M3.
