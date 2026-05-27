//! Spike technique : vérifier que le pattern `Cow<'a, [f32]>` issu du pool
//! reste utilisable après un `.await` dans un handler Tonic.
//!
//! Ce fichier n'est PAS un test fonctionnel du service — c'est une preuve de
//! compilation et une exécution minimale qui valide la compatibilité des
//! lifetimes entre le pool, `validate_and_align`, et le runtime async tokio.
//!
//! Critère de succès :
//! - Le code compile (contraintes Send satisfaites sur la future du handler).
//! - Le test s'exécute sans panique.
//! - Le résultat numérique est correct après l'await.

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

        // --- Le cœur du test : obtention du Cow emprunté. ---
        let view = validate_and_align(&req.vector, spec.dim, &mut pooled)
            .map_err(|e| Status::invalid_argument(format!("{e}")))?;

        // --- Simulation d'un appel VDB via await. ---
        tokio::time::sleep(Duration::from_millis(10)).await;

        // --- Utilisation de view après l'await : c'est ce qui doit compiler. ---
        let n2 = l2_norm_squared(&view).map_err(|e| Status::invalid_argument(format!("{e}")))?;

        Ok(Response::new(UpsertResponse {
            point_id: req.point_id,
            processing_us: n2.to_bits() as u64, // on stocke la norme² bit-for-bit
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

    // Vecteur test : [1, 2, 3, 4], norme² = 1+4+9+16 = 30.
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

    // Le pool doit avoir récupéré le buffer après le handler.
    assert_eq!(pool.available(), 2);
}
