# PaperClaw

PaperClaw turns a flat `~/inbox/` folder of paperwork — utility bills,
invoices, contracts, insurance letters, bank statements, tax mail — into
a queryable `~/library/`. Each archived PDF lands next to a Markdown
transcript and a JSON metadata sidecar, so an agent can search the
library and answer practical questions about it.

It speaks two interfaces: a CLI you can run yourself, and an MCP stdio
server (`paperclaw serve-mcp`) that lets an upstream LLM drive the tool
over JSON-RPC.

> **Status:** M3 — vision-backed extraction, real grep search, and the
> MCP server have landed. The CLI is driveable by an agent end-to-end.
> See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design and the
> deferred items.

## How it works

1. Drop PDFs / images (JPEG / PNG / WebP) into the inbox.
2. `paperclaw ingest` sniffs each file's magic bytes, extracts text
   (`pdf-extract` first, vision fallback via the Anthropic Messages API
   for scanned or image-only docs), and classifies it into a category
   (rule-based offline, or the Anthropic API when a key is configured).
3. Each filed document is written to `library/<category>/` as a sibling
   `.pdf`, `.md` (transcript), and `.json` (metadata sidecar). The inbox
   copy is removed only on a successful file or low-confidence skip;
   encrypted PDFs and hard failures stay in the inbox for retry.
4. `paperclaw search "<query>"` greps the markdown transcripts; the MCP
   tools expose the same surface to an agent.

## Quickstart

Prerequisites: a Rust toolchain pinned to `rustc 1.94.0` (see
`rust-toolchain.toml`), and [`just`](https://github.com/casey/just) for
the task runner.

```bash
# Verify the workspace is healthy.
just check

# Drop a PDF into ./inbox/, then:
cargo run -q --bin paperclaw -- ingest

# Search the library.
cargo run -q --bin paperclaw -- search "Finanzamt"
```

CLI surface:

```text
ingest      Process every PDF/image in the inbox and file it into the library
search      Grep over the library transcripts
serve-mcp   Speak MCP (JSON-RPC 2.0) over stdio
doctor      Print configuration and adapter health
```

Override paths with `--inbox` / `--library` or `PAPERCLAW_INBOX` /
`PAPERCLAW_LIBRARY`.

## Configuration

The CLI loads `.env` at startup. Recognised variables:

| Variable                     | Effect                                                                                       |
|------------------------------|----------------------------------------------------------------------------------------------|
| `ANTHROPIC_API_KEY`          | Opt into the LLM classifier and the vision-backed text extractor. Wrapped in a redacting secret type. |
| `PAPERCLAW_CLASSIFIER`       | `auto` (default; LLM when key is present), `anthropic` (force LLM), `rule-based` (force offline). |
| `PAPERCLAW_ANTHROPIC_MODEL`  | Override the Anthropic model ID (default `claude-haiku-4-5`).                                |
| `PAPERCLAW_INBOX`            | Inbox directory (default `./inbox`).                                                         |
| `PAPERCLAW_LIBRARY`          | Library root (default `./library`).                                                          |
| `PAPERCLAW_LOG`              | `tracing` filter. Set to `warn` when running `serve-mcp` so logs don't compete with JSON-RPC on stderr. |

## MCP server

`paperclaw serve-mcp` speaks newline-delimited JSON-RPC 2.0 over stdio
and exposes five tools:

| Tool                | What it does                                              |
|---------------------|-----------------------------------------------------------|
| `search_documents`  | Grep across the library, optional `category` filter       |
| `list_documents`    | Walk the library, return per-doc metadata                 |
| `get_document`      | Return one document's full transcript + sidecar           |
| `ingest_inbox`      | Process the user's inbox folder                           |
| `ingest_document`   | Ingest base64 bytes handed in by the caller (bypasses the inbox) |

`ingest_document` is what lets an upstream LLM hand PaperClaw a file
through the tool call itself — the bytes never touch the inbox folder.

## Architecture

Four crates, hexagonal / ports-and-adapters, strictly inward
dependencies:

| Crate                | Role                                              |
|----------------------|---------------------------------------------------|
| `paperclaw-domain`   | Pure types and trait ports. No I/O.               |
| `paperclaw-app`      | Use-cases orchestrating ports.                    |
| `paperclaw-adapters` | Concrete fs / pdf / classifier / vision impls.    |
| `paperclaw-cli`      | Binary, composition root, MCP stdio host.         |

[`docs/DESIGN.md`](docs/DESIGN.md) is the source of truth for the
architecture and the deferred work (content-hash dedupe, confidence-
tiered model escalation, on-device OCR, embedding-backed search).
[`CLAUDE.md`](CLAUDE.md) is the operating contract for agents working
in the repo.

## Development

```bash
just check        # fmt-check + clippy -D warnings + nextest. Run before declaring done.
just check-quick  # what the pre-commit hook runs.
just --list       # everything else (fmt, lint, test, doc, hack, deny, doctor).
```

Conventions worth knowing:

- Tests live in the same crate as the code under test; use the
  in-memory fakes from `paperclaw_domain::testing` (feature `testing`)
  instead of `mockall`.
- Workspace lints forbid `unsafe_code` and warn on `println!` /
  `eprintln!` / `todo!()` / `dbg!` / `unwrap` / `expect` — use
  `tracing` and return explicit `*Error::NotImplemented` variants.
- Errors: `thiserror` in libraries, `anyhow` in the CLI binary.

## License

MIT OR Apache-2.0 (per `Cargo.toml`). Add `LICENSE-MIT` and
`LICENSE-APACHE` files before publishing.
