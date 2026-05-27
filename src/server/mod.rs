//! Serveurs applicatifs : gRPC (`grpc`) pour le trafic métier,
//! HTTP (`http`) pour l'observabilité (health, ready, metrics).

pub mod grpc;
pub mod http;
