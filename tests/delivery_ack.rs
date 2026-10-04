//! Issue #27 验收测试 — 投递语义：at-least-once（稳定 id + ack 水位 + 背压拒新）。
//!
//! 覆盖的验收条款：
//!
//! - 条 1：`getupdates` 返回的每条消息带稳定 id（Hub 在入队时分配 `seq`）；
//!   客户端下次 poll 回带游标后，已确认的消息不再重投。
//! - 条 2：未确认的消息不因响应在回程丢失而永久消失——不带 ack 的再次 poll
//!   必须取回同一批（同 id、同内容）。
//! - 条 3：队列溢出改为背压拒绝新消息（保留最旧），并把拒绝计数归因到客户端
//!   （`ilink_hub_messages_rejected_total{client=…}`）。
//! - 条 4：响应的 `get_updates_buf` 非空且可回带，带游标 poll 只收到游标之后的消息。
//!
//! 游标载体钉死为 `get_updates_buf`（现网 bridge 每轮都会原样回带它）；
//! `last_ack_id` 是等价的显式载体，只在专门用例里注入。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::Json;
use ilink_hub::{
    hub::{AdminConfig, HubState},
    ilink::types::{GetUpdatesRequest, MessageItem, TextItem, WeixinMessage},
    ilink::UpstreamClient,
    store::Store,
    InMemoryQueue, MessageQueue,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Returns the state plus the shutdown watch sender, which the caller must keep
/// alive for the test's lifetime so `wait_notify_or_shutdown` really waits
/// instead of returning early on a closed channel (same rationale as
/// `tests/hub_routing_integration.rs`).
async fn make_state_with_queue(
    queue: Arc<dyn MessageQueue>,
) -> (Arc<HubState>, tokio::sync::watch::Sender<bool>) {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let upstream =
        Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("test upstream client"));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    );
    (state, shutdown_tx)
}

async fn make_state() -> (Arc<HubState>, tokio::sync::watch::Sender<bool>) {
    make_state_with_queue(Arc::new(InMemoryQueue::new())).await
}

async fn register(state: &Arc<HubState>, name: &str) -> (String, String) {
    let outcome =
        ilink_hub::server::pairing::register_client_in_hub(state, name.to_string(), None, None)
            .await;
    (outcome.plaintext, outcome.hashed)
}

fn auth(plain: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {plain}")).unwrap(),
    );
    h
}

/// Poll body carrying the delivery cursor the client echoes back (empty on first
/// contact). This is the carrier the production bridge uses.
fn poll_request(timeout: u32, cursor: &str) -> GetUpdatesRequest {
    GetUpdatesRequest {
        get_updates_buf: cursor.to_string(),
        last_ack_id: None,
        base_info: None,
        timeout: Some(timeout),
    }
}

/// Poll body carrying only the explicit `last_ack_id` carrier (no cursor string).
fn poll_request_with_last_ack(timeout: u32, id: i64) -> GetUpdatesRequest {
    GetUpdatesRequest {
        get_updates_buf: String::new(),
        last_ack_id: Some(id),
        base_info: None,
        timeout: Some(timeout),
    }
}

async fn dispatch_poll(
    state: &Arc<HubState>,
    plain: &str,
    req: GetUpdatesRequest,
) -> (StatusCode, serde_json::Value) {
    let (status, Json(resp)) =
        ilink_hub::server::routes::getupdates(State(Arc::clone(state)), auth(plain), Json(req))
            .await;
    let value = serde_json::to_value(&resp).expect("GetUpdatesResponse must serialize");
    (status, value)
}

/// Poll without any acknowledgement (first contact / lost previous response).
async fn poll(state: &Arc<HubState>, plain: &str, timeout: u32) -> (StatusCode, serde_json::Value) {
    dispatch_poll(state, plain, poll_request(timeout, "")).await
}

/// Poll echoing a cursor verbatim, exactly as a bridge does.
async fn poll_with_cursor(
    state: &Arc<HubState>,
    plain: &str,
    timeout: u32,
    cursor: &str,
) -> (StatusCode, serde_json::Value) {
    dispatch_poll(state, plain, poll_request(timeout, cursor)).await
}

