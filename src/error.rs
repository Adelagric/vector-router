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
    #[error("unknown model: {model_id}")]
    UnknownModel { model_id: String },

    #[error("invalid dimension: expected {expected}, got {got}")]
    InvalidDim { expected: usize, got: usize },

    #[error("vector contains a non-finite value (NaN or Inf)")]
    InvalidNumeric,

    #[error("vector database error: {0}")]
    Vdb(String),

    #[error("configuration error: {0}")]
    Config(Box<figment::Error>),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("configuration validation: {0}")]
    Validation(String),

    #[error("telemetry error: {0}")]
    Telemetry(String),

    #[error("service error: {0}")]
    Service(String),
}

impl From<figment::Error> for Error {
    fn from(e: figment::Error) -> Self {
        Error::Config(Box::new(e))
    }
}
