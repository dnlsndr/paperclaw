set shell := ["bash", "-uc"]

# Default: the agent loop. Fastest path to "is the workspace healthy?"
default: check

# --- Single-purpose targets ---------------------------------------------------

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

typecheck:
    cargo check --workspace --all-targets --all-features

test:
    @if command -v cargo-nextest >/dev/null 2>&1; then \
        cargo nextest run --workspace --all-features; \
    else \
        cargo test --workspace --all-features; \
    fi

doc:
    cargo doc --workspace --no-deps --all-features

# --- Composite targets --------------------------------------------------------

# Full local check. Run this before declaring work done.
check: fmt-check lint test

# Lighter check for pre-commit: skips test runner overhead but still catches
# format drift and lint regressions.
check-quick: fmt-check
    cargo clippy --workspace --all-targets -- -D warnings

# --- Supply-chain & dep hygiene (CI primarily) --------------------------------

deny:
    cargo deny check

machete:
    cargo machete

hack:
    cargo hack check --workspace --feature-powerset --depth 2

# --- Tooling bootstrap --------------------------------------------------------

install-hooks:
    git config core.hooksPath .githooks
    @echo "Installed .githooks as the git hooks path."

install-dev-tools:
    cargo install cargo-nextest cargo-machete cargo-hack cargo-deny --locked

# --- Runtime convenience ------------------------------------------------------

run *args:
    cargo run -p paperclaw-cli -- {{args}}

doctor:
    cargo run -p paperclaw-cli -- doctor
