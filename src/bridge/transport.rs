//! IM-protocol-agnostic transport abstraction.
//!
//! Bridge's core dispatcher speaks only the types in this module. Each concrete
//! IM protocol (iLink, and future 飞书 / Telegram / …) is an adapter that
//! implements [`Transport`] and translates between its own wire types and these
//! generic DTOs. This is the seam that lets bridge support multiple IMs without
//! the dispatcher depending on any IM's wire protocol.
//!
//! Design notes:
//! - `session_id` / `session_name` / `a2a_call_id` are **bridge-runtime**
//!   fields, not IM-protocol fields. They are first-class on the DTOs because
//!   the dispatcher needs them for routing and CLI session continuity. For the
//!   iLink-via-Hub adapter they are populated from `HubExt`; a direct or non-
//!   iLink adapter populates them from its own conversation identifiers.
//! - `extra` carries IM-private data so the main DTO does not bloat. `raw`
//!   holds the full original message as JSON for diagnostics.
//! - Media upload / typing / read-receipts are NOT modelled yet (Q4/Q5): media
//!   is an optional capability, IM status is deferred.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

pub(crate) mod connection;
pub(crate) mod ilink;

pub use connection::{
    default_auto_client_name, default_local_credential_path, hub_response_token_rejected,
    resolve_hub_connection, validate_hub_token,
};

/// Placeholder transport used when `transport:` names a protocol that has no
/// real adapter yet (stage 2 pluggability proof). Constructing it succeeds
/// (proving the seam loads any adapter), but every inbound poll returns an
/// "not implemented" error so the dispatcher backs off instead of busy-looping.
#[derive(Debug, Clone)]
pub struct NullTransport {
    name: String,
}

impl NullTransport {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

impl Transport for NullTransport {
    fn next_inbound<'a>(
        &'a self,
        _buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>> {
        let name = self.name.clone();
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "transport `{name}` is not implemented yet (stage 2 placeholder)"
            ))
        })
    }

    fn send_reply<'a>(
        &'a self,
        _reply: OutboundReply,
    ) -> BoxFuture<'a, anyhow::Result<SendOutcome>> {
        let name = self.name.clone();
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "transport `{name}` is not implemented yet (stage 2 placeholder)"
            ))
        })
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities::default()
    }
}

/// Outcome of a single inbound poll.
#[derive(Debug)]
pub enum InboundOutcome {
    /// Zero or more new inbound messages. The transport has already advanced
    /// its cursor (`buf`) internally when applicable.
    Messages(Vec<InboundMessage>),
    /// The transport's credential was rejected (401 / revoked). The caller
    /// should re-register or surface a token-rejected stop.
    TokenRejected,
}

/// Result of a `send_reply` call, mirroring the dispatcher's existing semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The IM accepted the reply.
    Sent,
    /// The IM signalled throttling / rate-limit; retry with backoff.
    Throttled { ret: i32, errmsg: Option<String> },
}

/// One media attachment on an inbound message, or to attach to an outbound reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaRef {
    pub kind: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
}

/// A generic inbound IM message.
///
/// Field-for-field covers everything the dispatcher reads today. IM-private
/// data lives in `extra`; the full original message lives in `raw` for debug.
#[derive(Debug, Clone, Default)]
pub struct InboundMessage {
    /// Reply-routing identifier (iLink `context_token`). Required to route a
    /// reply back to the correct conversation.
    pub context_token: Option<String>,
    /// Sender identifier (iLink `from_user_id`).
    pub from_user: Option<String>,
    /// True when the message was produced by a bot/agent rather than a human.
    /// Used for anti-loop filtering (iLink `message_type == 2`).
    pub is_from_bot: bool,
    /// Extracted text body (text item, or voice ASR transcript fallback).
    pub text: Option<String>,
    /// Media attachments carried by the message.
    pub media: Vec<MediaRef>,
    /// Bridge-runtime: the active CLI session id to resume (iLink `HubExt.session_id`).
    pub session_id: Option<String>,
    /// Bridge-runtime: human-readable session name, used as the dispatch key
    /// and echoed on outbound for footer routing (iLink `HubExt.session_name`).
    pub session_name: Option<String>,
    /// Bridge-runtime: A2A call identifier to echo back so Hub can resolve the
    /// MCP `call_agent` waiter (iLink `HubExt.a2a_call_id`).
    pub a2a_call_id: Option<String>,
    /// IM-private extension data (e.g. iLink `MessageItem.extra` fields).
    #[allow(dead_code)]
    pub extra: serde_json::Value,
    /// The full original IM message serialized as JSON, for diagnostics only.
    #[allow(dead_code)]
    pub raw: serde_json::Value,
}

impl InboundMessage {
    /// Non-empty trimmed text, if any.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref().filter(|s| !s.trim().is_empty())
    }
}

/// A generic outbound reply.
///
/// The dispatcher builds one of these for every send path (partial chunk, final
/// reply, A2A reply, cli_session_id persistence, error reply). The transport
/// translates it into its own send-message wire format.
#[derive(Debug, Clone, Default)]
pub struct OutboundReply {
    /// Reply-routing identifier.
    pub context_token: String,
    /// Reply body. May be empty for a session-persist-only send.
    pub text: String,
    /// Recipient (iLink `to_user_id`, derived from inbound `from_user_id`).
    pub to_user: String,
    /// Bridge-runtime: CLI session id to persist on the Hub (iLink `HubExt.cli_session_id`).
    pub cli_session_id: Option<String>,
    /// Bridge-runtime: session name for footer routing (iLink `HubExt.session_name`).
    pub session_name: Option<String>,
    /// Bridge-runtime: A2A call id to echo (iLink `HubExt.a2a_call_id`).
    pub a2a_call_id: Option<String>,
    /// Bridge-runtime: AgentProc usage stats to persist (iLink `HubExt.usage`).
    pub usage: Option<serde_json::Value>,
}

/// Transport-level capability flags. Stage 1 only declares media upload; IM
/// status (typing / read / revoke) is deferred (Q5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportCapabilities {
    /// Whether the transport can upload media for outbound replies.
    pub media_upload: bool,
}

/// IM-protocol-agnostic transport. One implementation per IM protocol.
///
/// `next_inbound` advances the long-poll cursor in `buf` for transports that
/// use one; transports without a cursor (e.g. webhook-driven) may ignore it.
///
/// Methods return boxed futures so the trait is object-safe and the futures
/// are `Send` — the dispatcher spawns tasks that hold a `dyn Transport`.
pub trait Transport: Send + Sync {
    /// Pull the next batch of inbound messages. Updates `buf` in place when the
    /// transport uses a cursor. The returned future borrows both `self` and
    /// `buf` for the same lifetime `'a`.
    fn next_inbound<'a>(
        &'a self,
        buf: &'a mut String,
    ) -> BoxFuture<'a, anyhow::Result<InboundOutcome>>;

    /// Send one reply. Returns the typed send outcome so callers can retry on
    /// throttling within a bounded budget.
    fn send_reply<'a>(&'a self, reply: OutboundReply)
        -> BoxFuture<'a, anyhow::Result<SendOutcome>>;

    /// Declare this transport's optional capabilities.
    fn capabilities(&self) -> TransportCapabilities;
}

pub use ilink::IlinkTransport;
