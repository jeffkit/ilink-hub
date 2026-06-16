//! End-to-end tests that simulate the full user → Hub → bridge → Hub → user
//! loop without a real WeChat/iLink backend or a real AI model.
//!
//! What is mocked and what is real:
//!
//! - **WeChat side** (upstream): the `UpstreamSink` trait is implemented by
//!   `MockUpstream`, which records every outbound `sendmessage` and replays
//!   it for assertion. No network call leaves the test process.
//! - **AI side** (downstream): the test itself plays the bridge role — it
//!   calls the Hub's `getupdates` long-poll, picks up the dispatched message,
//!   composes a synthetic "AI" reply, and posts it back via `sendmessage`.
//!   This lets us cover session continuity, vctx→real_ctx translation, and
//!   the outbound-origin footer without a model under test.
//! - **Hub core** (`dispatcher`, `router`, `queue`, persistence, all
//!   axum routes) is the real production code, brought up against a bound
//!   TcpListener on a random port and a `sqlite::memory:` store.

use std::sync::Arc;
use std::time::Duration;

// Pick a non-empty admin token up front and install it into the process env
// before any Hub code reads it. The token is read once via `OnceLock` in
// `check_admin_auth`, so we MUST set this before the first call into
// `register`. Using a fixed string keeps `register_client`'s `Bearer …` header
// and the server's expected value in sync.
const ADMIN_TOKEN: &str = "e2e-admin-token";
static ENV_INSTALLED: std::sync::Once = std::sync::Once::new();

fn install_test_env() {
    ENV_INSTALLED.call_once(|| {
        // `set_var` is marked unsafe in recent Rust editions because concurrent
        // readers can race writers; in tests we accept that — no other test
        // cares about this variable.
        unsafe {
            std::env::set_var("ILINK_ADMIN_TOKEN", ADMIN_TOKEN);
        }
    });
}

use async_trait::async_trait;
use axum::http::{header, StatusCode};
use ilink_hub::ilink::types::{
    BaseInfo, GetConfigRequest, GetConfigResponse, GetUploadUrlRequest, GetUploadUrlResponse,
    MessageItem, SendMessageRequest, SendMessageResponse, SendTypingRequest, TextItem,
    WeixinMessage,
};
use ilink_hub::ilink::UpstreamSink;
use ilink_hub::store::Store;
use ilink_hub::{hub::HubState, server, InMemoryQueue};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};

// ─── MockUpstream ────────────────────────────────────────────────────────────

/// Records every `sendmessage` call. `sent_messages()` returns a snapshot of
/// the recorded list — assertions can read it after the bridge has driven the
/// flow.
#[derive(Default)]
struct MockUpstream {
    sent: Mutex<Vec<SendMessageRequest>>,
}

impl MockUpstream {
    async fn sent_messages(&self) -> Vec<SendMessageRequest> {
        // SendMessageRequest doesn't derive Clone, so we drain-and-replay.
        let mut guard = self.sent.lock().await;
        let mut out = Vec::with_capacity(guard.len());
        while let Some(m) = guard.pop() {
            out.push(m);
        }
        out.reverse();
        out
    }
}

#[async_trait]
impl UpstreamSink for MockUpstream {
    async fn notify_start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send_message(&self, req: SendMessageRequest) -> anyhow::Result<SendMessageResponse> {
        self.sent.lock().await.push(req);
        Ok(SendMessageResponse::ok())
    }
    async fn send_typing(&self, _req: SendTypingRequest) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_config(&self, _req: GetConfigRequest) -> anyhow::Result<GetConfigResponse> {
        Ok(GetConfigResponse::default())
    }
    async fn get_upload_url(
        &self,
        _req: GetUploadUrlRequest,
    ) -> anyhow::Result<GetUploadUrlResponse> {
        Ok(GetUploadUrlResponse {
            ret: 0,
            upload_url: None,
            media_id: None,
            errmsg: None,
        })
    }
    fn polls_ok(&self) -> u64 {
        0
    }
    fn polls_err(&self) -> u64 {
        0
    }
    fn relogin_attempts(&self) -> u64 {
        0
    }
}

