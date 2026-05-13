#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn help_lists_every_m1_subcommand() {
    let mut cmd = Command::cargo_bin("paperclaw").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(contains("ingest"))
        .stdout(contains("search"))
        .stdout(contains("serve-mcp"))
        .stdout(contains("doctor"));
}

#[test]
fn doctor_prints_adapter_health() {
    let mut cmd = Command::cargo_bin("paperclaw").unwrap();
    cmd.arg("doctor")
        .assert()
        .success()
        .stdout(contains("paperclaw doctor"))
        .stdout(contains("FsInboxSource"))
        .stdout(contains("RuleBasedClassifier"))
        .stdout(contains("FallbackExtractor"));
}
