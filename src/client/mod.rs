//! Abstraction layer over the vector database.
//!
//! The `VectorDbClient` trait is the boundary between our pipeline and the
//! concrete backend (Qdrant today, something else tomorrow if needed). Any
//! vendor-specific logic lives behind this trait. Parameter types are owned
//! to avoid lifetime friction across `.await`.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::Error;

pub mod noop;
#[cfg(feature = "pgvector")]
pub mod pgvector;
#[cfg(feature = "qdrant")]
pub mod qdrant;

pub use noop::NoopVdbClient;
#[cfg(feature = "pgvector")]
pub use pgvector::PgVectorClient;
#[cfg(feature = "qdrant")]
pub use qdrant::QdrantVdbClient;

/// Parameters for ingesting a single point.
#[derive(Debug, Clone)]
pub struct UpsertParams {
    pub namespace: String,
    pub point_id: String,
    pub vector: Vec<f32>,
    pub metadata: HashMap<String, String>,
}

/// Parameters for a kNN search.
#[derive(Debug, Clone)]
pub struct SearchParams {
    pub namespace: String,
    pub vector: Vec<f32>,
    pub limit: u32,
    /// Minimum score threshold. `None` = no filtering.
    pub score_threshold: Option<f32>,
    /// Key/value equality filter on the metadata of stored points.
    pub metadata_filter: HashMap<String, String>,
}

/// Individual search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub point_id: String,
    pub score: f32,
    pub metadata: HashMap<String, String>,
}

/// Generic contract with a vector database.
///
/// Any implementation must be `Send + Sync + 'static` so it can be stored
/// behind an `Arc<dyn VectorDbClient>` in the gRPC handlers.
#[async_trait]
pub trait VectorDbClient: Send + Sync + 'static {
    /// Ingests a point into the database. Idempotent on the Qdrant side: a
    /// second upsert with the same `point_id` replaces the previous value.
    async fn upsert(&self, params: UpsertParams) -> Result<(), Error>;

    /// Searches the k nearest neighbors in the given collection.
    async fn search(&self, params: SearchParams) -> Result<Vec<SearchHit>, Error>;

    /// Lightweight health ping. Feeds `/ready` and supervision.
    async fn health(&self) -> Result<(), Error>;

    /// Number of VDB operations currently in flight. Default `0` for
    /// backends that don't track this. Overridden by `QdrantVdbClient` via
    /// its internal `AtomicU64` (see `InflightGuard`).
    fn inflight(&self) -> u64 {
        0
    }
}

// --- Tests with a hand-rolled mock ------------------------------------------

#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::Mutex;

    /// Minimal mock that records calls and returns programmable responses.
    /// Used to test the handlers (step 7) and API consistency without
    /// having to start a real gRPC server.
    pub struct MockVdbClient {
        pub upserts: Mutex<Vec<UpsertParams>>,
        pub searches: Mutex<Vec<SearchParams>>,
        pub search_result: Mutex<Vec<SearchHit>>,
        pub fail_with: Mutex<Option<String>>,
    }

    impl MockVdbClient {
        pub fn new() -> Self {
            Self {
                upserts: Mutex::new(Vec::new()),
                searches: Mutex::new(Vec::new()),
                search_result: Mutex::new(Vec::new()),
                fail_with: Mutex::new(None),
            }
        }

        pub fn set_search_result(&self, hits: Vec<SearchHit>) {
            *self.search_result.lock().expect("mutex poisoned") = hits;
        }

        pub fn set_failure(&self, msg: impl Into<String>) {
            *self.fail_with.lock().expect("mutex poisoned") = Some(msg.into());
        }
    }

    impl Default for MockVdbClient {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl VectorDbClient for MockVdbClient {
        async fn upsert(&self, params: UpsertParams) -> Result<(), Error> {
            if let Some(msg) = self.fail_with.lock().expect("mutex poisoned").clone() {
                return Err(Error::Vdb(msg));
            }
            self.upserts.lock().expect("mutex poisoned").push(params);
            Ok(())
        }

        async fn search(&self, params: SearchParams) -> Result<Vec<SearchHit>, Error> {
            if let Some(msg) = self.fail_with.lock().expect("mutex poisoned").clone() {
                return Err(Error::Vdb(msg));
            }
            self.searches.lock().expect("mutex poisoned").push(params);
            Ok(self.search_result.lock().expect("mutex poisoned").clone())
        }

        async fn health(&self) -> Result<(), Error> {
            if let Some(msg) = self.fail_with.lock().expect("mutex poisoned").clone() {
                return Err(Error::Vdb(msg));
            }
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_records_upsert() {
        let mock = MockVdbClient::new();
        let params = UpsertParams {
            namespace: "ns".to_string(),
            point_id: "p1".to_string(),
            vector: vec![0.1, 0.2, 0.3],
            metadata: HashMap::new(),
        };
        mock.upsert(params.clone()).await.unwrap();
        let calls = mock.upserts.lock().expect("mutex");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].point_id, "p1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_returns_programmed_search_result() {
        let mock = MockVdbClient::new();
        mock.set_search_result(vec![SearchHit {
            point_id: "hit-1".to_string(),
            score: 0.95,
            metadata: HashMap::new(),
        }]);

        let params = SearchParams {
            namespace: "ns".to_string(),
            vector: vec![0.1, 0.2],
            limit: 10,
            score_threshold: None,
            metadata_filter: HashMap::new(),
        };
        let hits = mock.search(params).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].point_id, "hit-1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_propagates_failure() {
        let mock = MockVdbClient::new();
        mock.set_failure("connection refused");
        let params = UpsertParams {
            namespace: "ns".to_string(),
            point_id: "p1".to_string(),
            vector: vec![0.1],
            metadata: HashMap::new(),
        };
        let err = mock.upsert(params).await.unwrap_err();
        assert!(matches!(err, Error::Vdb(msg) if msg.contains("connection refused")));
    }
}
