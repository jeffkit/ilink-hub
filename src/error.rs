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
        // Prefer downcasting to the owned value so we preserve the original
        // sqlx::Error variant (unique constraint, not-found, etc.) rather than
        // collapsing it into a string-only Protocol error. downcast() consumes
        // `e` on success and returns it back on failure, so chain the fallback
        // via the Err branch.
        match e.downcast::<sqlx::Error>() {
            Ok(db_err) => HubError::Database(db_err),
            Err(e) => HubError::Upstream(e.to_string()),
        }
    }
}
