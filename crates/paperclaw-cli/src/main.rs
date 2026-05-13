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

    let filter = EnvFilter::try_from_env("PAPERCLAW_LOG")
        .unwrap_or_else(|_| EnvFilter::new("paperclaw=info,warn"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}
