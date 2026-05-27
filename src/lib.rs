//! Middleware de routage vectoriel : bibliothèque interne.
//!
//! Le binaire `vector-router` consomme cette bibliothèque ; la séparation
//! évite que le code soit marqué mort tant qu'il n'a pas de consommateur final.

pub mod client;
pub mod config;
pub mod error;
pub mod math;
pub mod pool;
pub mod proto;
pub mod registry;
pub mod server;
pub mod service;
pub mod telemetry;
