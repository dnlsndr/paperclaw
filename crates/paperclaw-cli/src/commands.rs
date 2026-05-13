#![allow(clippy::print_stdout)]

//! CLI command surface. M1 ships the shape: `ingest`, `search`,
//! `serve-mcp`, and `doctor`. Only `doctor` is fully wired; the others
//! report which downstream piece is still stubbed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use paperclaw_domain::types::IngestOutcome;

use crate::wiring::{Wiring, WiringConfig};

/// `PaperClaw` — organize PDFs into a searchable library.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None, name = "paperclaw")]
pub struct Cli {
    /// Override the inbox directory.
    #[arg(
        long,
        env = "PAPERCLAW_INBOX",
        default_value = "./inbox",
        global = true
    )]
    pub inbox: PathBuf,

    /// Override the library root.
    #[arg(
        long,
        env = "PAPERCLAW_LIBRARY",
        default_value = "./library",
        global = true
    )]
    pub library: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Sub-commands exposed by the CLI.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Process every PDF in the inbox and file it into the library.
    Ingest,

    /// Search the library (M3).
    Search {
        /// Query string to match against transcripts.
        query: String,
        /// Max number of results.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Speak Model Context Protocol over stdio (M3).
    ServeMcp,

    /// Print configuration and adapter health.
    Doctor,
}

/// Entry point shared with the integration test harness.
pub async fn run(cli: Cli) -> Result<()> {
    let wiring = Wiring::build(&WiringConfig {
        inbox: cli.inbox.clone(),
        library: cli.library.clone(),
    });

    match cli.command {
        Command::Doctor => {
            doctor(&wiring, &cli);
            Ok(())
        }
        Command::Ingest => ingest(&wiring).await,
        Command::Search { query, limit } => search(&wiring, &query, limit).await,
        Command::ServeMcp => {
            serve_mcp();
            Ok(())
        }
    }
}

fn doctor(wiring: &Wiring, cli: &Cli) {
    println!("paperclaw doctor");
    println!("================");
    println!("inbox:    {}", cli.inbox.display());
    println!("library:  {}", cli.library.display());
    println!();
    println!("adapters:");
    for line in wiring.health_lines() {
        println!("  {line}");
    }
    println!();
    println!("Tip: set PAPERCLAW_LOG=debug for verbose logs.");
}

async fn ingest(wiring: &Wiring) -> Result<()> {
    let report = wiring
        .ingest()
        .ingest_all()
        .await
        .context("ingest run failed")?;

    println!("ingest summary");
    println!("==============");
    println!("processed:        {}", report.len());
    println!("filed:            {}", report.filed_count());
    println!("encrypted-skip:   {}", report.encrypted_count());

    let low_conf = report
        .entries
        .iter()
        .filter(|e| matches!(e.outcome, IngestOutcome::SkippedLowConfidence { .. }))
        .count();
    let failed = report
        .entries
        .iter()
        .filter(|e| matches!(e.outcome, IngestOutcome::Failed { .. }))
        .count();
    println!("low-confidence:   {low_conf}");
    println!("failed:           {failed}");

    if report.encrypted_count() > 0 {
        println!();
        println!(
            "{} encrypted PDFs skipped — decrypt them and re-drop them in inbox/.",
            report.encrypted_count(),
        );
    }

    Ok(())
}

async fn search(wiring: &Wiring, query: &str, limit: usize) -> Result<()> {
    let hits = wiring
        .search()
        .query(query, limit)
        .await
        .context("search failed")?;

    if hits.is_empty() {
        println!("(no results — search backend lands in M3)");
        return Ok(());
    }

    for hit in hits {
        println!(
            "{} [{}/{}] score={:.2}",
            hit.document_id, hit.library_path.category, hit.library_path.stem, hit.score,
        );
        if let Some(snippet) = hit.snippet {
            println!("    {snippet}");
        }
    }
    Ok(())
}

fn serve_mcp() {
    println!("MCP server lands at M3. Run `paperclaw doctor` for current state.");
}
