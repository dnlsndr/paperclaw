//! Search adapter stubs.
//!
//! M1 ships [`StubSearchIndex`] — always returns no hits. M3 will add a
//! real grep / tantivy / embedding-backed index alongside it.

use async_trait::async_trait;
use paperclaw_domain::SearchIndex;
use paperclaw_domain::ports::SearchError;
use paperclaw_domain::types::SearchHit;

/// Always returns no hits. Used until M3 ships the real index.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubSearchIndex;

#[async_trait]
impl SearchIndex for StubSearchIndex {
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        Ok(Vec::new())
    }
}
