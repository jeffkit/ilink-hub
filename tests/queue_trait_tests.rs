use ilink_hub::{hub::queue::InMemoryQueue, ilink::types::InboundMessage, MessageQueue};
use std::sync::Arc;

fn make_msg(content: &str) -> InboundMessage {
    InboundMessage {
        msg_id: format!("test_{content}"),
        from_user: "user1".to_string(),
        chat_id: None,
        chat_type: None,
        msg_type: 1,
        content: Some(content.to_string()),
        context_token: "ctx1".to_string(),
        timestamp: None,
        extra: serde_json::Value::Object(serde_json::Map::new()),
    }
}

// ─── US1 Tests ───────────────────────────────────────────────────────────────

/// FR-003, FR-004: push 3 messages, drain, verify FIFO order and count.
#[tokio::test]
async fn test_push_and_drain() {
    let q = InMemoryQueue::new();
    q.push("v1", make_msg("a")).await.unwrap();
    q.push("v1", make_msg("b")).await.unwrap();
    q.push("v1", make_msg("c")).await.unwrap();

    let msgs = q.drain("v1").await.unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].content, Some("a".to_string()));
    assert_eq!(msgs[1].content, Some("b".to_string()));
    assert_eq!(msgs[2].content, Some("c".to_string()));
}

/// Edge case: drain on a vtoken with no prior pushes returns empty vec.
#[tokio::test]
async fn test_drain_empty() {
    let q = InMemoryQueue::new();
    let msgs = q.drain("v1").await.unwrap();
    assert!(
        msgs.is_empty(),
        "drain on empty queue should return empty vec"
    );
}

/// FR-009, P5: push 201 messages; cap is 200; msg_0 (head) is dropped; result starts at msg_1.
#[tokio::test]
async fn test_overflow_head_drop() {
    let q = InMemoryQueue::new();
    for i in 0..=200 {
        let dropped = q.push("v1", make_msg(&format!("msg_{i}"))).await.unwrap();
        // Only the 201st push (i == 200) should signal overflow.
        if i < 200 {
            assert!(!dropped, "unexpected overflow at push {i}");
        } else {
            assert!(dropped, "expected overflow flag on 201st push");
        }
    }
    let msgs = q.drain("v1").await.unwrap();
    assert_eq!(
        msgs.len(),
        200,
        "queue should hold exactly MAX_QUEUE_SIZE messages"
    );
    assert_eq!(
        msgs[0].content,
        Some("msg_1".to_string()),
        "oldest message (msg_0) should have been head-dropped"
    );
    assert_eq!(msgs[199].content, Some("msg_200".to_string()));
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

/// FR-007: push 2 msgs, remove_client, drain returns empty; subsequent push recreates entry.
#[tokio::test]
async fn test_remove_client() {
    let q = InMemoryQueue::new();
    q.push("v1", make_msg("1")).await.unwrap();
    q.push("v1", make_msg("2")).await.unwrap();

    q.remove_client("v1").await.unwrap();

    let msgs = q.drain("v1").await.unwrap();
    assert!(
        msgs.is_empty(),
        "drain after remove_client should return empty"
    );

    // Push after removal should recreate the entry
    q.push("v1", make_msg("3")).await.unwrap();
    let msgs = q.drain("v1").await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, Some("3".to_string()));
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

    let msgs = q.drain("v1").await.unwrap();
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
        async fn push(&self, _vtoken: &str, _msg: InboundMessage) -> Result<bool, HubError> {
            Ok(false)
        }
        async fn drain(&self, _vtoken: &str) -> Result<Vec<InboundMessage>, HubError> {
            Ok(vec![])
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
    assert!(q.drain("x").await.unwrap().is_empty());
    assert!(!q.wait_notify("x", 0).await.unwrap());
}