// ─── Test harness ────────────────────────────────────────────────────────────

struct Harness {
    base_url: String,
    state: Arc<HubState>,
    mock: Arc<MockUpstream>,
    _dispatch_tx: broadcast::Sender<WeixinMessage>,
    /// Held so the watch::channel is never closed for the lifetime of the
    /// harness. Closing the sender would let `wait_shutdown_signal` resolve
    /// immediately and short-circuit every long-poll before its timer fires.
    _shutdown_tx: tokio::sync::watch::Sender<bool>,
}

async fn boot() -> Harness {
    install_test_env();
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let mock = Arc::new(MockUpstream::default());
    let queue: Arc<dyn ilink_hub::hub::MessageQueue> = Arc::new(InMemoryQueue::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(mock.clone() as Arc<dyn UpstreamSink>, store, queue, shutdown_rx);

    let router = server::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("read bound addr");
    let base_url = format!("http://{addr}");

    let dispatch_channel_size: usize = 64;
    let (tx, rx) = broadcast::channel::<WeixinMessage>(dispatch_channel_size);
    ilink_hub::hub::spawn_dispatcher(state.clone(), rx);

    let state_for_server = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum serve");
        drop(state_for_server);
    });

    Harness {
        base_url,
        state,
        mock,
        _dispatch_tx: tx,
        _shutdown_tx: shutdown_tx,
    }
}

/// Boot a Hub with no default route pre-set. Used to exercise the broadcast
/// path explicitly — the production `register_client_in_hub` sets the first
/// registered client as default, which would mask the broadcast branch in
/// tests that want to observe fan-out to all online clients.
async fn boot_without_default() -> Harness {
    install_test_env();
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let mock = Arc::new(MockUpstream::default());
    let queue: Arc<dyn ilink_hub::hub::MessageQueue> = Arc::new(InMemoryQueue::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(mock.clone() as Arc<dyn UpstreamSink>, store, queue, shutdown_rx);

    // Override the default route to None — HubState::new starts with None,
    // and the only thing that would set it is the first /register call.
    state.routing.router.lock().await.unset_default();

    let router = server::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("read bound addr");
    let base_url = format!("http://{addr}");

    let dispatch_channel_size: usize = 64;
    let (tx, rx) = broadcast::channel::<WeixinMessage>(dispatch_channel_size);
    ilink_hub::hub::spawn_dispatcher(state.clone(), rx);

    let state_for_server = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum serve");
        drop(state_for_server);
    });

    Harness {
        base_url,
        state,
        mock,
        _dispatch_tx: tx,
        _shutdown_tx: shutdown_tx,
    }
}

fn text_item(s: &str) -> MessageItem {
    MessageItem {
        item_type: Some(ilink_hub::ilink::types::msg_type::TEXT),
        text_item: Some(TextItem {
            text: Some(s.to_string()),
        }),
        voice_item: None,
        extra: serde_json::Value::Object(Default::default()),
    }
}

fn user_text_msg(from_user: &str, real_ctx: &str, text: &str) -> WeixinMessage {
    WeixinMessage {
        message_type: Some(1),
        from_user_id: Some(from_user.to_string()),
        context_token: Some(real_ctx.to_string()),
        item_list: Some(std::sync::Arc::new(vec![text_item(text)])),
        ..Default::default()
    }
}

async fn register_client(base_url: &str, name: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/hub/register"))
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .json(&json!({ "name": name }))
        .send()
        .await
        .expect("register http");
    assert_eq!(resp.status(), StatusCode::OK, "register should succeed");
    let body: serde_json::Value = resp.json().await.expect("register json");
    body["vtoken"]
        .as_str()
        .expect("vtoken in response")
        .to_string()
}

