use thiserror::Error;

/// Errors raised by this crate.
///
/// The processing variants (UnknownModel, InvalidDim, InvalidNumeric, Vdb)
/// correspond to a `status` label on the `requests_total` metric.
/// The startup variants (Config, Io, Validation) never touch the hot path.
///
/// Note: `figment::Error` is boxed because it weighs ~200 bytes and would
/// bloat every `Result<_, Error>` on the hot path (clippy::result_large_err).
#[derive(Error, Debug)]
pub enum Error {
    #[error("modèle inconnu : {model_id}")]
    UnknownModel { model_id: String },

    #[error("dimension invalide : attendu {expected}, reçu {got}")]
    InvalidDim { expected: usize, got: usize },

    #[error("vecteur contient une valeur non finie (NaN ou Inf)")]
    InvalidNumeric,

    #[error("erreur base vectorielle : {0}")]
    Vdb(String),

    #[error("erreur de configuration : {0}")]
    Config(Box<figment::Error>),

    #[error("erreur I/O : {0}")]
    Io(#[from] std::io::Error),

    #[error("validation de configuration : {0}")]
    Validation(String),

    #[error("erreur télémétrie : {0}")]
    Telemetry(String),

    #[error("erreur service : {0}")]
    Service(String),
}

impl From<figment::Error> for Error {
    fn from(e: figment::Error) -> Self {
        Error::Config(Box::new(e))
    }
}