/// Poll acknowledging via the explicit `last_ack_id` field only.
async fn poll_with_last_ack(
    state: &Arc<HubState>,
    plain: &str,
    timeout: u32,
    id: i64,
) -> (StatusCode, serde_json::Value) {
    dispatch_poll(state, plain, poll_request_with_last_ack(timeout, id)).await
}

/// Cursor carried by a response.
fn cursor(body: &serde_json::Value) -> String {
    body.get("get_updates_buf")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn msgs(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("msgs")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Stable delivery id of a message: `message_id` when the upstream supplied one,
/// otherwise the Hub-assigned per-vtoken `seq`.
fn stable_id(msg: &serde_json::Value) -> Option<i64> {
    ["message_id", "id", "msg_id", "ilink_msg_id", "seq"]
        .iter()
        .find_map(|k| msg.get(*k).and_then(|v| v.as_i64()))
}

fn ids(msgs: &[serde_json::Value]) -> Vec<Option<i64>> {
    msgs.iter().map(stable_id).collect()
}

fn texts(msgs: &[serde_json::Value]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            m.pointer("/item_list/0/text_item/text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn text_msg(text: &str, message_id: Option<i64>) -> WeixinMessage {
    WeixinMessage {
        message_id,
        message_type: Some(1),
        from_user_id: Some("user-27".to_string()),
        context_token: Some("ctx-27".to_string()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// 验收条 1：getupdates 返回的每条消息带稳定 id；同一客户端下次 poll 回带游标后，
/// 已确认的消息不再重投（push → poll → ack → 再 poll），且 ack 幂等。
#[tokio::test]
async fn poll_ack_poll_round_trip_does_not_redeliver() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m1", Some(1001)))
        .await
        .expect("push");

    let (status, first) = poll(&state, &plain, 0).await;
    assert_eq!(status, StatusCode::OK, "poll must succeed");
    let first_msgs = msgs(&first);
    assert_eq!(
        first_msgs.len(),
        1,
        "first poll must return the queued message"
    );
    assert!(
        stable_id(&first_msgs[0]).is_some(),
        "每条 getupdates 消息必须带稳定 id"
    );
    let cur = cursor(&first);
    assert!(!cur.is_empty(), "response must carry a non-empty cursor");

    // 客户端回带游标后，同一批不得重投。
    let (status, second) = poll_with_cursor(&state, &plain, 0, &cur).await;
    assert_eq!(status, StatusCode::OK, "poll after ack must succeed");
    assert!(
        msgs(&second).is_empty(),
        "已确认的消息不得重投，got {:?}",
        msgs(&second)
    );

    // ack 幂等：继续回带同一游标仍不得重投。
    let (_, third) = poll_with_cursor(&state, &plain, 0, &cur).await;
    assert!(
        msgs(&third).is_empty(),
        "ack 必须幂等，got {:?}",
        msgs(&third)
    );

    // 注：游标活在本 vtoken 的投递序号空间（`get_updates_buf`），而消息自带的
    // `message_id` 可能来自上游、是另一个数空间；Hub 对 ack 做 clamp，所以按
    // `message_id` 确认的客户端等价于「确认到目前已投递的最大序号」。
}

/// 验收条 2：未收到 ack 的消息不因响应在回程丢失而永久消失 —— 模拟丢响应
/// （poll 一次但不回带游标）后再次 poll，仍可取回同一 id、同一内容的消息。
#[tokio::test]
async fn unacked_message_survives_lost_response_and_is_redelivered() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-lost", Some(1002)))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    let first_msgs = msgs(&first);
    assert_eq!(
        first_msgs.len(),
        1,
        "first poll must return the queued message"
    );
    let id = stable_id(&first_msgs[0]).expect("delivered message must carry a stable id");

    // 模拟「响应在回程丢失」：客户端没拿到第一条响应，因此下一次 poll 不回带游标。
    let (_, second) = poll(&state, &plain, 0).await;
    let second_msgs = msgs(&second);
    assert_eq!(
        second_msgs.len(),
        1,
        "未 ack 的消息必须可再次取回（旧语义：破坏性 drain 后永久消失）"
    );
    assert_eq!(
        stable_id(&second_msgs[0]),
        Some(id),
        "重投必须复用同一稳定 id"
    );
    assert_eq!(second_msgs[0], first_msgs[0], "重投内容必须与首次投递一致");
}

/// 验收条 4：响应里的 `get_updates_buf` 不得恒为空串，且回带该游标必须真正推进
/// 投递水位（不得重投已交付的批次）。
#[tokio::test]
async fn response_cursor_is_non_empty_and_advances() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-cursor", Some(1101)))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    assert_eq!(
        msgs(&first).len(),
        1,
        "first poll must return the queued message"
    );
    let cur = cursor(&first);
    assert!(
        !cur.is_empty(),
        "getupdates 响应必须回带非空游标；响应: {first:?}"
    );

    // 原样回带游标：已交付的那批必须被水位推进掉，否则客户端会无限重复处理同一批消息。
    let (_, echo) = poll_with_cursor(&state, &plain, 0, &cur).await;
    assert!(
        msgs(&echo).is_empty(),
        "回带游标后不得重投已交付批次，got {:?}",
        msgs(&echo)
    );

    // 空队列也必须回带非空游标（客户端下次 poll 才能继续推进水位）。
    let (_, empty) = poll(&state, &plain, 0).await;
    assert!(
        !cursor(&empty).is_empty(),
        "空批同样必须回带非空游标，got {empty:?}"
    );
}

/// 验收条 4（断点续拉）：带游标 poll 只收到游标之后的消息。
#[tokio::test]
async fn cursor_poll_returns_only_messages_after_cursor() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("before-cursor", Some(1201)))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    assert_eq!(msgs(&first).len(), 1, "first poll must return the message");
    let cur = cursor(&first);
    assert!(!cur.is_empty(), "断点续拉依赖非空游标");

    // 游标之后新到的消息：必须能取回，且不得夹带游标之前的消息。
    state
        .clients
        .queue
        .push(&vtoken, text_msg("after-cursor", Some(1202)))
        .await
        .expect("push");
    let (_, second) = poll_with_cursor(&state, &plain, 0, &cur).await;
    let second_msgs = msgs(&second);
    assert_eq!(
        second_msgs.len(),
        1,
        "带游标 poll 只应收到游标之后的消息，got {second_msgs:?}"
    );
    assert_eq!(
        second_msgs[0]
            .pointer("/item_list/0/text_item/text")
            .and_then(|v| v.as_str()),
        Some("after-cursor"),
        "带游标 poll 不得重投游标之前的消息（before-cursor）"
    );
}

