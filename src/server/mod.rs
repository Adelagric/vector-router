//! Application servers: gRPC (`grpc`) for business traffic,
//! HTTP (`http`) for observability (health, ready, metrics).

pub mod grpc;
pub mod http;
