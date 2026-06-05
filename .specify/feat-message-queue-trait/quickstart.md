# Quickstart: MessageQueue Trait Abstraction

**Feature**: `feat/message-queue-trait`  
**Date**: 2026-06-05  
**Audience**: (1) Contributors verifying the implementation, (2) Downstream crate authors implementing a custom backend.

---

## 1. Running the Existing Test Suite

After implementing the feature:

```bash
cd /Users/kongjie/projects/ilink-hub

# Verify formatting
cargo fmt --check

# Verify lints (must produce no warnings)
RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features

# Run all tests (including new queue trait tests)
cargo test

# Run only queue-related tests with output
cargo test queue -- --nocapture
```

All three commands must pass with zero errors before the PR is ready.

---

## 2. Verifying the In-Memory Backend (Default)

Start the hub with no extra configuration — the in-memory backend is selected automatically:

```bash
# No ILINK_QUEUE_BACKEND set → uses InMemoryQueue
cargo run -- serve --database-url sqlite::memory:
```

Expected startup log (approximate):
```
INFO ilink_hub: queue backend initialized backend="memory"
INFO ilink_hub: iLink Hub listening addr="0.0.0.0:8765"
```

---

## 3. Verifying the `ILINK_QUEUE_BACKEND=memory` Explicit Setting

```bash
ILINK_QUEUE_BACKEND=memory cargo run -- serve --database-url sqlite::memory:
```

Expected: Same as above — logs `backend="memory"` at INFO level.

---

## 4. Verifying Startup Failure for `ILINK_QUEUE_BACKEND=redis`

```bash
ILINK_QUEUE_BACKEND=redis cargo run -- serve --database-url sqlite::memory:
```

Expected exit with error (ILINK_REDIS_URL not set):
```
Error: ILINK_QUEUE_BACKEND=redis requires ILINK_REDIS_URL (e.g., redis://localhost:6379)
```

```bash
ILINK_QUEUE_BACKEND=redis ILINK_REDIS_URL=redis://localhost:6379 \
  cargo run -- serve --database-url sqlite::memory:
```

Expected exit with error (Redis not yet implemented):
```
Error: Redis queue backend is not yet implemented in this version. ...
```

---

## 5. Verifying Startup Failure for Unknown Backend

```bash
ILINK_QUEUE_BACKEND=kafka cargo run -- serve --database-url sqlite::memory:
```

Expected:
```
Error: Unknown ILINK_QUEUE_BACKEND="kafka". Supported values: 'memory'. (redis: planned, not yet available)
```

---

## 6. Manual End-to-End Smoke Test

```bash
# Terminal 1: Start hub
cargo run -- serve

# Terminal 2: Register a client
cargo run -- register --name mybot --label "Test Bot"
# → outputs WEIXIN_TOKEN=vctx_<uuid>

export VTOKEN=vctx_<uuid>  # from above

# Terminal 3: Long-poll for updates (30s timeout)
curl -s -X POST http://localhost:8765/ilink/bot/getupdates \
  -H "Authorization: Bearer $VTOKEN" \
  -H "Content-Type: application/json" \
  -d '{"timeout": 5}'
# → {"ret":0,"buf":"","list":null} after 5 seconds (no messages)

# Terminal 4: Check Prometheus metrics show queue size gauge
curl -s http://localhost:8765/metrics | grep queue_size
# → ilink_hub_queue_size{client="mybot"} 0
```

---

## 7. Implementing a Custom Backend (Downstream Crate Authors)

Add `ilink-hub` as a dependency (when published):

```toml
[dependencies]
ilink-hub = { version = "0.1", git = "https://github.com/kongjie/ilink-hub" }
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
```

Implement the trait for your custom struct:

```rust
use async_trait::async_trait;
use ilink_hub::{MessageQueue, HubError};
use ilink_hub::ilink::types::InboundMessage;
use std::collections::HashMap;

pub struct RedisQueue {
    // your Redis connection pool here
    // client: redis::aio::ConnectionManager,
    // prefix: String,
}

#[async_trait]
impl MessageQueue for RedisQueue {
    async fn push(&self, vtoken: &str, msg: InboundMessage) -> Result<(), HubError> {
        // LPUSH to a Redis list keyed by vtoken
        // LTRIM to enforce MAX_QUEUE_SIZE
        // PUBLISH on a channel to wake waiting pollers
        todo!("implement Redis push")
    }

    async fn drain(&self, vtoken: &str) -> Result<Vec<InboundMessage>, HubError> {
        // LRANGE + DEL (or GETDEL in one Lua script)
        todo!("implement Redis drain")
    }

    async fn wait_notify(&self, vtoken: &str, timeout_secs: u64) -> Result<bool, HubError> {
        // SUBSCRIBE to a channel; tokio::time::timeout wrapping the subscribe await
        todo!("implement Redis wait_notify")
    }

    async fn queue_sizes(&self) -> Result<HashMap<String, usize>, HubError> {
        // SCAN for keys matching your prefix; LLEN for each
        todo!("implement Redis queue_sizes")
    }

    async fn remove_client(&self, vtoken: &str) -> Result<(), HubError> {
        // DEL the list key and UNSUBSCRIBE from the notify channel
        todo!("implement Redis remove_client")
    }
}
```

Wire your backend into `HubState`:

```rust
use std::sync::Arc;
use ilink_hub::hub::HubState;
use ilink_hub::ilink::UpstreamClient;
use ilink_hub::store::Store;

// In your binary's main:
let queue: Arc<dyn MessageQueue + Send + Sync> = Arc::new(RedisQueue::new(/* ... */));
let state = HubState::new(upstream, store, queue);
```

That's it. Your `RedisQueue` will be called for all five queue operations. The iLink protocol
behavior and all other hub functionality remain unchanged.

---

## 8. Writing Tests for Your Custom Backend

Use the standard test patterns from `tests/queue_trait_tests.rs` as a reference. The key tests you should replicate for any `MessageQueue` implementation:

```rust
#[tokio::test]
async fn test_push_and_drain() {
    let q = Arc::new(YourBackend::new());
    q.push("vctx_test", make_msg("hello")).await.unwrap();
    q.push("vctx_test", make_msg("world")).await.unwrap();
    let msgs = q.drain("vctx_test").await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content.as_deref(), Some("hello"));
}

#[tokio::test]
async fn test_drain_empty() {
    let q = Arc::new(YourBackend::new());
    let msgs = q.drain("vctx_unknown").await.unwrap();
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn test_wait_notify_timeout() {
    let q = Arc::new(YourBackend::new());
    let notified = q.wait_notify("vctx_empty", 1).await.unwrap();
    assert!(!notified, "should timeout, not receive notification");
}

#[tokio::test]
async fn test_wait_notify_receives() {
    let q = Arc::new(YourBackend::new());
    let q2 = q.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        q2.push("vctx_a", make_msg("ping")).await.unwrap();
    });
    let notified = q.wait_notify("vctx_a", 2).await.unwrap();
    assert!(notified, "should be notified by spawned push");
    let msgs = q.drain("vctx_a").await.unwrap();
    assert_eq!(msgs.len(), 1);
}
```

---

## 9. Checking Object Safety

The following code must compile (it is included as `test_object_safe` in the test suite):

```rust
fn assert_object_safe() {
    let _: std::sync::Arc<dyn ilink_hub::MessageQueue + Send + Sync> =
        std::sync::Arc::new(ilink_hub::hub::InMemoryQueue::new());
}
```

If this fails to compile, the trait definition violates object safety — check `async-trait` attribute is applied.
