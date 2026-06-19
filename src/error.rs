use thiserror::Error;

#[derive(Debug, Error)]
pub enum HubError {
    /// HTTP-level failure communicating with the iLink upstream.
    #[error("iLink upstream HTTP error {code}: {msg}")]
    UpstreamHttp { code: u16, msg: String },

    /// Response from the iLink upstream could not be parsed.
    #[error("iLink upstream parse error: {0}")]
    UpstreamParse(String),

    /// Generic upstream error — kept for third-party MessageQueue implementations
    /// that may not have a more specific variant available.
    #[error("iLink upstream error: {0}")]
    Upstream(String),

    #[error("client not found: {0}")]
    ClientNotFound(String),

    #[error("invalid token")]
    InvalidToken,

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("operation timed out")]
    Timeout,

    /// Queue backend operation failed.
    #[error("queue backend error: {0}")]
    QueueBackend(String),
}

impl From<anyhow::Error> for HubError {
    fn from(e: anyhow::Error) -> Self {
        // Attempt to downcast to known typed errors before falling back to string.
        if let Some(db_err) = e.downcast_ref::<sqlx::Error>() {
            return HubError::Upstream(format!("database: {db_err}"));
        }
        HubError::Upstream(e.to_string())
    }
}
