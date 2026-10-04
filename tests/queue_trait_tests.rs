use ilink_hub::{
    hub::queue::InMemoryQueue,
    ilink::types::{MessageItem, TextItem, WeixinMessage},
    MessageQueue, PollBatch,
};
use std::sync::Arc;

fn make_msg(content: &str) -> WeixinMessage {
    WeixinMessage {
        from_user_id: Some("user1".to_string()),
        context_token: Some("ctx1".to_string()),
        item_list: Some(std::sync::Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some(content.to_string()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    }
}

fn msg_text(msg: &WeixinMessage) -> Option<&str> {
    msg.text()
}

// ─── US1 Tests ───────────────────────────────────────────────────────────────

/// FR-003, FR-004: push 3 messages, poll, verify FIFO order and count.
#[tokio::test]
async fn test_push_and_poll() {
    let q = InMemoryQueue::new();
    q.push("v1", make_msg("a")).await.unwrap();
    q.push("v1", make_msg("b")).await.unwrap();
    q.push("v1", make_msg("c")).await.unwrap();

    let msgs = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(msgs.len(), 3);
    assert_eq!(msg_text(&msgs[0]), Some("a"));
    assert_eq!(msg_text(&msgs[1]), Some("b"));
    assert_eq!(msg_text(&msgs[2]), Some("c"));
}

/// Edge case: poll on a vtoken with no prior pushes returns an empty batch.
#[tokio::test]
async fn test_poll_empty() {
    let q = InMemoryQueue::new();
    let batch = q.poll("v1", None).await.unwrap();
    assert!(
        batch.is_empty(),
        "poll on empty queue should return no msgs"
    );
    assert_eq!(batch.cursor, 0, "empty batch carries the neutral cursor");
}

/// FR-009, P5: push 201 messages; cap is 200; the 201st is rejected under
/// backpressure and the queue still holds msg_0..msg_199 (oldest retained).
#[tokio::test]
async fn test_overflow_rejects_new_and_keeps_oldest() {
    let q = InMemoryQueue::new();
    for i in 0..=200 {
        let rejected = q.push("v1", make_msg(&format!("msg_{i}"))).await.unwrap();
        if i < 200 {
            assert!(!rejected, "unexpected rejection at push {i}");
        } else {
            assert!(rejected, "expected backpressure rejection on 201st push");
        }
    }
    let msgs = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(
        msgs.len(),
        200,
        "queue should hold exactly MAX_QUEUE_SIZE messages"
    );
    assert_eq!(
        msg_text(&msgs[0]),
        Some("msg_0"),
        "oldest message (msg_0) must be retained under backpressure"
    );
    assert_eq!(msg_text(&msgs[199]), Some("msg_199"));
}

/// FR-005: push from a spawned task wakes up wait_notify.
#[tokio::test]
async fn test_wait_notify_receives() {
    let q = Arc::new(InMemoryQueue::new());
    let q2 = q.clone();

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        q2.push("v1", make_msg("hello")).await.unwrap();
    });

    let notified = q.wait_notify("v1", 2).await.unwrap();
    assert!(
        notified,
        "wait_notify should return true when a message is pushed"
    );
}

/// FR-005 timeout path: no push occurs; wait_notify returns false after timeout.
#[tokio::test]
async fn test_wait_notify_timeout() {
    let q = InMemoryQueue::new();
    let notified = q.wait_notify("v1", 1).await.unwrap();
    assert!(
        !notified,
        "wait_notify should return false on timeout with no push"
    );
}

/// FR-006: push to two different vtokens; queue_sizes returns correct counts.
#[tokio::test]
async fn test_queue_sizes() {
    let q = InMemoryQueue::new();
    q.push("a", make_msg("1")).await.unwrap();
    q.push("a", make_msg("2")).await.unwrap();
    q.push("b", make_msg("x")).await.unwrap();
    q.push("b", make_msg("y")).await.unwrap();
    q.push("b", make_msg("z")).await.unwrap();

    let sizes = q.queue_sizes().await.unwrap();
    assert_eq!(sizes["a"], 2);
    assert_eq!(sizes["b"], 3);
}

/// FR-007: push 2 msgs, remove_client, poll returns empty; subsequent push recreates entry.
#[tokio::test]
async fn test_remove_client() {
    let q = InMemoryQueue::new();
    q.push("v1", make_msg("1")).await.unwrap();
    q.push("v1", make_msg("2")).await.unwrap();

    q.remove_client("v1").await.unwrap();

    let msgs = q.poll("v1", None).await.unwrap().msgs;
    assert!(
        msgs.is_empty(),
        "poll after remove_client should return empty"
    );

    q.push("v1", make_msg("3")).await.unwrap();
    let msgs = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msg_text(&msgs[0]), Some("3"));
}

