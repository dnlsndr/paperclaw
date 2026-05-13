//! Application services for `PaperClaw`. Holds use-cases that orchestrate
//! the domain ports defined in [`paperclaw_domain`].

pub mod ingest;
pub mod search;

pub use ingest::{AppError, IngestService};
pub use search::SearchService;
