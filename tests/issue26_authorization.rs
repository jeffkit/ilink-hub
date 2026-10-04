//! Issue #26 — authorization must be explicit, revocable, and call-scoped.
//!
//! Regression tests for the contract changes:
//!   1. `revoke_endpoint_rotates_vtoken_and_rejects_old_send`
//!      —— `POST /hub/clients/{name}/revoke` rotates the vtoken and clears the old
//!      credential's authorization rows, so both the old and the rotated vtoken are
//!      refused by `/ilink/bot/sendmessage`.
//!   2. `message_history_alone_does_not_grant_send_access`
//!      —— the ownership predicate reads `active_sessions` / `backend_sessions_v2`
//!      only; historical `messages` rows must not keep a revoked vtoken alive.
//!   3. `a2a_call_does_not_grant_permanent_send_access`
//!      —— `call_agent` authorizes the target for the duration of one call
//!      (`a2a-<call_id>` scope) and releases it once the reply is delivered.
//!   4. `a2a_unauthorized_target_is_rejected` + `list_agents_exposes_only_allowed_targets`
//!      —— A2A is default-deny and the agent list is filtered by the same ACL.
//!   5. `a2a_disconnect_releases_the_grant` / `a2a_timeout_releases_the_grant` /
//!      `a2a_call_reclaims_every_session_row_it_created`
//!      —— every A2A exit (reply / timeout / disconnect) reclaims the grants the
//!      call created, including `backend_sessions_v2` rows the target named itself
//!      while replying.
//!
//! The TTL that bounds an `active_sessions` grant (`expires_at`) is covered at the
//! store level by `src/store/store_tests.rs::expired_active_session_grant_is_not_authorization`.
//!
//! `ILINK_AGENT_ALLOWLIST` is process-wide and the DB tests share one process: run
//! this target with `--test-threads=1` (the repo convention for DB tests).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use ilink_hub::{
    hub::{AdminConfig, HubState, ENV_AGENT_ALLOWLIST},
    ilink::types::{HubExt, MessageItem, SendMessageRequest, TextItem, WeixinMessage},
    ilink::UpstreamClient,
    mcp::tools::{call_agent, list_agents, CallAgentContext, CallAgentParams},
    server::build_router,
    store::Store,
    InMemoryQueue,
};
use tower::ServiceExt;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn admin_config() -> AdminConfig {
    AdminConfig {
        token: Some("test-admin-token".to_string()),
        insecure_no_auth: false,
        outbound_origin_label: None,
    }
}

/// Build a HubState with `ILINK_AGENT_ALLOWLIST = acl_spec`. The env var is
/// process-wide, hence the `--test-threads=1` requirement for this target.
fn make_state(
    store: Arc<Store>,
    base_url: Option<String>,
    admin: AdminConfig,
    acl_spec: &str,
) -> Arc<HubState> {
    let upstream = Arc::new(
        UpstreamClient::new("sk-test:key".to_string(), base_url).expect("test upstream client"),
    );
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    temp_env::with_var(ENV_AGENT_ALLOWLIST, Some(acl_spec), || {
        HubState::new(
            upstream,
            store,
            queue,
            shutdown_rx,
            "test-relay-secret".to_string(),
            admin,
        )
    })
}