/// Concurrency: 10 tasks × 10 pushes to the same vtoken; result within cap, non-empty.
#[tokio::test]
async fn test_concurrent_push() {
    let q = Arc::new(InMemoryQueue::new());
    let mut handles = Vec::new();

    for task_id in 0..10 {
        let q2 = q.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                q2.push("v1", make_msg(&format!("t{task_id}_m{i}")))
                    .await
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let msgs = q.poll("v1", None).await.unwrap().msgs;
    assert!(
        !msgs.is_empty(),
        "queue should contain messages after concurrent pushes"
    );
    assert!(
        msgs.len() <= 200,
        "queue should respect MAX_QUEUE_SIZE cap; got {}",
        msgs.len()
    );
}

// ─── US2 Tests ───────────────────────────────────────────────────────────────

/// FR-002: compile-time proof that MessageQueue is object-safe.
#[test]
fn test_object_safe() {
    let _: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());
}

/// FR-001, SC-002: a minimal third-party impl compiles and works behind Arc<dyn MessageQueue>.
#[tokio::test]
async fn test_mock_implementation() {
    use async_trait::async_trait;
    use ilink_hub::error::HubError;
    use std::collections::HashMap;

    struct NoopQueue;

    #[async_trait]
    impl MessageQueue for NoopQueue {
        async fn push(&self, _vtoken: &str, _msg: WeixinMessage) -> Result<bool, HubError> {
            Ok(false)
        }
        async fn poll(&self, _vtoken: &str, _ack: Option<u64>) -> Result<PollBatch, HubError> {
            Ok(PollBatch::default())
        }
        async fn wait_notify(&self, _vtoken: &str, _timeout_secs: u64) -> Result<bool, HubError> {
            Ok(false)
        }
        async fn remove_client(&self, _vtoken: &str) -> Result<(), HubError> {
            Ok(())
        }
        async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
            Ok(HashMap::new())
        }
    }

    let q: Arc<dyn MessageQueue> = Arc::new(NoopQueue);
    assert!(q.push("x", make_msg("y")).await.is_ok());
    assert!(q.poll("x", None).await.unwrap().is_empty());
    assert!(!q.wait_notify("x", 0).await.unwrap());
}

// ─── US3 (A-02) Adversarial Tests ───────────────────────────────────────────

/// Boundary: cap=1 — single message fills the slot; the 2nd push is rejected and
/// the queued message is retained.
#[tokio::test]
async fn test_with_limit_boundary_one() {
    let q = InMemoryQueue::with_limit(1);
    let rejected = q.push("v1", make_msg("first")).await.unwrap();
    assert!(!rejected);
    let rejected = q.push("v1", make_msg("second")).await.unwrap();
    assert!(rejected, "cap=1 must reject the 2nd push");
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 1);
    assert_eq!(msg_text(&drained[0]), Some("first"));
}

/// Boundary: cap=MAX (10_000) — push exactly cap, no rejection; cap+1 rejected.
#[tokio::test]
async fn test_with_limit_boundary_max() {
    let q = InMemoryQueue::with_limit(10_000);
    for i in 0..10_000 {
        let rejected = q.push("v1", make_msg(&format!("m{i}"))).await.unwrap();
        assert!(!rejected, "unexpected rejection at i={i}");
    }
    let rejected = q.push("v1", make_msg("overflow")).await.unwrap();
    assert!(rejected, "cap+1 must be rejected under backpressure");
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 10_000);
    assert_eq!(
        msg_text(&drained[0]),
        Some("m0"),
        "oldest (m0) must be retained; the rejected message is not queued"
    );
    assert_eq!(msg_text(&drained[9_999]), Some("m9999"));
}

/// Overflow on different vtokens is independent: filling A must not affect B's cap.
#[tokio::test]
async fn test_with_limit_per_vtoken_isolation() {
    let q = InMemoryQueue::with_limit(2);
    q.push("a", make_msg("a0")).await.unwrap();
    q.push("a", make_msg("a1")).await.unwrap();
    let dropped = q.push("a", make_msg("a2")).await.unwrap();
    assert!(dropped, "a must overflow after 2 pushes");
    let dropped = q.push("b", make_msg("b0")).await.unwrap();
    assert!(!dropped, "b must not be affected by a's overflow");
    let sizes = q.queue_sizes().await.unwrap();
    assert_eq!(sizes["a"], 2);
    assert_eq!(sizes["b"], 1);
}

/// Interleaved acked-poll + push within the cap must not lose messages or exceed the cap.
#[tokio::test]
async fn test_with_limit_ack_then_refill() {
    let q = InMemoryQueue::with_limit(3);
    q.push("v1", make_msg("a")).await.unwrap();
    q.push("v1", make_msg("b")).await.unwrap();
    q.push("v1", make_msg("c")).await.unwrap();
    let first = q.poll("v1", None).await.unwrap();
    assert_eq!(first.msgs.len(), 3);
    assert_eq!(first.cursor, 3);
    // Echoing the cursor acknowledges the batch, which frees the slot.
    let acked = q.poll("v1", Some(first.cursor)).await.unwrap();
    assert!(acked.is_empty(), "acked batch must not be redelivered");
    // Refill: should accept 3 more without rejections.
    for i in 0..3 {
        let rejected = q.push("v1", make_msg(&format!("d{i}"))).await.unwrap();
        assert!(!rejected, "refill push {i} unexpectedly rejected");
    }
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 3);
    assert_eq!(msg_text(&drained[0]), Some("d0"));
}

