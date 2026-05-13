# PaperClaw — Design Document (M1)

> Status: M1 design. Updated whenever a decision in here changes.
> Last reviewed: 2026-05-13.

## 1. Problem & scope

PaperClaw ingests everyday paperwork (utility bills, invoices, contracts,
insurance letters, bank statements, Finanzamt mail) from a flat `~/inbox/`
folder and organizes it into a queryable `~/library/`. Each archived PDF is
stored alongside a Markdown transcript and a JSON metadata sidecar so an
agent can search and answer practical questions.

**In scope:** local PDF ingest, classification into a small set of
categories, deterministic filenames, Markdown transcripts, structured
ingest logs, an agent-callable CLI.

**Out of scope (for the workshop):** images / scanned-only documents
without OCR, email-based ingest, multi-user sharing, encryption of the
library, password-protected PDFs (skipped — see §6).

## 2. Stack

| Concern              | Choice                                              |
|----------------------|-----------------------------------------------------|
| Language / edition   | Rust 2024, pinned `rustc 1.94.0`                    |
| Async runtime        | `tokio` (multi-thread, `fs`, `macros`, `io-util`)   |
| Trait objects        | `#[async_trait]` for dyn-compat ports               |
| Errors               | `thiserror` in libraries, `anyhow` in the binary    |
| CLI                  | `clap` (derive)                                     |
| Logs                 | `tracing` + `tracing-subscriber` (JSON layer)       |
| Serialization        | `serde`, `serde_json`                               |
| Enums                | `strum` derives for `DocumentKind`                  |
| Time / IDs           | `time` for dates, `uuid` for document IDs           |
| Tests                | `cargo nextest` + `tempfile`; in-memory fakes       |
| PDF extraction       | Trait only at M1; concrete crate chosen at M2       |
| LLM provider         | Anthropic (wired at M3)                             |

## 3. Architecture

Hexagonal / ports-and-adapters. Four crates with strictly inward
dependencies:

```
                    ┌──────────────────────┐
                    │   paperclaw-cli      │  binary + composition root
                    │   (clap, MCP later)  │
                    └──────────┬───────────┘
              wires │          │ wires
            ┌───────┴───┐      └───────┐
            ▼           ▼              ▼
   ┌────────────────┐   ┌──────────────────────┐
   │ paperclaw-app  │──▶│  paperclaw-domain    │  pure types + trait ports
   │  use-cases     │   │  (no I/O)            │
   └────────┬───────┘   └──────────────────────┘
            │                       ▲
            │ depends on            │ depends on
            ▼                       │
   ┌──────────────────────┐         │
   │ paperclaw-adapters   │─────────┘
   │ fs / pdf / classifier│
   └──────────────────────┘
```

**Dependency direction is the rule.** `domain` knows nothing about `tokio`
filesystem APIs, Anthropic, or `clap`. `app` knows only the trait surface.
`adapters` may use real I/O libraries but never refer to `app` or `cli`.

### DI

Services hold `Arc<dyn Trait + Send + Sync>` fields. The CLI's composition
root constructs concrete adapters once and hands them to use-cases. This
keeps swapping adapters (fake → real, rule-based → LLM, fs → in-memory) a
one-line change.

### Ports (M1 surface)

| Port                 | Responsibility                                     |
|----------------------|----------------------------------------------------|
| `InboxSource`        | List & read pending PDFs                           |
| `TextExtractor`      | Bytes → `Transcript`, signals encryption via error |
| `Classifier`         | `&Transcript` → `Classification` *(shape TBD M2)*  |
| `LibraryStore`       | Persist `(pdf, transcript, metadata)`              |
| `SearchIndex`        | Query transcripts *(shape TBD M3)*                 |
| `Clock`              | Inject time for deterministic tests                |
| `IdGenerator`        | Inject IDs for deterministic tests                 |
| `LibraryPathPolicy`  | Slug rules + category folder routing               |

Note: `Classifier` and `SearchIndex` are stubs at M1; expect their shapes
to change once M2 / M3 give them real callers.

### What is *not* a port

* **Logging.** `tracing` already gives us structured, leveled, JSON-capable
  output. Wrapping it in a port adds friction without clarifying anything.
* **PDF encryption preflight.** Lives as `ExtractionError::Encrypted` on
  the `TextExtractor` trait. Two-phase APIs invite TOCTOU bugs; exhaustive
  enum errors are idiomatic Rust.
