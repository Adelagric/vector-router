//! Crate de vérification pour le repo vitrine vector-router.
//!
//! Ce crate sert UNIQUEMENT à compiler, tester et linter les modules publics
//! exposés dans `samples/` (math, pool). Le code complet du produit (registre,
//! service gRPC, client VDB, licence, etc.) n'est PAS inclus ici — voir
//! LICENSE et `Licensing` dans le README.
//!
//! La CI exécute sur ce crate :
//!   - `cargo fmt --check`
//!   - `cargo clippy --all-targets -- -D warnings`
//!   - `cargo test --all-targets`
//!
//! Les ~20 tests embarqués dans `samples/math.rs` et `samples/pool.rs`
//! couvrent : alignement mémoire, propriétés numériques (NaN/Inf, normalisation),
//! pool RAII, comportement du pool sous charge concurrente.

#![deny(unsafe_code)]

pub mod error;

#[path = "../samples/math.rs"]
pub mod math;

#[path = "../samples/pool.rs"]
pub mod pool;
