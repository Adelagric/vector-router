/**
 * Client gRPC TypeScript pour vector-router.
 *
 * Démontre :
 *  - chargement runtime du .proto via @grpc/proto-loader (pas de codegen)
 *  - encodage f32 little-endian d'un vecteur 1536-dim en `Buffer` protobuf
 *  - appels Upsert et Search avec un producer_id (pour tracer l'origine
 *    côté Prometheus du middleware)
 *  - gestion des erreurs de validation (NaN, dimension fausse, modèle
 *    inconnu) qui remontent en grpc.status.INVALID_ARGUMENT
 *
 * Usage :
 *    npm install
 *    npm run demo               # demo end-to-end contre localhost:50051
 *
 * Variables d'env :
 *    VR_ADDR       (default: localhost:50051)
 *    VR_MODEL      (default: openai-text-embedding-3-small)
 *    VR_PRODUCER   (default: ts-client)
 */

import { credentials, loadPackageDefinition, type ServiceError, status } from "@grpc/grpc-js";
import { loadSync } from "@grpc/proto-loader";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Encode un Float32Array en Buffer little-endian (format wire `bytes`). */
function packF32LE(values: Float32Array): Buffer {
  const buf = Buffer.alloc(values.length * 4);
  for (let i = 0; i < values.length; i++) {
    buf.writeFloatLE(values[i], i * 4);
  }
  return buf;
}

/** Vecteur déterministe pour reproductibilité ; pas une vraie embedding. */
function makeDemoVector(dim = 1536, seed = 42): Float32Array {
  // PRNG mulberry32 : déterministe, pas crypto.
  let s = seed >>> 0;
  const v = new Float32Array(dim);
  for (let i = 0; i < dim; i++) {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    v[i] = ((t ^ (t >>> 14)) >>> 0) / 4294967296 - 0.5;
  }
  return v;
}

// ---------------------------------------------------------------------------
// Chargement du proto
// ---------------------------------------------------------------------------

const __dirname = dirname(fileURLToPath(import.meta.url));
const PROTO_PATH = resolve(__dirname, "../../../proto/vector_router/v1/router.proto");
const PROTO_INCLUDE = resolve(__dirname, "../../../proto");

const def = loadSync(PROTO_PATH, {
  keepCase: false,
  longs: String,
  enums: String,
  defaults: true,
  oneofs: true,
  includeDirs: [PROTO_INCLUDE],
});

// Le service est sous vector_router.v1.VectorRouter dans le proto.
// loadPackageDefinition typé en `any` côté grpc-js : on encapsule.
type UpsertRequest = {
  model_id: string;
  point_id: string;
  vector: Buffer;
  dim: number;
  metadata?: Record<string, string>;
  producer_id?: string;
};
type UpsertResponse = {
  pointId: string;
  processingUs: string;
  wasNormalized: boolean;
  vdbNamespace: string;
};
type SearchRequest = {
  model_id: string;
  vector: Buffer;
  dim: number;
  limit: number;
  score_threshold?: number;
  metadata_filter?: Record<string, string>;
  producer_id?: string;
};
type SearchHit = { pointId: string; score: number; metadata: Record<string, string> };
type SearchResponse = { hits: SearchHit[]; processingUs: string; vdbNamespace: string };

interface VectorRouterClient {
  Upsert(req: UpsertRequest, cb: (err: ServiceError | null, resp: UpsertResponse) => void): void;
  Search(req: SearchRequest, cb: (err: ServiceError | null, resp: SearchResponse) => void): void;
  close(): void;
}

const proto = loadPackageDefinition(def) as unknown as {
  vector_router: { v1: { VectorRouter: new (addr: string, creds: ReturnType<typeof credentials.createInsecure>) => VectorRouterClient } };
};

// Promisify
function upsert(client: VectorRouterClient, req: UpsertRequest): Promise<UpsertResponse> {
  return new Promise((res, rej) => client.Upsert(req, (err, r) => (err ? rej(err) : res(r))));
}
function search(client: VectorRouterClient, req: SearchRequest): Promise<SearchResponse> {
  return new Promise((res, rej) => client.Search(req, (err, r) => (err ? rej(err) : res(r))));
}

// ---------------------------------------------------------------------------
// Demo
// ---------------------------------------------------------------------------

async function main() {
  const addr = process.env.VR_ADDR ?? "localhost:50051";
  const model = process.env.VR_MODEL ?? "openai-text-embedding-3-small";
  const producer = process.env.VR_PRODUCER ?? "ts-client";

  const client = new proto.vector_router.v1.VectorRouter(addr, credentials.createInsecure());
  const vec = makeDemoVector(1536);

  // 1. Upsert OK
  try {
    const r = await upsert(client, {
      model_id: model,
      point_id: randomUUID(),
      vector: packF32LE(vec),
      dim: vec.length,
      producer_id: producer,
    });
    console.log(`[OK] Upsert → namespace=${r.vdbNamespace}, normalized=${r.wasNormalized}, processing_us=${r.processingUs}`);
  } catch (e) {
    const err = e as ServiceError;
    console.log(`[KO] Upsert: ${err.code} ${err.details}`);
  }

  // 2. Upsert avec NaN — doit être rejeté avec INVALID_ARGUMENT
  const bad = new Float32Array(vec);
  bad[42] = NaN;
  try {
    await upsert(client, {
      model_id: model,
      point_id: randomUUID(),
      vector: packF32LE(bad),
      dim: bad.length,
      producer_id: producer,
    });
    console.log("[KO] Upsert NaN aurait dû être rejeté");
    process.exit(1);
  } catch (e) {
    const err = e as ServiceError;
    if (err.code !== status.INVALID_ARGUMENT) throw e;
    console.log(`[OK] NaN rejeté côté router : ${err.details}`);
  }

  // 3. Search avec le même pipeline de validation
  try {
    const r = await search(client, {
      model_id: model,
      vector: packF32LE(vec),
      dim: vec.length,
      limit: 5,
      producer_id: producer,
    });
    console.log(`[OK] Search → ${r.hits.length} hits, namespace=${r.vdbNamespace}`);
  } catch (e) {
    const err = e as ServiceError;
    console.log(`[INFO] Search : ${err.code} ${err.details}`);
  }

  client.close();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
