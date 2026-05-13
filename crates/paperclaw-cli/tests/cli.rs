//! In-process clap surface tests.
//!
//! Subprocess-based testing (`assert_cmd`) burns build time and forces
//! you to assert on stdout regex. Clap's derive surface gives us a
//! typed `Cli` we can construct via [`clap::Parser::parse_from`] and
//! poke at directly — much cleaner.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use paperclaw_cli::config::SecretString;
use paperclaw_cli::{AppConfig, ClassifierChoice, Cli, Command, Wiring, WiringConfig};

/// Help should mention every M1 subcommand. We don't go through stdout —
/// we ask clap directly which subcommands the derive macro registered.
#[test]
fn cli_registers_every_subcommand() {
    let registered: Vec<_> = Cli::command()
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();

    for expected in ["ingest", "search", "serve-mcp", "doctor"] {
        assert!(
            registered.iter().any(|s| s == expected),
            "subcommand `{expected}` missing from clap surface; got: {registered:?}",
        );
    }
}

#[test]
fn parses_doctor_invocation() {
    let cli = Cli::parse_from(["paperclaw", "doctor"]);
    assert!(matches!(cli.command, Command::Doctor));
}

#[test]
fn parses_ingest_with_custom_inbox_and_library() {
    let cli = Cli::parse_from([
        "paperclaw",
        "--inbox",
        "/tmp/in",
        "--library",
        "/tmp/lib",
        "ingest",
    ]);
    assert_eq!(cli.inbox, PathBuf::from("/tmp/in"));
    assert_eq!(cli.library, PathBuf::from("/tmp/lib"));
    assert!(matches!(cli.command, Command::Ingest));
}

#[test]
fn parses_search_with_default_limit() {
    let cli = Cli::parse_from(["paperclaw", "search", "Rechnung"]);
    match cli.command {
        Command::Search { query, limit } => {
            assert_eq!(query, "Rechnung");
            assert_eq!(limit, 10);
        }
        other => panic!("expected Search, got {other:?}"),
    }
}

/// Without a key, the auto-classifier resolves to rule-based; the
/// health line must say so. No subprocess, no stdout regex — we read
/// the typed strings the wiring exposes.
#[test]
fn wiring_falls_back_to_rule_based_when_no_key() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = WiringConfig {
        inbox: temp.path().join("inbox"),
        library: temp.path().join("library"),
    };
    let app_cfg = AppConfig {
        anthropic_api_key: None,
        anthropic_model: "claude-haiku-4-5".into(),
        classifier: ClassifierChoice::Auto,
    };
    let wiring = Wiring::build(&cfg, &app_cfg).unwrap();
    let health = wiring.health_lines().join("\n");
    assert!(health.contains("RuleBasedClassifier"), "got:\n{health}");
    assert!(!health.contains("AnthropicClassifier"));
}

#[test]
fn wiring_picks_anthropic_when_key_is_present() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = WiringConfig {
        inbox: temp.path().join("inbox"),
        library: temp.path().join("library"),
    };
    let app_cfg = AppConfig {
        anthropic_api_key: Some(SecretString::for_test(
            "sk-ant-test-dummy-not-real".to_owned(),
        )),
        anthropic_model: "claude-haiku-4-5".into(),
        classifier: ClassifierChoice::Auto,
    };
    let wiring = Wiring::build(&cfg, &app_cfg).unwrap();
    let health = wiring.health_lines().join("\n");
    assert!(
        health.contains("AnthropicClassifier(claude-haiku-4-5)"),
        "got:\n{health}",
    );
}

#[test]
fn wiring_forced_anthropic_without_key_is_an_error() {
    // Catching this at startup (rather than on the first ingest) is the
    // whole point of resolving config eagerly in the composition root.
    let temp = tempfile::tempdir().unwrap();
    let cfg = WiringConfig {
        inbox: temp.path().join("inbox"),
        library: temp.path().join("library"),
    };
    let app_cfg = AppConfig {
        anthropic_api_key: None,
        anthropic_model: "claude-haiku-4-5".into(),
        classifier: ClassifierChoice::Anthropic,
    };
    let err = Wiring::build(&cfg, &app_cfg).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ANTHROPIC_API_KEY"),
        "error should explain what's missing, got: {msg}",
    );
}
