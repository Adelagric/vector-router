"""
Python gRPC client for vector-router.

Demonstrates:
  - runtime loading of the proto via grpcio-tools (no pre-build codegen)
  - little-endian f32 encoding of a 1536-dim vector into protobuf `bytes`
  - Upsert and Search calls with a producer_id (to trace origin on the
    middleware's Prometheus side)
  - handling of validation errors (NaN, wrong dimension, unknown model)
    which surface as grpc.StatusCode.INVALID_ARGUMENT

Usage:
    pip install -r requirements.txt
    python client.py            # end-to-end demo against localhost:50051
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
# Runtime compilation of the .proto — no versioned stubs to commit.
# ---------------------------------------------------------------------------

PROTO_ROOT = Path(__file__).resolve().parents[3] / "proto"
PROTO_FILE = PROTO_ROOT / "vector_router" / "v1" / "router.proto"
GENERATED_DIR = Path(__file__).resolve().parent / "_generated"


def ensure_stubs() -> None:
    """Compile the .proto into _generated/ if missing or older than the source."""
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
    """Encode a list of f32 into little-endian bytes (expected wire format)."""
    return struct.pack(f"<{len(values)}f", *values)


def make_demo_vector(dim: int = 1536, seed: int = 42) -> list[float]:
    """Deterministic vector for reproducibility; not a real embedding."""
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

    # Late imports: generated just now
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

        # 2. Upsert with NaN — must be rejected with INVALID_ARGUMENT
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

        # 3. Search through the same validation pipeline
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