/// 上游（iLink）不保证 `message_id` 一定存在：`WeixinMessage::message_id` 是
/// `Option`，且 Hub 自己合成的消息（@mention 转发、MCP `call_agent`）没有上游 id。
/// 因此「每条消息带稳定 id」必须由 Hub 在投递侧保证，不能依赖上游字段。
#[tokio::test]
async fn delivery_assigns_stable_id_when_upstream_omits_it() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-no-upstream-id", None))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    let first_msgs = msgs(&first);
    assert_eq!(
        first_msgs.len(),
        1,
        "first poll must return the queued message"
    );
    assert!(
        first_msgs[0].get("message_id").is_none(),
        "上游没给 id，投递消息不得凭空捏造 message_id"
    );
    let id = stable_id(&first_msgs[0]).expect("Hub 必须为无上游 message_id 的消息分配稳定 id");

    let (_, second) = poll(&state, &plain, 0).await;
    assert_eq!(
        stable_id(
            msgs(&second)
                .first()
                .expect("unacked message must be redelivered")
        ),
        Some(id),
        "Hub 分配的 id 在重投之间必须稳定"
    );

    // Hub 分配的 id 必须是可回带的 ack 值。
    let (_, acked) = poll_with_cursor(&state, &plain, 0, &id.to_string()).await;
    assert!(
        msgs(&acked).is_empty(),
        "Hub 分配的 id 必须能作为游标确认该批，got {:?}",
        msgs(&acked)
    );
}

