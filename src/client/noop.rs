//! No-op VDB client: accepts every operation, responds immediately.
//!
//! Used by the load-test binary (`bench-server`) to isolate the
//! performance of the middleware itself, so that Qdrant's network latency
//! and capacity don't contaminate the measurements.
//!
//! NOT for production — it performs no real storage.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::error::Error;

use super::{SearchHit, SearchParams, UpsertParams, VectorDbClient};

/// No-op VDB client, for benchmarking the middleware in isolation.
#[derive(Default)]
pub struct NoopVdbClient {
    upserts: AtomicU64,
    searches: AtomicU64,
}

impl NoopVdbClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_count(&self) -> u64 {
        self.upserts.load(Ordering::Relaxed)
    }

    pub fn search_count(&self) -> u64 {
        self.searches.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl VectorDbClient for NoopVdbClient {
    async fn upsert(&self, _params: UpsertParams) -> Result<(), Error> {
        self.upserts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn search(&self, _params: SearchParams) -> Result<Vec<SearchHit>, Error> {
        self.searches.fetch_add(1, Ordering::Relaxed);
        Ok(Vec::new())
    }

    async fn health(&self) -> Result<(), Error> {
        Ok(())
    }
}
