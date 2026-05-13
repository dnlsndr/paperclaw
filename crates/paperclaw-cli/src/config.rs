//! Typed configuration loaded from `.env` + the process environment.
//!
//! Layered loader: [`dotenvy::dotenv`] populates the process environment
//! from a `.env` file at the workspace root, then the [`config`] crate
//! deserialises a typed [`AppConfig`] from that environment.
//!
//! The Anthropic API key is the only secret today. It is wrapped in
//! [`SecretString`] so a stray `tracing::error!(?config)` or
//! `Debug`-derived error renders it as `REDACTED` instead of leaking the
//! key. **Never** print or log a [`SecretString`] without going through
//! [`SecretString::expose`].

use std::fmt;

use anyhow::{Context, Result};
use config::{Config, Environment};
use serde::Deserialize;

/// All runtime config the binary cares about. Populated by [`load`].
///
/// Fields are derived from environment variables — the `config` crate
/// reads them case-insensitively. Use a `PAPERCLAW_` prefix for our own
/// settings; standard third-party keys (`ANTHROPIC_API_KEY`) are read at
/// their canonical names so users don't need to learn a parallel namespace.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    /// Anthropic API key. Optional; absence falls back to the rule-based
    /// classifier in the CLI's composition root. Wrapped so a stray Debug
    /// render doesn't dump the key into logs.
    pub anthropic_api_key: Option<SecretString>,

    /// Anthropic model ID to use for classification. Defaults to a Haiku
    /// tier (cheap + fast); intentionally not pinned to a date suffix so
    /// the env can override without a code change.
    #[serde(default = "default_anthropic_model")]
    pub anthropic_model: String,

    /// Override which classifier the CLI wires up. `"auto"` (default)
    /// picks Anthropic when the key is present, otherwise rule-based.
    /// `"rule-based"` forces the offline path even when a key is set
    /// — useful for tests and demos. `"anthropic"` forces the live API.
    #[serde(default = "default_classifier_choice")]
    pub classifier: ClassifierChoice,
}

fn default_anthropic_model() -> String {
    "claude-haiku-4-5".to_owned()
}

const fn default_classifier_choice() -> ClassifierChoice {
    ClassifierChoice::Auto
}

/// Which classifier the CLI should wire up at startup. See [`AppConfig::classifier`].
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClassifierChoice {
    /// Use Anthropic when a key is available, otherwise fall back to
    /// rule-based. Sensible default for both dev and CI.
    #[default]
    Auto,
    /// Force the rule-based classifier even if `ANTHROPIC_API_KEY` is set.
    RuleBased,
    /// Force the Anthropic classifier; error if no key is available.
    Anthropic,
}

/// Best-effort `.env` load + typed env deserialise.
///
/// `.env` is read from the current working directory; missing file is not
/// an error (production runs typically inject env vars via the parent
/// shell or systemd unit). `config`'s [`Environment`] source then turns
/// the process env into our [`AppConfig`].
///
/// # Errors
///
/// Fails if the env contains values that can't be deserialised into the
/// target types (e.g. `PAPERCLAW_CLASSIFIER=garbage`).
pub fn load() -> Result<AppConfig> {
    // Load .env if present. Missing file → not an error: production
    // deployments push env via the shell / unit file, not a checked-in
    // dotfile.
    let _ = dotenvy::dotenv();

    let builder = Config::builder()
        // The PAPERCLAW_ prefix scopes our own settings
        // (`PAPERCLAW_CLASSIFIER`, `PAPERCLAW_ANTHROPIC_MODEL`). The
        // prefix is stripped before deserialisation, so the AppConfig
        // field name is `anthropic_model`.
        .add_source(Environment::with_prefix("PAPERCLAW").try_parsing(true))
        // ANTHROPIC_API_KEY is the canonical Anthropic SDK env name; we
        // intentionally read it without our prefix so users don't need
        // to learn a parallel name.
        .add_source(
            Environment::default()
                .try_parsing(true)
                .keep_prefix(false)
                .with_list_parse_key("anthropic_api_key"),
        );

    let raw = builder
        .build()
        .context("failed to assemble config sources")?;
    let cfg: AppConfig = raw
        .try_deserialize()
        .context("failed to deserialise config from environment")?;
    Ok(cfg)
}

/// Newtype wrapping a secret string. The inner value is kept private and
/// the [`Debug`]/[`fmt::Display`] impls always render `REDACTED` so a
/// surprise log line or error report never leaks the secret.
///
/// To use the value, call [`SecretString::expose`] at the exact call site
/// that needs it (HTTP auth header construction, typically). Keeping the
/// expose call narrow makes leaks audit-able.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a raw secret value. Intentionally test-only and gated so we
    /// don't grow a habit of hand-rolling secrets at random call sites —
    /// production secrets always come in through [`load`] / serde.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(value: String) -> Self {
        Self(value)
    }

    /// Expose the inner value. Call this only at the point of use; do not
    /// store the return value in a struct that could be Debug-printed.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("REDACTED")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_never_leaks_the_inner_value() {
        let s = SecretString::for_test("sk-ant-supersecret-DO-NOT-LEAK".to_owned());
        let debug = format!("{s:?}");
        let display = format!("{s}");
        assert!(!debug.contains("sk-ant"), "Debug must not leak: {debug}");
        assert!(
            !display.contains("sk-ant"),
            "Display must not leak: {display}"
        );
        // Sanity: expose() does give the value when explicitly asked.
        assert_eq!(s.expose(), "sk-ant-supersecret-DO-NOT-LEAK");
    }

    #[test]
    fn classifier_choice_defaults_to_auto() {
        assert_eq!(default_classifier_choice(), ClassifierChoice::Auto);
    }
}
