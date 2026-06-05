use thiserror::Error;

#[derive(Debug, Error)]
pub enum HubError {
    #[error("iLink upstream error: {0}")]
    Upstream(#[from] anyhow::Error),

    #[error("client not found: {0}")]
    ClientNotFound(String),

    #[error("invalid token")]
    InvalidToken,

    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Queue backend operation failed
    #[error("queue backend error: {0}")]
    QueueBackend(String),
}
