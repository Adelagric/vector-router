//! Shim minimal de l'enum `Error` utilisé par les modules `math` et `pool`,
//! fourni pour permettre la compilation et l'exécution des tests publics dans
//! ce repo de vitrine.
//!
//! Le crate complet expose une variante d'`Error` plus riche (contextes
//! additionnels, conversions `tonic::Status`, gestion des erreurs VDB, etc.) —
//! non incluse ici par périmètre.

#[derive(Debug)]
pub enum Error {
    InvalidDim { expected: usize, got: usize },
    InvalidNumeric,
    Validation(String),
}
