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

## Out of scope for now

OCR, MCP server, real PDF extraction, real Anthropic classifier, search
indexing — all stubbed at M1 and built out in M2 / M3.