/// Long-poll for messages on `vtoken`. Returns the first batch of messages
/// received within `poll_secs`, or empty if the timeout fires.
async fn poll_for_messages(
    base_url: &str,
    vtoken: &str,
    poll_secs: u32,
) -> Vec<WeixinMessage> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/ilink/bot/getupdates"))
        .header(header::AUTHORIZATION, format!("Bearer {vtoken}"))
        .json(&json!({ "timeout": poll_secs, "base_info": BaseInfo::default() }))
        .send()
        .await
        .expect("getupdates http");
    assert_eq!(resp.status(), StatusCode::OK, "getupdates should succeed");
    let body: serde_json::Value = resp.json().await.expect("getupdates json");
    if let Some(msgs) = body.get("msgs").and_then(|m| m.as_array()) {
        msgs.iter()
            .map(|m| serde_json::from_value(m.clone()).expect("decode msg"))
            .collect()
    } else {
        Vec::new()
    }
}

/// Drive `sendmessage` as if a bridge were forwarding the AI's reply to the
/// Hub. The Hub will translate the vctx back to a real_ctx and dispatch to
/// the mock upstream.
async fn bridge_send(
    base_url: &str,
    vtoken: &str,
    vctx: &str,
    to_user: &str,
    text: &str,
) -> serde_json::Value {
    bridge_send_with_session(base_url, vtoken, vctx, to_user, text, None, None).await
}

/// Like `bridge_send` but lets the test pass a `session_name` and a
/// `cli_session_id` in `ilink_hub_ext`. The latter is what the bridge would
/// forward after the AI model assigns it a UUID (e.g. Claude `--resume`).
async fn bridge_send_with_session(
    base_url: &str,
    vtoken: &str,
    vctx: &str,
    to_user: &str,
    text: &str,
    session_name: Option<&str>,
    cli_session_id: Option<&str>,
) -> serde_json::Value {
    let mut hub_ext = serde_json::Map::new();
    if let Some(s) = session_name {
        hub_ext.insert("session_name".into(), json!(s));
    }
    if let Some(s) = cli_session_id {
        hub_ext.insert("cli_session_id".into(), json!(s));
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_url}/ilink/bot/sendmessage"))
        .header(header::AUTHORIZATION, format!("Bearer {vtoken}"))
        .json(&json!({
            "msg": {
                "context_token": vctx,
                "to_user_id": to_user,
                "ilink_hub_ext": serde_json::Value::Object(hub_ext),
                "item_list": [{
                    "type": 1,
                    "text_item": { "text": text },
                }],
            },
            "base_info": BaseInfo::default(),
        }))
        .send()
        .await
        .expect("sendmessage http");
    assert_eq!(resp.status(), StatusCode::OK, "sendmessage should succeed");
    resp.json().await.expect("sendmessage json")
}

