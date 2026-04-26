"""
Client gRPC Python pour vector-router.

Démontre :
  - chargement runtime du proto via grpcio-tools (pas de codegen pré-build)
  - encodage f32 little-endian d'un vecteur 1536-dim en `bytes` protobuf
  - appels Upsert et Search avec un producer_id (pour tracer l'origine côté
    Prometheus du middleware)
  - gestion des erreurs de validation (NaN, dimension fausse, modèle inconnu)
    qui remontent en grpc.StatusCode.INVALID_ARGUMENT

Usage :
    pip install -r requirements.txt
    python client.py            # demo end-to-end contre localhost:50051
"""

from __future__ import annotations

import math
import os
import struct
import sys
import uuid
from pathlib import Path

import grpc
from grpc_tools import protoc


# ---------------------------------------------------------------------------
# Compilation runtime du .proto — pas de stubs versionnés à committer.
# ---------------------------------------------------------------------------

PROTO_ROOT = Path(__file__).resolve().parents[3] / "proto"
PROTO_FILE = PROTO_ROOT / "vector_router" / "v1" / "router.proto"
GENERATED_DIR = Path(__file__).resolve().parent / "_generated"


def ensure_stubs() -> None:
    """Compile le .proto dans _generated/ si absent ou plus vieux que la source."""
    GENERATED_DIR.mkdir(exist_ok=True)
    (GENERATED_DIR / "__init__.py").touch(exist_ok=True)

    pb2 = GENERATED_DIR / "router_pb2.py"
    if pb2.exists() and pb2.stat().st_mtime >= PROTO_FILE.stat().st_mtime:
        return

    args = [
        "protoc",
        f"--proto_path={PROTO_ROOT}",
        f"--python_out={GENERATED_DIR}",
        f"--grpc_python_out={GENERATED_DIR}",
        str(PROTO_FILE),
    ]
    rc = protoc.main(args)
    if rc != 0:
        raise RuntimeError(f"protoc a échoué (code {rc})")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def pack_f32_le(values: list[float]) -> bytes:
    """Encode une liste de f32 en bytes little-endian (format wire attendu)."""
    return struct.pack(f"<{len(values)}f", *values)


def make_demo_vector(dim: int = 1536, seed: int = 42) -> list[float]:
    """Vecteur déterministe pour reproductibilité ; pas une vraie embedding."""
    import random
    rng = random.Random(seed)
    return [rng.gauss(0.0, 1.0) for _ in range(dim)]


# ---------------------------------------------------------------------------
# Demo
# ---------------------------------------------------------------------------


def main() -> int:
    ensure_stubs()
    sys.path.insert(0, str(GENERATED_DIR))
    sys.path.insert(0, str(GENERATED_DIR / "vector_router" / "v1"))

    # imports tardifs : générés à l'instant
    from vector_router.v1 import router_pb2, router_pb2_grpc  # noqa: E402

    addr = os.environ.get("VR_ADDR", "localhost:50051")
    model = os.environ.get("VR_MODEL", "openai-text-embedding-3-small")
    producer = os.environ.get("VR_PRODUCER", "python-client")

    with grpc.insecure_channel(addr) as channel:
        stub = router_pb2_grpc.VectorRouterStub(channel)
        vec = make_demo_vector(1536)

        # 1. Upsert OK
        req = router_pb2.UpsertRequest(
            model_id=model,
            point_id=str(uuid.uuid4()),
            vector=pack_f32_le(vec),
            dim=len(vec),
            producer_id=producer,
        )
        try:
            resp = stub.Upsert(req, timeout=2.0)
            print(f"[OK] Upsert → namespace={resp.vdb_namespace}, "
                  f"normalized={resp.was_normalized}, "
                  f"processing_us={resp.processing_us}")
        except grpc.RpcError as e:
            print(f"[KO] Upsert: {e.code().name}: {e.details()}")

        # 2. Upsert avec NaN — doit être rejeté avec INVALID_ARGUMENT
        bad = list(vec)
        bad[42] = math.nan
        req_nan = router_pb2.UpsertRequest(
            model_id=model,
            point_id=str(uuid.uuid4()),
            vector=pack_f32_le(bad),
            dim=len(bad),
            producer_id=producer,
        )
        try:
            stub.Upsert(req_nan, timeout=2.0)
            print("[KO] Upsert NaN aurait dû être rejeté")
            return 1
        except grpc.RpcError as e:
            assert e.code() == grpc.StatusCode.INVALID_ARGUMENT, e
            print(f"[OK] NaN rejeté côté router : {e.details()}")

        # 3. Search avec le même pipeline de validation
        req_search = router_pb2.SearchRequest(
            model_id=model,
            vector=pack_f32_le(vec),
            dim=len(vec),
            limit=5,
            producer_id=producer,
        )
        try:
            resp = stub.Search(req_search, timeout=2.0)
            print(f"[OK] Search → {len(resp.hits)} hits, "
                  f"namespace={resp.vdb_namespace}")
        except grpc.RpcError as e:
            print(f"[INFO] Search : {e.code().name}: {e.details()}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
