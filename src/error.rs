use thiserror::Error;

/// Erreurs remontées par le crate.
///
/// Les variantes de traitement (UnknownModel, InvalidDim, InvalidNumeric, Vdb)
/// correspondent à un label `status` de la métrique `requests_total`.
/// Les variantes de démarrage (Config, Io, Validation) ne touchent pas
/// le chemin chaud.
///
/// Note : `figment::Error` est boxé car il pèse ~200 octets et ferait grossir
/// tout `Result<_, Error>` sur le chemin chaud (clippy::result_large_err).
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