/// Long-poll for *one* message on `vtoken`, with a short timeout. Tests that
/// expect exactly one inbound message use this so they fail loudly if the
/// bridge got zero or two instead.
async fn poll_one(base_url: &str, vtoken: &str, poll_secs: u32) -> WeixinMessage {
    let msgs = poll_for_messages(base_url, vtoken, poll_secs).await;
    assert_eq!(
        msgs.len(),
        1,
        "expected exactly one message on vtoken {}, got {}",
        &vtoken[..vtoken.len().min(8)],
        msgs.len()
    );
    msgs.into_iter().next().unwrap()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Golden path: a WeChat user types "hello" to the bot, the dispatcher pushes
/// the message into the registered bridge's queue, the bridge polls it, and
/// sends back "hi there" as the AI reply. The Hub must translate the vctx back
/// to the user's real_ctx and forward the reply to the mock upstream — which
/// is what the WeChat user would have received.
#[tokio::test]
async fn user_message_flows_through_dispatcher_to_bridge_and_reply_reaches_upstream() {
    let h = boot().await;
    let vtoken = register_client(&h.base_url, "claude").await;

    // 1) The "WeChat" user sends a message. We bypass the real iLink WebSocket
    //    by injecting straight into the dispatch channel — that is the exact
    //    path `UpstreamClient::run_polling_loop` would use in production.
    let real_ctx = "real-ctx-golden-001";
    let user_msg = user_text_msg("alice@wechat", real_ctx, "hello");
    h._dispatch_tx.send(user_msg).unwrap();

    // 2) The bridge (simulated by this test) long-polls and picks up the
    //    message. The dispatcher rewrites context_token to a vctx — the
    //    bridge must echo that vctx back when sending the AI reply.
    // Give the dispatcher a moment to process the broadcast channel message.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let polled = poll_for_messages(&h.base_url, &vtoken, 2).await;
    assert_eq!(polled.len(), 1, "bridge should receive the message");
    let inbound = &polled[0];
    assert_eq!(inbound.text(), Some("hello"));
    assert_eq!(inbound.from_user_id.as_deref(), Some("alice@wechat"));
    let vctx = inbound
        .context_token
        .as_deref()
        .expect("inbound message has a vctx")
        .to_string();
    assert_ne!(vctx, real_ctx, "vctx must differ from real_ctx");

    // 3) The bridge forwards the AI's reply. We tag ilink_hub_ext.session_name
    //    to drive the round-trip without touching the store directly.
    let _ = bridge_send(
        &h.base_url,
        &vtoken,
        &vctx,
        "alice@wechat",
        "hi there",
    )
    .await;

    // 4) The Hub translates vctx → real_ctx and calls upstream.send_message.
    //    The mock records the call so we can assert against it.
    let sent = h.mock.sent_messages().await;
    assert_eq!(sent.len(), 1, "exactly one outbound message to upstream");
    let out = sent.into_iter().next().unwrap();
    let out_msg = out.msg.expect("sendmessage body carries msg");
    assert_eq!(
        out_msg.context_token.as_deref(),
        Some(real_ctx),
        "upstream must see the real_ctx, not the vctx"
    );
    assert_eq!(out_msg.to_user_id.as_deref(), Some("alice@wechat"));
    assert_eq!(out_msg.text(), Some("hi there"));
    assert_eq!(out_msg.message_type, Some(2), "outbound is a bot message");
}

/// When no bridge is online, a regular user message must NOT crash; the Hub
/// sends a polite no-backend fallback to the user and the mock upstream
/// records it. This is the behaviour that protects users from a misconfigured
/// bridge dropping their messages silently.
#[tokio::test]
async fn no_backend_online_triggers_fallback_reply_to_user() {
    let h = boot().await;
    let real_ctx = "real-ctx-fallback-001";

    h._dispatch_tx
        .send(user_text_msg("bob@wechat", real_ctx, "anyone home?"))
        .unwrap();

    // Wait for the dispatch + fallback to land. 200ms is generous because the
    // fallback path is in-memory and does not call out to any I/O.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let sent = h.mock.sent_messages().await;
    assert_eq!(sent.len(), 1, "fallback reply should reach upstream");
    let out = sent.into_iter().next().unwrap();
    let out_msg = out.msg.expect("fallback carries a msg");
    assert_eq!(out_msg.context_token.as_deref(), Some(real_ctx));
    assert_eq!(out_msg.to_user_id.as_deref(), Some("bob@wechat"));
    let text = out_msg.text().unwrap_or("");
    assert!(
        text.contains("暂无 AI 助手后端在线") || text.contains("iLink Hub"),
        "fallback should explain the situation; got: {text:?}"
    );
}

/// Multiple bridges registered → a message from a user with no per-user route
/// is broadcast to all online bridges. Each bridge's `getupdates` returns
/// exactly one copy.
#[tokio::test]
async fn broadcast_dispatches_to_every_online_bridge() {
    let h = boot_without_default().await;
    let v1 = register_client(&h.base_url, "claude").await;
    let v2 = register_client(&h.base_url, "codex").await;

    // The two /register calls re-set claude as default (the first registration
    // always becomes default). Clear it again so subsequent messages fall
    // through to broadcast instead of being forwarded to claude.
    h.state.routing.router.lock().await.unset_default();

    // The first /register would have set claude as default; the helper
    // already cleared that, so subsequent messages with no per-user route
    // fall through to broadcast. Mark both bridges as online via a no-poll
    // getupdates so broadcast finds them in `online_clients()`.
    let pre_poll = async |v: &str| {
        let _ = poll_for_messages(&h.base_url, v, 0).await;
    };
    pre_poll(&v1).await;
    pre_poll(&v2).await;

    h._dispatch_tx
        .send(user_text_msg("carol@wechat", "real-ctx-bcast", "ping"))
        .unwrap();

    // Give the dispatcher a beat to fan out and write to both queues.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let a = poll_for_messages(&h.base_url, &v1, 1).await;
    let b = poll_for_messages(&h.base_url, &v2, 1).await;
    assert_eq!(a.len(), 1, "claude should receive one message");
    assert_eq!(b.len(), 1, "codex should receive one message");
    assert_eq!(a[0].text(), Some("ping"));
    assert_eq!(b[0].text(), Some("ping"));
}

// ─── Session continuity (cli_session_id round-trip) ──────────────────────────
//
// The bridge reports the AI model's session UUID (e.g. Claude Code's
// `--resume` UUID) back to the Hub via `sendmessage.ilink_hub_ext.cli_session_id`.
// The Hub persists it against the active (vctx, vtoken, session_name) tuple
// and then injects it back into the next ForwardTo message so the bridge can
// `--resume` the same conversation. This is what keeps a multi-turn WeChat
// conversation attached to one Claude session instead of spawning a new one
// every turn.

/// End-to-end: the bridge reports a `cli_session_id` on its first reply;
/// the Hub persists it; the user's next message is forwarded to the bridge
/// with that same UUID in `ilink_hub_ext.session_id`.
#[tokio::test]
async fn cli_session_id_round_trips_through_persistence() {
    let h = boot().await;
    let vtoken = register_client(&h.base_url, "claude").await;
    let user = "alice@wechat";
    let real_ctx = "real-ctx-session-001";
    const CLI_UUID: &str = "claude-uuid-aaaa-bbbb-cccc-dddddddddddd";

    // ── Turn 1 ────────────────────────────────────────────────────────────────
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "first question"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let inbound1 = poll_one(&h.base_url, &vtoken, 2).await;
    assert_eq!(inbound1.text(), Some("first question"));
    let vctx1 = inbound1.context_token.as_deref().unwrap().to_string();

    // The first inbound message has no session_id yet (Hub has no persisted UUID).
    assert!(
        inbound1
            .ilink_hub_ext
            .as_ref()
            .and_then(|e| e.session_id.as_deref())
            .is_none(),
        "first turn has no persisted session_id yet"
    );

    // Bridge replies and reports the Claude `--resume` UUID back to the Hub.
    let _ = bridge_send_with_session(
        &h.base_url,
        &vtoken,
        &vctx1,
        user,
        "first answer",
        Some("default"),
        Some(CLI_UUID),
    )
    .await;

    // Yield so the fire-and-forget persist completes. The store is sqlite::memory:
    // so there's no real I/O, but the task is still on a separate spawn.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Turn 2 ────────────────────────────────────────────────────────────────
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "follow-up"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let inbound2 = poll_one(&h.base_url, &vtoken, 2).await;
    assert_eq!(inbound2.text(), Some("follow-up"));
    // The session_id must round-trip — the bridge needs this to `--resume`.
    let session_id = inbound2
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.as_deref());
    assert_eq!(
        session_id,
        Some(CLI_UUID),
        "Hub should re-inject the persisted cli_session_id into turn 2"
    );
    // Same vctx → same conversation key in the bridge.
    let vctx2 = inbound2.context_token.as_deref().unwrap();
    assert_eq!(vctx2, vctx1, "vctx must stay stable across turns");
}

