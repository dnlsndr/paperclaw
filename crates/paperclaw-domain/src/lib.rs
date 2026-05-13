//! Pure domain types and trait ports for `PaperClaw`.
//!
//! This crate has no I/O. It defines what `PaperClaw` operates on
//! (`Document`, `Transcript`, `Classification`, …) and the trait surface
//! the application layer talks to. Concrete implementations live in
//! `paperclaw-adapters`; the binary in `paperclaw-cli` wires them up.

#![warn(missing_docs)]

pub mod errors;
pub mod policy;
pub mod ports;
mod slug;
pub mod types;

#[cfg(feature = "testing")]
pub mod testing;

pub use errors::ExtractionError;
pub use policy::LibraryPathPolicy;
pub use ports::{
    Classifier, Clock, IdGenerator, InboxSource, LibraryStore, LibraryWrite, SearchIndex,
    TextExtractor,
};
pub use types::{
    Classification, Confidence, Document, DocumentId, DocumentKind, IngestOutcome, IngestReport,
    LibraryPath, PendingDocument, SearchHit, SourcePath, Transcript,
};