/// 验收条 1（等价载体）：仅用显式 `last_ack_id`（不带 `get_updates_buf`）也必须
/// 能确认该批。
#[tokio::test]
async fn ack_via_last_ack_id_alone_prunes() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-last-ack", None))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    let id = stable_id(&msgs(&first)[0]).expect("stable id");

    let (_, second) = poll_with_last_ack(&state, &plain, 0, id).await;
    assert!(
        msgs(&second).is_empty(),
        "last_ack_id 必须同样能推进水位，got {:?}",
        msgs(&second)
    );
}

/// D5：伪造的超大游标被 clamp 到「已分配上限」，只能确认已投递的消息，
/// 不得让队列永久静默（后续 push 的消息仍必须可投递）。
#[tokio::test]
async fn forged_large_cursor_clamps_and_does_not_silence_future_messages() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-old", Some(1301)))
        .await
        .expect("push");

    let (_, forged) = poll_with_cursor(&state, &plain, 0, "9999").await;
    assert!(
        msgs(&forged).is_empty(),
        "伪造超大游标只能确认已分配的消息，got {:?}",
        msgs(&forged)
    );

    state
        .clients
        .queue
        .push(&vtoken, text_msg("m-new", Some(1302)))
        .await
        .expect("push");
    let (_, after) = poll(&state, &plain, 0).await;
    let after_msgs = msgs(&after);
    assert_eq!(
        after_msgs.len(),
        1,
        "伪造游标不得让队列永久静默，got {after_msgs:?}"
    );
    assert_eq!(
        after_msgs[0]
            .pointer("/item_list/0/text_item/text")
            .and_then(|v| v.as_str()),
        Some("m-new"),
        "只有 clamp 之前已分配的消息被确认"
    );
}

/// 客户端进程重启（内存里的游标丢失）：不带游标的 poll 必须取回全部未确认消息
/// （同 id、按 FIFO），证明队列而非客户端状态才是投递权威。
#[tokio::test]
async fn cursor_survives_client_restart() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;

    state
        .clients
        .queue
        .push(&vtoken, text_msg("r1", Some(1401)))
        .await
        .expect("push");
    state
        .clients
        .queue
        .push(&vtoken, text_msg("r2", Some(1402)))
        .await
        .expect("push");

    let (_, first) = poll(&state, &plain, 0).await;
    assert_eq!(msgs(&first).len(), 2, "both queued messages delivered");
    let before_restart = ids(&msgs(&first));
    let texts_before = texts(&msgs(&first));

    // 「重启后」的新进程没有内存游标，因此首轮 poll 不带 ack。
    let (_, after_restart) = poll(&state, &plain, 0).await;
    assert_eq!(
        ids(&msgs(&after_restart)),
        before_restart,
        "未确认消息必须在客户端重启后按同一 id 顺序取回"
    );
    let texts_after = texts(&msgs(&after_restart));
    assert_eq!(texts_after, texts_before, "内容与顺序都不得变化");
}

/// 验收条 3：队列溢出不再静默丢最旧 —— 改为背压拒绝新消息，
/// 已入队的最旧消息必须被保留。
#[tokio::test]
async fn queue_overflow_rejects_new_instead_of_dropping_oldest() {
    let queue = InMemoryQueue::with_limit(3);
    for i in 0..6 {
        let _ = queue
            .push(
                "vt-27",
                WeixinMessage {
                    message_id: Some(i),
                    ..Default::default()
                },
            )
            .await;
    }

    let batch = queue.poll("vt-27", None).await.expect("poll");
    let got: Vec<i64> = batch.msgs.iter().filter_map(|m| m.message_id).collect();
    assert_eq!(
        got,
        vec![0, 1, 2],
        "溢出必须背压拒绝新消息、保留最旧；旧语义是丢最旧留最新"
    );

    // 回带游标确认该批后队列才有空位 —— 背压不等于永久卡死。
    let acked = queue.poll("vt-27", Some(batch.cursor)).await.expect("poll");
    assert!(acked.is_empty());

    let rejected = queue
        .push(
            "vt-27",
            WeixinMessage {
                message_id: Some(99),
                ..Default::default()
            },
        )
        .await
        .expect("push after ack must be accepted");
    assert!(!rejected, "确认后队列有空位，push 不应报告背压");
    let after = queue.poll("vt-27", None).await.expect("poll");
    assert_eq!(
        after
            .msgs
            .iter()
            .filter_map(|m| m.message_id)
            .collect::<Vec<_>>(),
        vec![99],
        "确认后入队的消息必须可被取回"
    );
}

