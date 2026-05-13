//! Concrete adapters for `PaperClaw`'s domain ports.
//!
//! At M1 only the surface shapes ship: filesystem inbox & store, a
//! placeholder PDF extractor, a rule-based classifier stub, and the
//! [`FallbackExtractor`] composition wrapper that M3 will use to layer
//! OCR behind text-layer extraction.

pub mod anthropic;
pub mod anthropic_vision;
pub mod classifier;
pub mod clock;
pub mod fs;
pub mod lock;
pub mod ocr;
pub mod pdf;
pub mod search;

pub use anthropic::{
    AnthropicClassifier, AnthropicClassifierConfig, AnthropicTransport, ReqwestTransport,
    SecretString as AnthropicApiKey, TransportError,
};
pub use anthropic_vision::{AnthropicVisionConfig, AnthropicVisionExtractor};
pub use classifier::{NotImplementedClassifier, RuleBasedClassifier};
pub use clock::{SystemClock, UuidV4Generator};
pub use fs::{FsInboxSource, FsLibraryStore};
pub use lock::{IngestLock, LockError};
pub use ocr::FallbackExtractor;
pub use pdf::PdfTextExtractor;
pub use search::{GrepSearchIndex, StubSearchIndex};