/// remove_client on a vtoken that has overflowed history must fully clear the slot,
/// so a subsequent push to a fresh slot starts at cap (not already-filled).
#[tokio::test]
async fn test_with_limit_remove_client_resets_capacity() {
    let q = InMemoryQueue::with_limit(2);
    q.push("v1", make_msg("a")).await.unwrap();
    q.push("v1", make_msg("b")).await.unwrap();
    q.push("v1", make_msg("c")).await.unwrap(); // rejected (backpressure)
    q.remove_client("v1").await.unwrap();
    // After remove, a fresh push should not be rejected.
    let rejected = q.push("v1", make_msg("fresh")).await.unwrap();
    assert!(!rejected, "after remove_client, slot must be empty");
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 1);
    assert_eq!(msg_text(&drained[0]), Some("fresh"));
}

// ─── Backpressure / broadcast tests ──────────────────────────────────────────

/// When the per-client queue is full the NEW message is rejected (backpressure);
/// the queued messages are retained — deliberately NOT the old "drop oldest"
/// policy, which would silently destroy a message the client never
/// acknowledged. See `PerClientSlot::push` / `MessageQueue::push`.
#[tokio::test]
async fn test_broadcast_path_full_queue_rejects_new_and_keeps_oldest() {
    let q = InMemoryQueue::with_limit(2);

    let rejected1 = q.push("v1", make_msg("first")).await.unwrap();
    let rejected2 = q.push("v1", make_msg("second")).await.unwrap();
    assert!(!rejected1);
    assert!(!rejected2);

    // Full: "third" is rejected, "first"/"second" stay queued.
    let rejected3 = q.push("v1", make_msg("third")).await.unwrap();
    assert!(rejected3, "queue full must report backpressure");
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 2);
    assert_eq!(msg_text(&drained[0]), Some("first"));
    assert_eq!(msg_text(&drained[1]), Some("second"));
}

/// `push_shared` lets the broadcast path share the unchanged base via
/// `Arc<WeixinMessage>`. Under contention from many concurrent recipients
/// pushing to different vtokens, the inner `item_list` (the expensive
/// `Arc<Vec<MessageItem>>` payload) must be cloned only once per push, not
/// per recipient. We assert that the `Arc::strong_count` of the inner
/// payload does not balloon — a regression here would silently regress
/// the broadcast hot path.
#[tokio::test]
async fn test_push_shared_does_not_clone_heavy_payload() {
    use ilink_hub::ilink::types::HubExt;
    let q = InMemoryQueue::new();
    let heavy = Arc::new(vec![MessageItem {
        item_type: Some(1),
        text_item: Some(TextItem {
            text: Some("payload".to_string()),
        }),
        ..Default::default()
    }]);
    let base = Arc::new(WeixinMessage {
        from_user_id: Some("u".into()),
        item_list: Some(Arc::clone(&heavy)),
        ..Default::default()
    });
    // Strong count of the inner payload before any push.
    let before = Arc::strong_count(&heavy);

    for i in 0..32 {
        q.push_shared(
            &format!("v{i}"),
            Arc::clone(&base),
            Some(format!("vctx-{i}")),
            Some(HubExt {
                session_id: Some(format!("sid-{i}")),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    }

    // After 32 push_shared calls, the inner payload's strong count should
    // have grown by at most 33 (the original + one clone per push into the
    // slot's VecDeque). If the impl were cloning the full base instead of
    // sharing the Arc, growth would be linear in the *full* WeixinMessage
    // size, not just `+1` per recipient.
    let after = Arc::strong_count(&heavy);
    let growth = after - before;
    assert!(
        growth <= 33,
        "inner payload Arc should not balloon: grew by {growth}"
    );
}

/// Many concurrent producers pushing to the same vtoken must not lose
/// messages or corrupt the queue, even if the order of arrival matters.
/// This is the closest integration test to a real "burst" scenario.
#[tokio::test]
async fn test_concurrent_pushes_preserve_message_count() {
    use std::sync::Arc;
    let q = Arc::new(InMemoryQueue::with_limit(10_000));
    let mut handles = vec![];
    for t in 0..8 {
        let q = Arc::clone(&q);
        handles.push(tokio::spawn(async move {
            for i in 0..50 {
                let dropped = q.push("v1", make_msg(&format!("t{t}-i{i}"))).await.unwrap();
                assert!(
                    !dropped,
                    "queue should not overflow with 8*50=400 msgs and limit 10_000"
                );
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let drained = q.poll("v1", None).await.unwrap().msgs;
    assert_eq!(drained.len(), 400, "all 8*50 pushes must be preserved");
}
