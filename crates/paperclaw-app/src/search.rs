//! Search use-case. Wraps a [`paperclaw_domain::SearchIndex`] trait
//! object so the CLI / MCP host can swap backends (grep, tantivy,
//! embedding-based) without touching higher layers.
//!
//! M1 ships the service shell; concrete search lands in M3.

use std::sync::Arc;

use paperclaw_domain::SearchIndex;
use paperclaw_domain::ports::SearchError;
use paperclaw_domain::types::SearchHit;

/// Search facade for the CLI / MCP server.
#[derive(Clone)]
pub struct SearchService {
    index: Arc<dyn SearchIndex>,
}

impl std::fmt::Debug for SearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchService").finish_non_exhaustive()
    }
}

impl SearchService {
    /// Wire the service with a chosen index adapter.
    #[must_use]
    pub fn new(index: Arc<dyn SearchIndex>) -> Self {
        Self { index }
    }

    /// Query the library; returns up to `limit` ranked hits.
    ///
    /// # Errors
    ///
    /// Propagates whatever the underlying [`SearchIndex`] returns —
    /// typically [`SearchError::NotImplemented`] in M1.
    pub async fn query(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        self.index.search(query, limit).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use paperclaw_domain::testing::{EmptySearchIndex, assert_send, assert_sync};

    use super::*;

    const _: fn() = || {
        assert_send::<SearchService>();
        assert_sync::<SearchService>();
    };

    #[tokio::test]
    async fn empty_index_returns_no_hits() {
        let svc = SearchService::new(Arc::new(EmptySearchIndex));
        let hits = svc.query("rent", 10).await.unwrap();
        assert!(hits.is_empty());
    }
}
