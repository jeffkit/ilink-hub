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

    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Queue backend operation failed.
    #[error("queue backend error: {0}")]
    QueueBackend(String),
}

impl From<anyhow::Error> for HubError {
    fn from(e: anyhow::Error) -> Self {
        HubError::Upstream(e.to_string())
    }
}
