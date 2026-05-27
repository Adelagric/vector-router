//! Client VDB no-op : accepte toutes les opérations, répond immédiatement.
//!
//! Utilisé par le binaire de load-test (`bench-server`) pour isoler la
//! performance du middleware lui-même, sans que la latence réseau et la
//! capacité de Qdrant ne contaminent les mesures.
//!
//! PAS destiné à la production — il n'effectue aucun stockage réel.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::error::Error;

use super::{SearchHit, SearchParams, UpsertParams, VectorDbClient};

/// Client VDB qui ne fait rien, pour benchmark du middleware seul.
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
