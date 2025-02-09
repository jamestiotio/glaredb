#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("Invalid number of arguments.")]
    InvalidNumArgs,

    #[error("Expected argument at index {index}: {what}")]
    ExpectedIndexedArgument { index: usize, what: String },

    #[error("{0}")]
    String(String),

    #[error("Unable to find {obj_typ}: '{name}'")]
    MissingObject { obj_typ: &'static str, name: String },

    #[error("Missing named argument: '{0}'")]
    MissingNamedArgument(&'static str),

    #[error("Invalid parameter value {param}, expected a {expected}")]
    InvalidParamValue {
        param: String,
        expected: &'static str,
    },

    #[error(transparent)]
    Access(Box<dyn std::error::Error + Send + Sync>),

    #[error(transparent)]
    DataFusion(#[from] datafusion::error::DataFusionError),

    #[error(transparent)]
    Arrow(#[from] datafusion::arrow::error::ArrowError),

    #[error(transparent)]
    DecimalError(#[from] decimal::DecimalError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("Unimplemented: {0}")]
    Unimplemented(&'static str),

    #[error(transparent)]
    ListingErrBoxed(#[from] Box<dyn std::error::Error + Sync + Send>),

    #[error("object store: {0}")]
    ObjectStore(String),
}

impl ExtensionError {
    pub fn access<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Access(Box::new(err))
    }
}

pub type Result<T, E = ExtensionError> = std::result::Result<T, E>;