/// 验收条 3（归因）：背压拒绝必须归因到具体客户端，而不是一个全局无标签计数器。
#[tokio::test]
async fn queue_overflow_drop_is_attributed_to_the_client() {
    let queue = Arc::new(InMemoryQueue::with_limit(2));
    let (state, _keepalive) = make_state_with_queue(queue).await;
    let (_, vtoken) = register(&state, "claude").await;

    // 填满队列（2 条），第 3 条必然被拒。
    for i in 0..2 {
        ilink_hub::hub::push_to_queue_pub(
            &state.clients.queue,
            &state.metrics,
            &vtoken,
            text_msg("fill", Some(i)),
        )
        .await;
    }
    ilink_hub::hub::push_to_queue_pub(
        &state.clients.queue,
        &state.metrics,
        &vtoken,
        text_msg("overflow", Some(2)),
    )
    .await;

    let text = ilink_hub::metrics::gather_metrics(&state, "test-hub")
        .await
        .expect("gather_metrics");

    let attributed = text.lines().any(|line| {
        line.starts_with("ilink_hub_messages_rejected_total")
            && line.contains("claude")
            && line.trim_end().ends_with('1')
    });
    assert!(
        attributed,
        "背压拒绝必须归因到具体客户端（client 标签，值 1）。metrics 相关行：\n{}",
        text.lines()
            .filter(|l| l.contains("dropped") || l.contains("rejected"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Hub 级总量同步反映（后向兼容的既有家族）。
    assert!(
        text.contains("ilink_hub_messages_dropped_total 1"),
        "Hub 级 messages_dropped 仍须计入背压拒绝"
    );
}

/// 防回归：poll 不应把「取走」当成「已投递」——取走但未 ack 的消息必须对同一
/// vtoken 可见，且不同 vtoken 之间不得互相看到（ack 是 per-client 语义）。
#[tokio::test]
async fn redelivery_is_per_client() {
    let (state, _keepalive) = make_state().await;
    let (plain_a, vtoken_a) = register(&state, "claude-a").await;
    let (plain_b, _vtoken_b) = register(&state, "claude-b").await;

    state
        .clients
        .queue
        .push(&vtoken_a, text_msg("only-for-a", Some(1003)))
        .await
        .expect("push");

    let (_, a_first) = poll(&state, &plain_a, 0).await;
    let id = stable_id(&msgs(&a_first)[0]).expect("stable id");

    let (_, b_poll) = poll(&state, &plain_b, 0).await;
    assert!(
        msgs(&b_poll).is_empty(),
        "client B 不得看到 client A 的消息"
    );

    let (_, a_second) = poll(&state, &plain_a, 0).await;
    assert_eq!(
        stable_id(&msgs(&a_second)[0]),
        Some(id),
        "未 ack 的消息只对原 vtoken 重投"
    );
}

/// 沙箱兜底：整条测试链路不得长时间挂起（长轮询 timeout=0，全部应立即返回）。
#[tokio::test]
async fn poll_path_terminates_promptly() {
    let (state, _keepalive) = make_state().await;
    let (plain, vtoken) = register(&state, "claude").await;
    state
        .clients
        .queue
        .push(&vtoken, text_msg("fast", Some(1004)))
        .await
        .expect("push");

    tokio::time::timeout(Duration::from_secs(5), async {
        let _ = poll(&state, &plain, 0).await;
        let _ = poll(&state, &plain, 0).await;
    })
    .await
    .expect("timeout=0 的 poll 必须立即返回");
}