/// Mock iLink upstream that accepts every `sendmessage` with an empty 200 body
/// (the real API's success shape — `UpstreamClient::send_message` treats an
/// empty body as `ret = 0`).
async fn spin_mock_upstream() -> String {
    let app = Router::new().route(
        "/ilink/bot/sendmessage",
        post(|| async { (StatusCode::OK, "") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Register a backend and return `(plaintext, hashed)` — plaintext is what the
/// bridge puts in `Authorization: Bearer`, hashed is the registry/store key.
async fn register(state: &Arc<HubState>, name: &str) -> (String, String) {
    let outcome =
        ilink_hub::server::pairing::register_client_in_hub(state, name.to_string(), None, None)
            .await;
    (outcome.plaintext, outcome.hashed)
}

fn send_request(vctx: &str, vtoken_plain: &str) -> Request<Body> {
    let body = SendMessageRequest::reply(vctx.to_string(), "hello".to_string(), "user-1");
    Request::builder()
        .method("POST")
        .uri("/ilink/bot/sendmessage")
        .header("authorization", format!("Bearer {vtoken_plain}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&body).expect("serialize sendmessage"),
        ))
        .expect("build sendmessage request")
}

/// `/ilink/bot/sendmessage` reports auth/ownership failures as HTTP 200 with a
/// non-zero `ret` (`SendMessageResponse::err`), so assertions read `ret`.
async fn send_ret(app: Router, req: Request<Body>) -> i64 {
    let resp = app.oneshot(req).await.expect("sendmessage request");
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("sendmessage body");
    serde_json::from_slice::<serde_json::Value>(&body).expect("sendmessage json")["ret"]
        .as_i64()
        .unwrap_or(0)
}

// ─── 1. 显式吊销 + vtoken 轮换 ───────────────────────────────────────────────

/// 验收：`POST /hub/clients/{name}/revoke` 存在；返回轮换后的新 vtoken；旧 vtoken
/// 与旧授权行全部失效（`resolve_send_context` → `None` → sendmessage 401/403）。
#[tokio::test]
async fn revoke_endpoint_rotates_vtoken_and_rejects_old_send() {
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(store.clone(), None, admin_config(), "");

    let (old_plain, old_hash) = register(&state, "agent-a").await;
    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");
    // Simulate the dispatch-time grant (`src/hub/dispatch/pipeline.rs:184`).
    store
        .set_active_session_with_depth(&vctx, &old_hash, "default", 0)
        .await
        .expect("grant");
    assert!(
        store
            .resolve_send_context(&vctx, &old_hash)
            .await
            .expect("query")
            .is_some(),
        "precondition: dispatch grant must exist before revoke"
    );
    assert!(
        store
            .get_active_ctx_for_vtoken(&old_hash)
            .await
            .expect("query")
            .is_some(),
        "precondition: the dispatch grant must make the active-session pointer resolvable"
    );

    let app = build_router(state.clone());
    let revoke_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hub/clients/agent-a/revoke")
                .header("authorization", "Bearer test-admin-token")
                .header("content-type", "application/json")
                .body(Body::empty())
                .expect("build revoke request"),
        )
        .await
        .expect("revoke request");
    assert_eq!(
        revoke_resp.status(),
        StatusCode::OK,
        "POST /hub/clients/agent-a/revoke must exist and succeed (got {})",
        revoke_resp.status()
    );

    let body = axum::body::to_bytes(revoke_resp.into_body(), 64 * 1024)
        .await
        .expect("revoke body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("revoke json");
    let new_plain = json
        .get("vtoken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        !new_plain.is_empty() && new_plain != old_plain,
        "revoke must return a rotated vtoken, got: {json}"
    );
    let new_hash = ilink_hub::hub::hash_vtoken(&new_plain);

    // Store-level: the old vtoken's grants must be gone; the rotated vtoken has
    // no grant either (it must be re-authorized by a fresh dispatch).
    assert!(
        store
            .resolve_send_context(&vctx, &old_hash)
            .await
            .expect("query")
            .is_none(),
        "revoke must clear the revoked vtoken's active_sessions/backend_sessions_v2 rows"
    );
    assert!(
        store
            .get_active_ctx_for_vtoken(&old_hash)
            .await
            .expect("query")
            .is_none(),
        "revoke must clear the revoked vtoken's active-session pointer"
    );
    assert!(
        store
            .resolve_send_context(&vctx, &new_hash)
            .await
            .expect("query")
            .is_none(),
        "the rotated vtoken must not inherit the old grants"
    );

    // HTTP-level: both the old and the rotated credential are refused on send.
    for (label, token) in [("old", &old_plain), ("rotated", &new_plain)] {
        let ret = send_ret(app.clone(), send_request(&vctx, token)).await;
        assert!(
            ret == 401 || ret == 403,
            "{label} vtoken must be rejected after revoke (401/403), got ret={ret}"
        );
    }

    // In-memory routing: the revoked hash must not keep serving any route. The
    // router may repoint the default at the *rotated* client (same backend, new
    // credential), but the old hash must be gone from `active_routes`/default.
    let route = state
        .routing
        .router
        .lock()
        .await
        .get_route("user-1")
        .map(str::to_string);
    assert_ne!(
        route.as_deref(),
        Some(old_hash.as_str()),
        "revoke must drop the revoked hash from in-memory routing; the rotated \
         client may only be reachable through its new credential"
    );
    assert!(
        store
            .get_route(ilink_hub::store::HUB_DEFAULT_SENTINEL)
            .await
            .expect("query")
            .as_deref()
            != Some(old_hash.as_str()),
        "revoke must clear the revoked hash's persisted default route (routing_state)"
    );
}

/// 验收：「吊销后 `resolve_send_context` 返回 None」。
///
/// 归属判定的谓词只读 `active_sessions` / `backend_sessions_v2`。派发时
/// `pipeline.rs` 会同时写入一条 `messages(vctx, vtoken)` 行，如果历史消息也算
/// 授权，**只删 `active_sessions` / `backend_sessions_v2`** 的吊销实现就仍会放行
/// 已吊销的 vtoken —— 本用例把这条不可绕过的前提钉在 store 层。
#[tokio::test]
async fn message_history_alone_does_not_grant_send_access() {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");
    store
        .save_message(&vctx, Some("vtoken-old"), "default", "user-1", "user", "hi")
        .await
        .expect("save history");

    assert!(
        store
            .resolve_send_context(&vctx, "vtoken-old")
            .await
            .expect("query")
            .is_none(),
        "message history alone must not authorize a vtoken to send into {vctx} \
         (otherwise revoke cannot make resolve_send_context return None)"
    );
}

// ─── 2. A2A 授权必须限定单次调用 ─────────────────────────────────────────────

/// 轮询目标队列直到 `call_agent` 压入合成消息，返回其 `a2a_call_id`。
async fn wait_for_a2a_call_id(
    state: &Arc<HubState>,
    target_vtoken: &str,
    deadline: Duration,
) -> String {
    let start = Instant::now();
    loop {
        let msgs = state
            .clients
            .queue
            .poll(target_vtoken, None)
            .await
            .expect("poll target queue")
            .msgs;
        if let Some(call_id) = msgs
            .iter()
            .find_map(|m| m.ilink_hub_ext.as_ref().and_then(|e| e.a2a_call_id.clone()))
        {
            return call_id;
        }
        assert!(
            start.elapsed() < deadline,
            "call_agent never pushed an A2A message to the target queue"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 验收：A2A 授权绑定 `call_id` 并在回复完成后失效 —— 回复投递后目标不得再向该
/// vctx 发送（`resolve_send_context` → `None` → 403）。
#[tokio::test]
async fn a2a_call_does_not_grant_permanent_send_access() {
    let base_url = spin_mock_upstream().await;
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(
        store.clone(),
        Some(base_url),
        admin_config(),
        "caller->target",
    );

    let (_caller_plain, caller_hash) = register(&state, "caller").await;
    let (target_plain, target_hash) = register(&state, "target").await;

    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");
    // The caller is serving this WeChat conversation (dispatch-time grant).
    store
        .set_active_session_with_depth(&vctx, &caller_hash, "default", 0)
        .await
        .expect("grant caller");

    let ctx = CallAgentContext {
        caller_vtoken: caller_hash,
        vctx: vctx.clone(),
        real_ctx: "real-ctx-1".to_string(),
        peer_user_id: "user-1".to_string(),
        a2a_depth: 0,
    };
    let call = tokio::spawn({
        let state = state.clone();
        async move {
            call_agent(
                &state,
                ctx,
                CallAgentParams {
                    target_name: "target".to_string(),
                    message: "ping".to_string(),
                    session: None,
                },
            )
            .await
        }
    });

    let call_id = wait_for_a2a_call_id(&state, &target_hash, Duration::from_secs(10)).await;
    assert!(
        state.a2a_waiter.resolve(&call_id, "pong".to_string()),
        "waiter must still be pending when the target replies"
    );
    let result = tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .expect("call_agent must finish after the reply")
        .expect("call_agent task");
    assert!(
        result.to_string().contains("pong"),
        "call_agent must return the target's reply, got: {result}"
    );

    // After the call completes the target's one-shot authorization must be gone.
    assert!(
        store
            .resolve_send_context(&vctx, &target_hash)
            .await
            .expect("query")
            .is_none(),
        "A2A authorization must expire once the reply is delivered, but the target \
         still resolves send context for {vctx} (permanent cross-tenant write channel)"
    );
    assert!(
        store
            .get_active_ctx_for_vtoken(&target_hash)
            .await
            .expect("query")
            .is_none(),
        "the target must not retain an active session pointer for the caller's vctx"
    );

    // HTTP-level equivalent: the target's own credential is refused on the
    // caller's conversation.
    let app = build_router(state.clone());
    let ret = send_ret(app, send_request(&vctx, &target_plain)).await;
    assert!(
        ret != 0,
        "target must not be able to send into the caller's conversation after the A2A \
         call ends (got ret=0 = accepted)"
    );
}

// ─── 3. A2A 释放不得误删合法授权 ─────────────────────────────────────────────

/// 释放只回收本次调用写入的行：目标在该会话上已有的合法 grant（广播 / `/use` 派发）
/// 必须原样保留（session 名与深度），否则 A2A 会切断目标正常的回复通道。
#[tokio::test]
async fn a2a_call_restores_the_targets_preexisting_grant() {
    let base_url = spin_mock_upstream().await;
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(
        store.clone(),
        Some(base_url),
        admin_config(),
        "caller->target",
    );

    let (_caller_plain, caller_hash) = register(&state, "caller").await;
    let (_target_plain, target_hash) = register(&state, "target").await;

    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");
    // The caller serves this conversation; the target is separately selected for it.
    store
        .set_active_session_with_depth(&vctx, &caller_hash, "default", 0)
        .await
        .expect("grant caller");
    store
        .set_active_session_with_depth(&vctx, &target_hash, "selected", 0)
        .await
        .expect("pre-existing target grant");

    let call = tokio::spawn({
        let state = state.clone();
        let vctx = vctx.clone();
        async move {
            call_agent(
                &state,
                CallAgentContext {
                    caller_vtoken: caller_hash,
                    vctx,
                    real_ctx: "real-ctx-1".to_string(),
                    peer_user_id: "user-1".to_string(),
                    a2a_depth: 0,
                },
                CallAgentParams {
                    target_name: "target".to_string(),
                    message: "ping".to_string(),
                    session: None,
                },
            )
            .await
        }
    });

    let call_id = wait_for_a2a_call_id(&state, &target_hash, Duration::from_secs(10)).await;
    assert!(state.a2a_waiter.resolve(&call_id, "pong".to_string()));
    let result = tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .expect("call_agent must finish after the reply")
        .expect("call_agent task");
    assert!(result.to_string().contains("pong"));

    assert_eq!(
        store
            .get_active_session_row(&vctx, &target_hash)
            .await
            .expect("query"),
        Some(("selected".to_string(), 0)),
        "the target's pre-existing grant must survive the A2A release"
    );
    assert!(
        store
            .resolve_send_context(&vctx, &target_hash)
            .await
            .expect("query")
            .is_some(),
        "the target must keep its legitimate send access to {vctx}"
    );
}

// ─── 4. A2A allowlist（默认拒绝）────────────────────────────────────────────
fn agents_json(result: &serde_json::Value) -> Vec<serde_json::Value> {
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).expect("agents JSON")
}

fn agent_names(result: &serde_json::Value) -> Vec<String> {
    agents_json(result)
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::to_string))
        .collect()
}

/// 验收：未授权目标被拒（`isError` + 403 语义），且拒绝发生在任何副作用之前 ——
/// 不注册 waiter、不写授权行、不推目标队列。
#[tokio::test]
async fn a2a_unauthorized_target_is_rejected() {
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(
        store.clone(),
        None,
        admin_config(),
        "caller->some-other-agent",
    );

    let (_caller_plain, caller_hash) = register(&state, "caller").await;
    let (_target_plain, target_hash) = register(&state, "target").await;

    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");

    let result = call_agent(
        &state,
        CallAgentContext {
            caller_vtoken: caller_hash,
            vctx: vctx.clone(),
            real_ctx: "real-ctx-1".to_string(),
            peer_user_id: "user-1".to_string(),
            a2a_depth: 0,
        },
        CallAgentParams {
            target_name: "target".to_string(),
            message: "ping".to_string(),
            session: None,
        },
    )
    .await;

    assert_eq!(
        result["isError"],
        serde_json::json!(true),
        "an unauthorized A2A target must fail the tool call: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("403") && text.contains("not authorized"),
        "rejection must name the ACL cause, got: {text}"
    );

    let queued = state
        .clients
        .queue
        .poll(&target_hash, None)
        .await
        .expect("poll target queue")
        .msgs;
    assert!(
        queued.is_empty(),
        "a rejected A2A call must not reach the target's queue"
    );
    assert!(
        store
            .get_active_session_row(&vctx, &target_hash)
            .await
            .expect("query")
            .is_none(),
        "a rejected A2A call must not write an authorization row"
    );
}

/// 验收：`list_agents` 只暴露调用方被授权调用的目标（默认拒绝）。
#[tokio::test]
async fn list_agents_exposes_only_allowed_targets() {
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(store, None, admin_config(), "caller->target");

    let (_caller_plain, caller_hash) = register(&state, "caller").await;
    register(&state, "target").await;
    register(&state, "hidden").await;

    let listed = list_agents(&state, &caller_hash).await;
    assert_eq!(
        agent_names(&listed),
        vec!["target"],
        "only allowlisted targets may be listed"
    );

    // An unregistered caller sees nothing at all (fail-closed).
    let anonymous = list_agents(&state, "not-a-registered-vtoken").await;
    assert!(
        agents_json(&anonymous).is_empty(),
        "an unknown caller must not enumerate agents"
    );
}

// ─── 5. A2A 释放：超时 / 断连 / 差集回收 ─────────────────────────────────────

/// Drive a bridge-style reply through the real `/ilink/bot/sendmessage` route
/// with a fully controlled `ilink_hub_ext` (session name, cli_session_id,
/// a2a_call_id) — exactly what a target Agent controls when it replies.
fn bridge_reply_request(vctx: &str, vtoken_plain: &str, text: &str, ext: HubExt) -> Request<Body> {
    let msg = WeixinMessage {
        message_type: Some(2),
        to_user_id: Some("user-1".to_string()),
        context_token: Some(vctx.to_string()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            ..Default::default()
        }])),
        ilink_hub_ext: Some(ext),
        ..Default::default()
    };
    let body = SendMessageRequest {
        msg: Some(msg),
        base_info: None,
    };
    Request::builder()
        .method("POST")
        .uri("/ilink/bot/sendmessage")
        .header("authorization", format!("Bearer {vtoken_plain}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&body).expect("serialize bridge reply"),
        ))
        .expect("build bridge reply request")
}

/// Shared setup for the A2A release-exit tests: caller grants itself the
/// conversation, then calls `target` and returns the spawned call plus the ids.
struct A2aCallSetup {
    state: Arc<HubState>,
    store: Arc<Store>,
    vctx: String,
    target_plain: String,
    target_hash: String,
    call: tokio::task::JoinHandle<serde_json::Value>,
}

async fn spawn_a2a_call(target_session: Option<String>) -> A2aCallSetup {
    let base_url = spin_mock_upstream().await;
    let store = Arc::new(
        Store::connect("sqlite::memory:")
            .await
            .expect("in-memory store"),
    );
    let state = make_state(
        store.clone(),
        Some(base_url),
        admin_config(),
        "caller->target",
    );

    let (_caller_plain, caller_hash) = register(&state, "caller").await;
    let (target_plain, target_hash) = register(&state, "target").await;

    let vctx = store
        .find_or_create_vctx("peer:user-1", None, "real-ctx-1")
        .await
        .expect("create vctx");
    store
        .set_active_session_with_depth(&vctx, &caller_hash, "default", 0)
        .await
        .expect("grant caller");

    let call = tokio::spawn({
        let state = state.clone();
        let vctx = vctx.clone();
        async move {
            call_agent(
                &state,
                CallAgentContext {
                    caller_vtoken: caller_hash,
                    vctx,
                    real_ctx: "real-ctx-1".to_string(),
                    peer_user_id: "user-1".to_string(),
                    a2a_depth: 0,
                },
                CallAgentParams {
                    target_name: "target".to_string(),
                    message: "ping".to_string(),
                    session: target_session,
                },
            )
            .await
        }
    });

    A2aCallSetup {
        state,
        store,
        vctx,
        target_plain,
        target_hash,
        call,
    }
}

/// 验收 B2-2（断连出口）：目标在回复前掉线（waiter 的 sender 被丢弃 → 接收端
/// `RecvError`）时，本次调用的一次性授权同样必须回收。
#[tokio::test]
async fn a2a_disconnect_releases_the_grant() {
    let mut setup = spawn_a2a_call(None).await;
    let call_id =
        wait_for_a2a_call_id(&setup.state, &setup.target_hash, Duration::from_secs(10)).await;

    // Target goes offline before replying.
    setup.state.a2a_waiter.cancel(&call_id);

    let result = tokio::time::timeout(Duration::from_secs(15), &mut setup.call)
        .await
        .expect("call_agent must finish when the target disconnects")
        .expect("call_agent task");
    assert_eq!(
        result["isError"],
        serde_json::json!(true),
        "a disconnect must fail the tool call: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("disconnected"),
        "the disconnect exit must name its cause, got: {text}"
    );

    assert_released(
        &setup.store,
        &setup.state,
        &setup.vctx,
        &setup.target_plain,
        &setup.target_hash,
    )
    .await;
}

/// 验收 B2-2（超时出口）：目标在 `CALL_AGENT_TIMEOUT` 内没有回复时，本次调用的一次性
/// 授权同样必须回收。
///
/// 120s 的 `CALL_AGENT_TIMEOUT` 不能真等：等调用进入等待后 `tokio::time::pause()`
/// 冻结时钟，再以固定步长推进，直到这次调用走到它自己的超时出口；推进结束后立刻
/// `resume()`，让所有真实连接断言跑在墙上时钟上。（不用
/// `#[tokio::test(start_paused = true)]`：冻结的时钟会让 sqlx 连接池自身的超时被
/// 自动推进，store 还没建起来就 `PoolTimedOut`；同一个自动推进在断言期会把健康查询
/// 打进池的 `acquire_timeout`，即 #40 的 `pool timed out while waiting for an open
/// connection`。）步进而非一次 `advance`：每次 park 的自动推进被队列里唯一的定时器
/// 上限住，因此不依赖「120s 死线此刻是否已注册」。
#[tokio::test]
async fn a2a_timeout_releases_the_grant() {
    let mut setup = spawn_a2a_call(None).await;
    // The call is now blocked on its reply channel (the grant row is written
    // before the queue push, so reaching the queue means the wait has started).
    let _call_id =
        wait_for_a2a_call_id(&setup.state, &setup.target_hash, Duration::from_secs(10)).await;

    tokio::time::pause();
    // Step the frozen clock until the call reaches its own timeout exit: the
    // 120s deadline is created *after* the queue push this test waits on, so a
    // single `advance` cannot know whether it already exists. Each `sleep` is
    // the only timer queued, which bounds every auto-advance to one step.
    let mut result = None;
    for _ in 0..40 {
        if let Ok(done) = tokio::time::timeout(Duration::ZERO, &mut setup.call).await {
            result = Some(done.expect("call_agent task"));
            break;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    // Every real-connection assertion below runs on the wall clock: on a frozen
    // clock the pool's own timers (30s acquire_timeout, 600s idle reaper) are
    // auto-advanced, which is what turns a healthy query into a `PoolTimedOut`.
    tokio::time::resume();

    let result = result.expect("call_agent must reach its timeout exit");
    assert_eq!(
        result["isError"],
        serde_json::json!(true),
        "a timeout must fail the tool call: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("did not reply within"),
        "the timeout exit must name its cause, got: {text}"
    );

    assert_released(
        &setup.store,
        &setup.state,
        &setup.vctx,
        &setup.target_plain,
        &setup.target_hash,
    )
    .await;
}

/// Post-call assertions shared by the disconnect / timeout exits: no grant row,
/// no active-session pointer, and the target's own credential is refused by
/// `/ilink/bot/sendmessage`.
async fn assert_released(
    store: &Arc<Store>,
    state: &Arc<HubState>,
    vctx: &str,
    target_plain: &str,
    target_hash: &str,
) {
    assert!(
        store
            .resolve_send_context(vctx, target_hash)
            .await
            .expect("query")
            .is_none(),
        "the released A2A call must not keep an authorization row for {vctx}"
    );
    assert!(
        store
            .get_active_ctx_for_vtoken(target_hash)
            .await
            .expect("query")
            .is_none(),
        "the released A2A call must not keep an active-session pointer"
    );
    let app = build_router(state.clone());
    let ret = send_ret(app, send_request(vctx, target_plain)).await;
    assert!(
        ret != 0,
        "the target must not be able to send into the caller's conversation after \
         the A2A call ends (got ret=0 = accepted)"
    );
}

/// 验收 B2-4：目标回复时回带 `cli_session_id` 会在 `backend_sessions_v2` 落一行
/// **无 TTL** 的授权行，且行名由目标自选。若释放只删本次 `reply_session`，目标把
/// 行名取成别的值就能留下永久授权 —— 释放必须按「调用前快照的差集」回收该 pair
/// 的全部新行。
#[tokio::test]
async fn a2a_call_reclaims_every_session_row_it_created() {
    let mut setup = spawn_a2a_call(Some("a2a-scope-1".to_string())).await;
    assert!(
        setup
            .store
            .list_backend_sessions(&setup.vctx, &setup.target_hash)
            .await
            .expect("query")
            .is_empty(),
        "precondition: the pair must hold no backend session before the call"
    );

    let call_id =
        wait_for_a2a_call_id(&setup.state, &setup.target_hash, Duration::from_secs(10)).await;

    // The target replies through the real sendmessage route, echoing a
    // cli_session_id under a session name of its own choosing ("escape"), which
    // is not this call's reply session.
    let ret = send_ret(
        build_router(setup.state.clone()),
        bridge_reply_request(
            &setup.vctx,
            &setup.target_plain,
            "pong",
            HubExt {
                a2a_call_id: Some(call_id),
                session_name: Some("escape".to_string()),
                cli_session_id: Some("uuid-escape".to_string()),
                ..Default::default()
            },
        ),
    )
    .await;
    assert_eq!(ret, 0, "the target's reply must be accepted");

    let result = tokio::time::timeout(Duration::from_secs(15), &mut setup.call)
        .await
        .expect("call_agent must finish after the reply")
        .expect("call_agent task");
    assert!(
        result.to_string().contains("pong"),
        "call_agent must return the target's reply, got: {result}"
    );

    assert!(
        setup
            .store
            .list_backend_sessions(&setup.vctx, &setup.target_hash)
            .await
            .expect("query")
            .is_empty(),
        "the authorization row the reply created via cli_session_id must be reclaimed"
    );
    assert!(
        setup
            .store
            .resolve_send_context(&setup.vctx, &setup.target_hash)
            .await
            .expect("query")
            .is_none(),
        "no residual send authorization may survive the call"
    );

    // Control: the very same reply without an a2a_call_id does persist the row,
    // which proves the assertion above is not vacuous.
    let control_vctx = setup
        .store
        .find_or_create_vctx("peer:user-2", None, "real-ctx-2")
        .await
        .expect("create control vctx");
    setup
        .store
        .set_active_session_with_depth(&control_vctx, &setup.target_hash, "default", 0)
        .await
        .expect("grant target on the control conversation");
    let ret = send_ret(
        build_router(setup.state.clone()),
        bridge_reply_request(
            &control_vctx,
            &setup.target_plain,
            "hello",
            HubExt {
                session_name: Some("escape".to_string()),
                cli_session_id: Some("uuid-control".to_string()),
                ..Default::default()
            },
        ),
    )
    .await;
    assert_eq!(ret, 0, "the control reply must be accepted");
    let rows = setup
        .store
        .list_backend_sessions(&control_vctx, &setup.target_hash)
        .await
        .expect("query");
    assert_eq!(
        rows.iter()
            .map(|r| r.session_name.as_str())
            .collect::<Vec<_>>(),
        vec!["escape"],
        "without an a2a_call_id the cli_session_id row persists (control)"
    );
}
