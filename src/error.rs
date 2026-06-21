use thiserror::Error;

#[derive(Debug, Error)]
pub enum HubError {
    /// HTTP-level failure communicating with the iLink upstream.
    #[error("iLink upstream HTTP error {status}: {msg}")]
    UpstreamHttp { status: u16, msg: String },

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
        // First, try to recover a `HubError` that was wrapped at an upstream
        // call site via `anyhow::Error::new(HubError::UpstreamHttp { ... })` or
        // `HubError::UpstreamParse(...)`. This lets N-06 specific variants
        // survive a round-trip through `anyhow::Result` and still be
        // pattern-matched by downstream consumers (e.g. to distinguish a
        // transient HTTP 503 from a malformed JSON body).
        //
        // `downcast()` consumes `e` and returns the inner `T` on success, or
        // hands `e` back on failure, so the fallback chain stays in scope.
        match e.downcast::<HubError>() {
            Ok(hub_err) => hub_err,
            Err(e) => match e.downcast::<sqlx::Error>() {
                Ok(db_err) => HubError::Database(db_err),
                Err(e) => HubError::Upstream(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N-06: UpstreamHttp Display includes both the status code and the message.
    /// Without this, downstream log lines and JSON error responses would lose
    /// the status code that distinguishes a transient 503 from a permanent 4xx.
    #[test]
    fn upstream_http_display_includes_status_and_msg() {
        let err = HubError::UpstreamHttp {
            status: 503,
            msg: "service unavailable".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("503"), "status missing from Display: {s}");
        assert!(
            s.contains("service unavailable"),
            "msg missing from Display: {s}"
        );
    }

    /// N-06: UpstreamParse Display includes the parse error string. Callers
    /// pattern-match on the variant; Display is for logs / HTTP error bodies.
    #[test]
    fn upstream_parse_display_includes_message() {
        let err = HubError::UpstreamParse("unexpected token at line 3".to_string());
        let s = err.to_string();
        assert!(
            s.contains("unexpected token at line 3"),
            "msg missing from Display: {s}"
        );
    }

    /// N-06 contract: when an upstream call site wraps UpstreamHttp in
    /// `anyhow::Error::new(...)` and that error propagates through `?` to a
    /// HubError consumer, the `From<anyhow::Error>` impl downcasts and
    /// recovers the specific variant. This is the load-bearing invariant for
    /// the migration in `ilink/upstream.rs` and `ilink/login.rs`.
    #[test]
    fn from_anyhow_preserves_upstream_http_via_downcast() {
        let original = HubError::UpstreamHttp {
            status: 429,
            msg: "rate limited".to_string(),
        };
        let wrapped: anyhow::Error = anyhow::Error::new(original);
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::UpstreamHttp { status, msg } => {
                assert_eq!(status, 429);
                assert_eq!(msg, "rate limited");
            }
            other => panic!("expected UpstreamHttp, got {other:?}"),
        }
    }

    /// N-06 contract: same as above for the parse variant.
    #[test]
    fn from_anyhow_preserves_upstream_parse_via_downcast() {
        let original = HubError::UpstreamParse("bad json".to_string());
        let wrapped: anyhow::Error = anyhow::Error::new(original);
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::UpstreamParse(msg) => assert_eq!(msg, "bad json"),
            other => panic!("expected UpstreamParse, got {other:?}"),
        }
    }

    /// Regression guard: an anyhow::Error that does NOT wrap a HubError or a
    /// sqlx::Error still falls through to the legacy Upstream(string) variant
    /// so existing call sites that build plain anyhow::Error values (e.g. via
    /// `anyhow!(...)` macros elsewhere in the codebase) keep working.
    #[test]
    fn from_anyhow_collapses_other_errors_to_upstream_string() {
        let wrapped: anyhow::Error = anyhow::anyhow!("raw anyhow message");
        let recovered: HubError = wrapped.into();
        match recovered {
            HubError::Upstream(s) => assert_eq!(s, "raw anyhow message"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    /// `upstream_http_err` maps transport-level reqwest failures (DNS, TLS,
    /// connection reset) — where `e.status()` is None — to `status: 0`.
    /// Verify the variant accepts 0 without panicking on Display so
    /// downstream log lines and JSON error bodies always have something
    /// parseable to render.
    #[test]
    fn upstream_http_status_zero_is_legal_for_pre_send_failures() {
        let err = HubError::UpstreamHttp {
            status: 0,
            msg: "connection refused".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("connection refused"), "msg missing: {s}");
    }
}
