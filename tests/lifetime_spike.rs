//! Technical spike: verify that the `Cow<'a, [f32]>` pattern from the pool
//! remains usable after an `.await` inside a Tonic handler.
//!
//! This file is NOT a functional test of the service — it's a compile
//! proof and a minimal run that validates lifetime compatibility between
//! the pool, `validate_and_align`, and the tokio async runtime.
//!
//! Success criteria:
//! - The code compiles (Send constraints met on the handler future).
//! - The test runs without panicking.
//! - The numeric result is correct after the await.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tonic::{Request, Response, Status};

use vector_router::config::ModelSpec;
use vector_router::math::{l2_norm_squared, validate_and_align};
use vector_router::pool::BufferPool;
use vector_router::proto::vector_router::v1::{
    SearchRequest, SearchResponse, UpsertRequest, UpsertResponse,
    vector_router_server::VectorRouter,
};
use vector_router::registry::Registry;

struct SpikeService {
    pool: Arc<BufferPool>,
    registry: Arc<Registry>,
}

#[tonic::async_trait]
impl VectorRouter for SpikeService {
    async fn upsert(
        &self,
        request: Request<UpsertRequest>,
    ) -> Result<Response<UpsertResponse>, Status> {
        let req = request.into_inner();

        let spec = self
            .registry
            .get(&req.model_id)
            .ok_or_else(|| Status::invalid_argument(format!("unknown model {}", req.model_id)))?;

        if req.dim as usize != spec.dim {
            return Err(Status::invalid_argument("dim mismatch"));
        }

        let mut pooled = self.pool.take();

        // --- The heart of the test: obtaining the borrowed Cow. ---
        let view = validate_and_align(&req.vector, spec.dim, &mut pooled)
            .map_err(|e| Status::invalid_argument(format!("{e}")))?;

        // --- Simulate a VDB call via await. ---
        tokio::time::sleep(Duration::from_millis(10)).await;

        // --- Use `view` after the await: this is what must compile. ---
        let n2 = l2_norm_squared(&view).map_err(|e| Status::invalid_argument(format!("{e}")))?;

        Ok(Response::new(UpsertResponse {
            point_id: req.point_id,
            processing_us: n2.to_bits() as u64, // we stash the squared norm bit-for-bit
            was_normalized: false,
            vdb_namespace: spec.vdb_namespace,
        }))
    }

    async fn search(
        &self,
        _request: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        Err(Status::unimplemented("hors scope du spike"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cow_survives_await_in_tonic_handler() {
    let mut models = HashMap::new();
    models.insert(
        "test".to_string(),
        ModelSpec {
            dim: 4,
            normalize: false,
            vdb_namespace: "ns-test".to_string(),
        },
    );
    let registry = Arc::new(Registry::new(models));
    let pool = Arc::new(BufferPool::new(2, 1024));

    let service = SpikeService {
        pool: pool.clone(),
        registry: registry.clone(),
    };

    // Test vector: [1, 2, 3, 4], norm² = 1+4+9+16 = 30.
    let floats: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
    let bytes: Vec<u8> = bytemuck::cast_slice(&floats).to_vec();

    let req = Request::new(UpsertRequest {
        model_id: "test".to_string(),
        point_id: "p1".to_string(),
        vector: bytes,
        dim: 4,
        metadata: HashMap::new(),
        producer_id: "lifetime-spike".to_string(),
    });

    let resp = service.upsert(req).await.expect("handler ok");
    let inner = resp.into_inner();

    let n2 = f32::from_bits(inner.processing_us as u32);
    assert!((n2 - 30.0).abs() < 1e-5, "norme² attendue 30, obtenue {n2}");
    assert_eq!(inner.vdb_namespace, "ns-test");

    // The pool must have reclaimed the buffer after the handler.
    assert_eq!(pool.available(), 2);
}