* **OCR.** OCR is another `TextExtractor` impl, consumed via a
  `FallbackExtractor` chain-of-responsibility wrapper. The ingest use-case
  stays unaware of which extractor produced the transcript.

## 4. Library layout

```
library/
├── invoices/
│   ├── 2026-03-15_acme-co_inv-1234.pdf
│   ├── 2026-03-15_acme-co_inv-1234.md
│   └── 2026-03-15_acme-co_inv-1234.paperclaw.json
├── bills/
├── contracts/
├── bank-statements/
├── insurance/
├── tax/
├── _unsorted/                 # low-confidence bucket
└── _logs/
    └── ingest-2026-05-13.jsonl
```

**Filename policy** — `YYYY-MM-DD_<sender>_<subject>.pdf`, slugified to
`[a-z0-9-]+`. Date falls back to ingest date when the classifier can't
extract one. Collisions resolved with a `-2`, `-3`, … suffix.

**Metadata sidecar** carries the `Classification`, original filename,
content hash, ingest timestamp, and classifier version. Lets the agent
re-classify without re-extracting and lets us audit ingest decisions.

## 5. Classifier strategy

* **M2:** `RuleBasedClassifier` using simple keyword matches over the
  transcript. Deterministic, free, easy to test.
* **M3:** `AnthropicClassifier` calling Claude via the official Rust SDK.
  Opt-in via `ANTHROPIC_API_KEY`. Rule-based stays as the offline default.
* Both implement the same `Classifier` trait. The use-case never knows
  which is in use.

## 6. Encrypted-PDF handling

The `TextExtractor` adapter inspects PDFs on open. If a document is
encrypted / password-protected, it returns
`ExtractionError::Encrypted { hint }` (no real preflight port, no boolean
flag method). The ingest use-case:

1. Emits a `tracing::warn!` event with the source path and the hint.
2. Records `IngestOutcome::SkippedEncrypted` in the `IngestReport`.
3. Continues with the next PDF — never aborts the batch.

The CLI prints a per-batch summary at the end:

> "3 encrypted PDFs skipped. Decrypt them and re-drop them in `inbox/`."

## 7. OCR roadmap

Scanned-only PDFs have no text layer; `pdf-extract` will return empty
output. The plan:

1. M1 ships the `FallbackExtractor { primary, fallback }` composition
   wrapper. Real fallback is a no-op for now.
2. M3+ swaps the fallback for a `TesseractExtractor` (system `tesseract`
   binary or a Rust crate — TBD when we get there).
3. The use-case never branches on "is this scanned?". The wrapper does.

## 8. Security & privacy

* **Local-first.** No telemetry. No content leaves the host except when
  the Anthropic classifier is enabled.
* **PII awareness.** PDFs may contain Steuer-ID, IBAN, salary data. Logs
  by default contain only IDs, original filenames, classifications, and
  outcomes — never transcript content.
* **Library is gitignored.** `/inbox`, `/library`, and `*.paperclaw.bak`
  are listed in `.gitignore`.
* **LLM calls opt-in** via `ANTHROPIC_API_KEY`. The CLI refuses to send a
  transcript to the API if the variable is unset.
* **`unsafe_code = "forbid"`** at the workspace level.
* **Pre-commit hook** runs lint + format on every commit so secrets-style
  config drift is caught early. (Hook is bash, not a Python framework.)

## 9. Open questions / risks

* **OCR cost & accuracy** — Tesseract is good enough for typed scans but
  hand-written or low-DPI scans degrade fast. Need a confidence signal.
* **Encrypted-PDF ergonomics** — should the CLI offer a `paperclaw
  decrypt <pdf>` companion? Out of scope for M1.
* **Classifier token cost** — a 30-page bank statement is expensive to
  classify. Likely truncate transcript before sending; design at M3.
* **Transcript size** — very large PDFs blow up the sidecar. Cap or chunk
  at M3 when search lands.

## 10. Feedback loops (summary)

The agent's default loop is **`just check`** (fmt-check + clippy
`-D warnings` + tests). CI adds `cargo-hack` for feature-flag coverage,
`cargo-machete` for unused deps, and `cargo-deny` for license/supply-chain.
Pre-commit runs `just check-quick`.

The trait fakes in `paperclaw-domain::testing` (gated by the `testing`
feature) let every use-case test inject deterministic clocks, IDs, and
in-memory stores — no real PDFs needed for the M1 trait surface.
