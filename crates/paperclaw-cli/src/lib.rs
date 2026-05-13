//! Library crate for `paperclaw-cli`.
//!
//! The binary at `src/main.rs` is the user-facing entry point. This
//! library exists so integration tests can drive the same `Cli` /
//! `Wiring` / `commands::run` surface in-process via
//! [`clap::Parser::parse_from`] — much cleaner than spawning a
//! subprocess via `assert_cmd` and regexing stdout.

#![allow(clippy::print_stdout)]

pub mod commands;
pub mod config;
pub mod mcp;
pub mod wiring;

pub use commands::{Cli, Command, run};
pub use config::{AppConfig, ClassifierChoice};
pub use mcp::{McpServices, run as run_mcp};
pub use wiring::{Wiring, WiringConfig};
