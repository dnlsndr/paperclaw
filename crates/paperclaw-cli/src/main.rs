// CLI is the user-facing surface — printing to stdout is its job.
#![allow(clippy::print_stdout)]

use anyhow::Result;
use clap::Parser;

mod commands;
mod wiring;

use commands::{Cli, run};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    run(cli).await
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    // Default filter scopes to `paperclaw` crates at info level. The
    // previous `"paperclaw=info,warn"` directive accidentally tripped a
    // global `warn` filter that included third-party crate noise — the
    // comma separates *directives*, not paperclaw-scoped levels.
    let filter = EnvFilter::try_from_env("PAPERCLAW_LOG")
        .unwrap_or_else(|_| EnvFilter::new("paperclaw=info"));

    let builder = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);

    // PAPERCLAW_LOG_FORMAT=json switches to the structured JSON layer
    // (cargo feature is already enabled in workspace deps). Default
    // remains compact text for interactive use.
    match std::env::var("PAPERCLAW_LOG_FORMAT").as_deref() {
        Ok("json") => builder.json().init(),
        _ => builder.compact().init(),
    }
}