// ─── Quote-reply routing ────────────────────────────────────────────────────
//
// A user can quote-reply a bot message and the Hub should route the follow-up
// back to the backend that produced the quoted reply, even if the user's
// `/use` default points elsewhere. This is the everyday multi-backend flow:
// user talks to claude, switches default to codex, then quote-replies the
// claude answer to continue that conversation.

/// User quote-replies a message that was produced by backend A while the
/// current `/use` route points at backend B. The Hub must route the quoted
/// reply to A (not B).
#[tokio::test]
async fn quote_reply_routes_back_to_originating_backend() {
    let h = boot_without_default().await;
    let va = register_client(&h.base_url, "claude").await;
    let vb = register_client(&h.base_url, "codex").await;
    let user = "alice@wechat";
    let real_ctx = "real-ctx-quote-001";

    // Both clients must be `online=true` for broadcast to fan out. `register`
    // doesn't mark them online; only a real `getupdates` call does (mark_seen).
    // We short-poll both with timeout=0 to flip them online without consuming
    // the messages we'll want later.
    let _ = poll_for_messages(&h.base_url, &va, 0).await;
    let _ = poll_for_messages(&h.base_url, &vb, 0).await;

    // Drop the Hub-internal default that `register` set on the first client.
    // Now both clients are reachable only via broadcast (no per-user route).
    h.state.routing.router.lock().await.unset_default();

    // ── Turn 1: broadcast a question; claude (and codex) both receive it.
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "what is foo?"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let claude_msgs = poll_for_messages(&h.base_url, &va, 1).await;
    let codex_msgs = poll_for_messages(&h.base_url, &vb, 1).await;
    assert_eq!(claude_msgs.len(), 1, "broadcast must reach claude");
    assert_eq!(codex_msgs.len(), 1, "broadcast must reach codex");
    let claude_vctx = claude_msgs[0].context_token.as_deref().unwrap().to_string();

    // Drain the codex copy so it doesn't bleed into the next assertion. Claude's
    // copy is also drained via poll_for_messages above.
    let _ = poll_for_messages(&h.base_url, &vb, 0).await;

    // ── Claude replies via `sendmessage`. The Hub registers the outbound body
    // (including the origin footer it appends) into the QuoteRouteIndex. The
    // footer is `"\n\n---\nclaude"` because `should_append_outbound_origin_label`
    // returns true with 2+ online clients and no env override.
    let claude_body = "the meaning of foo is 42";
    let _ = bridge_send_with_session(
        &h.base_url,
        &va,
        &claude_vctx,
        user,
        claude_body,
        Some("default"),
        None,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Pin the user's default to codex so we can prove the quote override wins
    // against a non-broadcast base decision (ForwardTo{codex} rather than Broadcast).
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "/use codex"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Discard the /use ack that landed on the mock upstream.
    let _ = h.mock.sent_messages().await;

    // ── Turn 2: user quote-replies claude's answer. The ref_msg must carry
    // exactly the text the Hub indexed — body + appended footer — otherwise
    // content-based matching misses and the message falls through to /use.
    let quoted_registered_text = format!("{claude_body}\n\n---\nclaude");
    let quote_text = "but what about bar?";
    let mut msg = user_text_msg(user, real_ctx, quote_text);
    let items = std::sync::Arc::make_mut(msg.item_list.as_mut().unwrap());
    items[0].extra = json!({
        "ref_msg": {
            "message_item": {
                "type": 1,
                "text_item": { "text": quoted_registered_text },
                "create_time_ms": chrono_millis_now(),
            }
        }
    });
    h._dispatch_tx.send(msg).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Codex (the user's current /use default) must NOT receive the quoted reply.
    let codex_after = poll_for_messages(&h.base_url, &vb, 1).await;
    assert!(
        codex_after.is_empty(),
        "quote-reply should NOT land on codex (current /use default); got {:?}",
        codex_after.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
    // Claude (the origin of the quoted message) MUST receive it.
    let claude_after = poll_for_messages(&h.base_url, &va, 1).await;
    assert_eq!(
        claude_after.len(),
        1,
        "quote-reply must route back to claude"
    );
    assert_eq!(claude_after[0].text(), Some(quote_text));
}

/// Tiny helper: returns `chrono::Utc::now().timestamp_millis()`. We use the
/// real wall clock for `create_time_ms` because the QuoteRouteIndex tiebreaks
/// by closeness to this value when several origins share the same text.
fn chrono_millis_now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ─── Hub commands end-to-end ────────────────────────────────────────────────
//
// The user types `/list`, `/use`, `/session new`, `/session use`, etc. as
// ordinary text messages. The Hub intercepts them, mutates routing/session
// state, and sends a confirmation reply back through the mock upstream.

/// `/list` enumerates registered backends and marks the active one.
#[tokio::test]
async fn hub_command_list_reports_active_backend() {
    let h = boot_without_default().await;
    let _va = register_client(&h.base_url, "claude").await;
    let _vb = register_client(&h.base_url, "codex").await;
    // Unset default so /list clearly marks the *active* one (none) vs *default*.
    h.state.routing.router.lock().await.unset_default();

    let user = "alice@wechat";
    h._dispatch_tx
        .send(user_text_msg(user, "real-ctx-list-001", "/list"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let sent = h.mock.sent_messages().await;
    assert_eq!(sent.len(), 1, "/list should produce one upstream reply");
    let body = sent.into_iter().next().unwrap();
    let text = body.msg.as_ref().and_then(|m| m.text()).unwrap_or("");
    assert!(text.contains("claude"), "/list body should include claude: {text}");
    assert!(text.contains("codex"), "/list body should include codex: {text}");
    assert!(
        text.contains("广播模式") || text.contains("当前未选中"),
        "/list body should note no active backend; got: {text}"
    );
}

/// `/use <name>` switches the user's default backend, and the *next* user
/// message is forwarded to the newly selected backend (proving the route
/// actually flipped, not just that the ack message was sent).
#[tokio::test]
async fn hub_command_use_reroutes_subsequent_messages() {
    let h = boot_without_default().await;
    let va = register_client(&h.base_url, "claude").await;
    let vb = register_client(&h.base_url, "codex").await;
    h.state.routing.router.lock().await.unset_default();

    let user = "alice@wechat";
    let real_ctx = "real-ctx-use-001";

    // /use codex
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "/use codex"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Discard the /use ack message — we care about the *next* user turn.
    let _ = h.mock.sent_messages().await;

    // The user's follow-up should now land in codex's queue, not claude's.
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "hello codex"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let claude_msgs = poll_for_messages(&h.base_url, &va, 1).await;
    let codex_msgs = poll_for_messages(&h.base_url, &vb, 1).await;
    assert!(
        claude_msgs.is_empty(),
        "claude should NOT receive the message after /use codex"
    );
    assert_eq!(
        codex_msgs.len(),
        1,
        "codex should receive the message after /use codex"
    );
    assert_eq!(codex_msgs[0].text(), Some("hello codex"));
}

/// `/session use <name>` persists a per-(vctx, vtoken) active session and
/// causes subsequent inbound messages to carry that session name forward in
/// `ilink_hub_ext.session_name`. The bridge uses this to decide which session
/// to `--resume` on the backend side.
#[tokio::test]
async fn hub_command_session_use_propagates_to_inbound_messages() {
    let h = boot().await;
    let vtoken = register_client(&h.base_url, "claude").await;
    let user = "alice@wechat";
    let real_ctx = "real-ctx-sessionuse-001";

    // Create + activate a named session.
    h._dispatch_tx
        .send(user_text_msg(
            user,
            real_ctx,
            "/session new feature-x some-uuid-zzz",
        ))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = h.mock.sent_messages().await;

    // Now send a normal message; the inbound message on the bridge side must
    // carry session_name = "feature-x".
    h._dispatch_tx
        .send(user_text_msg(user, real_ctx, "work on feature-x please"))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let inbound = poll_one(&h.base_url, &vtoken, 2).await;

    let session_name = inbound
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref());
    assert_eq!(
        session_name,
        Some("feature-x"),
        "inbound message should advertise the active session name"
    );
}

// ─── getupdates 429 under split-brain load ──────────────────────────────────
//
// The Hub caps concurrent `getupdates` long-polls per vtoken at
// `MAX_CONCURRENT_POLLS_PER_VTOKEN` to prevent split-brain (two bridge
// processes sharing one credential and stealing each other's messages). The
// (cap+1)th concurrent poll must be rejected with 429.

/// Hold MAX+1 concurrent polls on the same vtoken: the first MAX return
/// normally (no messages), the (MAX+1)th gets 429.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn over_cap_concurrent_polls_get_429() {
    use ilink_hub::hub::MAX_CONCURRENT_POLLS_PER_VTOKEN;
    let h = boot().await;
    let vtoken = register_client(&h.base_url, "claude").await;

    // Sanity: the PollTracker is wired up and 4 concurrent enters return
    // monotonically increasing counts 1..=4. We exercise this in a real OS
    // thread (not a spawned tokio task) so the std::sync::Mutex inside
    // PollTracker is hit the same way it would be by hyper's worker threads
    // in production — the cross-process behavior we want to validate.
    let tracker = h.state.clients.poll_tracker.clone();
    let vtoken_for_inproc = vtoken.clone();
    let inproc = std::thread::spawn(move || {
        let c1 = tracker.enter(&vtoken_for_inproc).0;
        let g1 = tracker.enter(&vtoken_for_inproc).1;
        let c2 = tracker.enter(&vtoken_for_inproc).0;
        let g2 = tracker.enter(&vtoken_for_inproc).1;
        let c3 = tracker.enter(&vtoken_for_inproc).0;
        let g3 = tracker.enter(&vtoken_for_inproc).1;
        let c4 = tracker.enter(&vtoken_for_inproc).0;
        drop(g3);
        drop(g2);
        drop(g1);
        (c1, c2, c3, c4)
    });
    let (c1, c2, c3, c4) = inproc.join().unwrap();
    eprintln!("DEBUG: inproc counts = {c1} {c2} {c3} {c4}");
    assert_eq!((c1, c2, c3, c4), (1, 2, 3, 4));

    let client = reqwest::Client::new();
    let url = format!("{}/ilink/bot/getupdates", h.base_url);

    // Fire MAX+1 concurrent long-polls. Each blocks waiting for a message
    // that never arrives, so they only return when the timer (1s) expires
    // or when the server pushes something. We use a short poll timeout so
    // the test finishes quickly.
    let poll_secs: u32 = 1;
    let mut handles = Vec::new();
    let total = MAX_CONCURRENT_POLLS_PER_VTOKEN + 1;
    for _ in 0..total {
        let url = url.clone();
        let auth_val = format!("Bearer {vtoken}");
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            client
                .post(&url)
                .header(header::AUTHORIZATION, auth_val)
                .json(&json!({ "timeout": poll_secs, "base_info": BaseInfo::default() }))
                .send()
                .await
                .expect("http send")
        }));
    }

    let mut statuses = Vec::with_capacity(total);
    for h in handles {
        let resp = h.await.expect("join");
        statuses.push(resp.status());
    }

    let ok_count = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    let too_many = statuses
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();

    assert_eq!(
        ok_count, MAX_CONCURRENT_POLLS_PER_VTOKEN,
        "exactly MAX polls should be admitted, got ok={ok_count}, statuses={statuses:?}"
    );
    assert_eq!(
        too_many, 1,
        "exactly one poll should be rejected with 429, got {too_many}, statuses={statuses:?}"
    );
}
