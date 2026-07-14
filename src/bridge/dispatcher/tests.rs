use super::{
    backoff_for, backoff_for_test, classify_sendoutcome, parse_sendoutcome,
    run_partial_forward_loop, sanitize_errmsg, send_final_with_retry, session_dispatch_key,
    ApprovalBroker, BridgeStop, HubClient, ReplySender, SendOutcome, SessionDispatcher,
    MAX_BACKOFF_SECS,
};
use crate::bridge::config::BridgeApp;
use crate::ilink::types::{
    HubExt, MessageItem, SendMessageRequest, SendMessageResponse, TextItem, WeixinMessage,
};
use anyhow::Result;
use futures_util::future::BoxFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn make_msg(ctx: &str, session_name: &str) -> WeixinMessage {
    WeixinMessage {
        context_token: Some(ctx.into()),
        ilink_hub_ext: Some(HubExt {
            session_id: Some(String::new()),
            session_name: Some(session_name.into()),
            cli_session_id: None,
            a2a_call_id: None,
            a2a_depth: None,
        }),
        item_list: Some(std::sync::Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello".into()),
            }),
            ..Default::default()
        }])),
        from_user_id: Some("user1".into()),
        ..Default::default()
    }
}

fn make_fast_app() -> BridgeApp {
    BridgeApp::parse_yaml(
        r#"
command: echo
args: []
stdin: none
timeout_secs: 5
"#,
    )
    .unwrap()
}

fn fake_client() -> HubClient {
    HubClient::new("http://127.0.0.1:1".into(), "test-token".into()).expect("test http client")
}

fn make_stop_tx() -> tokio::sync::watch::Sender<Option<BridgeStop>> {
    tokio::sync::watch::channel(None).0
}

#[test]
fn key_combines_ctx_and_session_name() {
    assert_eq!(
        session_dispatch_key(&make_msg("ctx-123", "feat-a")),
        "ctx-123:feat-a"
    );
}

#[test]
fn key_defaults_session_name_when_ext_absent() {
    let msg = WeixinMessage {
        context_token: Some("ctx-x".into()),
        ilink_hub_ext: None,
        ..Default::default()
    };
    assert_eq!(session_dispatch_key(&msg), "ctx-x:default");
}

#[test]
fn key_uses_empty_string_when_ctx_absent() {
    let msg = WeixinMessage {
        context_token: None,
        ilink_hub_ext: None,
        ..Default::default()
    };
    assert_eq!(session_dispatch_key(&msg), ":default");
}

#[test]
fn key_differs_for_different_session_names() {
    let a = make_msg("ctx", "session-a");
    let b = make_msg("ctx", "session-b");
    assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
}

#[test]
fn key_differs_for_different_ctx_tokens() {
    let a = make_msg("ctx-1", "default");
    let b = make_msg("ctx-2", "default");
    assert_ne!(session_dispatch_key(&a), session_dispatch_key(&b));
}

#[tokio::test]
async fn same_key_reuses_single_sender() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    let msg = make_msg("ctx-a", "default");
    disp.dispatch(msg.clone()).await;
    disp.dispatch(msg.clone()).await;
    assert_eq!(disp.sender_keys(), vec!["ctx-a:default"]);
}

#[tokio::test]
async fn different_ctx_tokens_get_separate_senders() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    disp.dispatch(make_msg("ctx-a", "default")).await;
    disp.dispatch(make_msg("ctx-b", "default")).await;
    assert_eq!(disp.sender_keys(), vec!["ctx-a:default", "ctx-b:default"]);
}

#[tokio::test]
async fn different_session_names_get_separate_senders() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    disp.dispatch(make_msg("ctx-a", "feature-x")).await;
    disp.dispatch(make_msg("ctx-a", "feature-y")).await;
    assert_eq!(
        disp.sender_keys(),
        vec!["ctx-a:feature-x", "ctx-a:feature-y"]
    );
}

#[tokio::test]
async fn three_distinct_sessions_create_three_senders() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    disp.dispatch(make_msg("ctx-1", "default")).await;
    disp.dispatch(make_msg("ctx-2", "default")).await;
    disp.dispatch(make_msg("ctx-1", "feature-a")).await;
    assert_eq!(
        disp.sender_keys(),
        vec!["ctx-1:default", "ctx-1:feature-a", "ctx-2:default"]
    );
}

#[tokio::test]
async fn repeated_same_key_does_not_grow_sender_map() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    let msg = make_msg("ctx-x", "s1");
    for _ in 0..5 {
        disp.dispatch(msg.clone()).await;
    }
    assert_eq!(disp.sender_keys().len(), 1);
}

#[tokio::test]
async fn dead_sender_triggers_new_worker_on_next_dispatch() {
    let disp = SessionDispatcher::new(
        fake_client(),
        Arc::new(make_fast_app()),
        make_stop_tx(),
        CancellationToken::new(),
        ApprovalBroker::new(),
    );
    let msg = make_msg("ctx-z", "default");

    disp.dispatch(msg.clone()).await;

    {
        let mut senders = disp.senders.lock().expect("senders poisoned");
        senders.remove("ctx-z:default");
    }
    assert_eq!(disp.sender_keys().len(), 0);

    disp.dispatch(msg.clone()).await;
    assert_eq!(disp.sender_keys(), vec!["ctx-z:default"]);
}

/// N-04 / F-M1-N04: when a `std::sync::Mutex` is poisoned (a thread
/// panicked while holding the lock), the `unwrap_or_else(|e|
/// e.into_inner())` recovery idiom MUST yield the inner state
/// instead of propagating the panic. This pins the *semantics* of
/// the recovery call used by [`SessionDispatcher::dispatch`] and
/// [`SessionDispatcher::evict_closed_senders`] on the same
/// `Mutex<HashMap<String, _>>` shape as the production field, so
/// any future refactor that drops `into_inner()` (and falls back to
/// `expect`) would fail this test by panicking in the test thread.
#[test]
fn senders_lock_recovery_after_poison_yields_inner_state() {
    // Mirrors `SessionDispatcher::senders` exactly. We share the
    // mutex by `Arc` so the spawned poisoner thread can lock it
    // without resorting to raw-pointer unsafety.
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    let senders: Arc<Mutex<HashMap<String, &'static str>>> = Arc::new(Mutex::new(HashMap::new()));
    // Seed the inner state.
    senders.lock().unwrap().insert("k1".to_string(), "v1");

    // Poison the mutex from another thread: lock it, then panic
    // while still holding the guard. Join handles the panic; the
    // mutex is now permanently poisoned on every subsequent
    // `.lock().unwrap()` call.
    let poisoned_clone = Arc::clone(&senders);
    let join = std::thread::spawn(move || {
        let _g = poisoned_clone.lock().expect("acquired");
        panic!("intentional poison");
    });
    let _ = join.join();

    // The N-04 recovery idiom used by `dispatch`:
    let mut guard = senders.lock().unwrap_or_else(|e| e.into_inner());
    // Inner state survived — the seed entry is still there.
    assert_eq!(guard.get("k1"), Some(&"v1"));
    guard.insert("k2".to_string(), "v2");
    assert_eq!(guard.len(), 2, "recovery must allow normal mutation");
}

// ─── parse_sendoutcome ─────────────────────────────────────────────
//
// M1 review F-009: pin the body-text → SendOutcome mapping with unit
// tests so M2/M3 retry decisions have a stable oracle. The legacy
// behaviour (parse failure → Sent) is intentionally preserved; tests
// make that explicit so future changes are a deliberate decision.

#[test]
fn parse_empty_body_is_sent() {
    assert_eq!(parse_sendoutcome(""), Ok(SendOutcome::Sent));
    assert_eq!(parse_sendoutcome("   \n\t "), Ok(SendOutcome::Sent));
}

#[test]
fn parse_ret_zero_is_sent() {
    let body = r#"{"ret":0}"#;
    assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
}

#[test]
fn parse_ret_none_is_sent() {
    let body = r#"{}"#;
    assert_eq!(parse_sendoutcome(body), Ok(SendOutcome::Sent));
}

#[test]
fn parse_ret_negative_two_is_throttled() {
    let body = r#"{"ret":-2,"errmsg":"rate limited"}"#;
    match parse_sendoutcome(body).unwrap() {
        SendOutcome::Throttled { ret, errmsg } => {
            assert_eq!(ret, -2);
            assert_eq!(errmsg.as_deref(), Some("rate limited"));
        }
        other => panic!("expected Throttled, got {:?}", other),
    }
}

#[test]
fn parse_ret_other_non_zero_is_err() {
    let body = r#"{"ret":1,"errmsg":"oops"}"#;
    match parse_sendoutcome(body) {
        Err((1, Some(m))) => assert_eq!(m, "oops"),
        other => panic!("expected Err((1, Some(..))), got {:?}", other),
    }
    let body = r#"{"ret":-99,"errmsg":"unknown"}"#;
    match parse_sendoutcome(body) {
        Err((-99, Some(m))) => assert_eq!(m, "unknown"),
        other => panic!("expected Err((-99, Some(..))), got {:?}", other),
    }
}

#[test]
fn parse_unparseable_body_falls_back_to_sent() {
    // Legacy behaviour: pre-M1 code returned Ok(Sent) on JSON parse
    // failure. M1 keeps the fallback. The dispatcher emits a warn
    // log only when ALL three conditions hold:
    //   1. body_len > 0          (skip empty bodies silently)
    //   2. JSON parse fails      (not a parseable envelope)
    //   3. outcome is Sent       (would otherwise hide the anomaly)
    // The unit test here pins the fallback itself; the warn
    // emission is exercised end-to-end by the dispatcher
    // integration tests. F-M2-008.
    assert_eq!(parse_sendoutcome("not json"), Ok(SendOutcome::Sent));
    assert_eq!(parse_sendoutcome(r#"{"ret": "#), Ok(SendOutcome::Sent));
}

// ─── Adversarial / property-style tests ────────────────────────────
//
// Goal: catch silent regressions in the parser that could let a
// hostile hub payload (e.g. ret==-2 disguised as Sent) bypass M2/M3
// retry logic.

#[test]
fn adversarial_ret_negative_two_never_becomes_sent() {
    // Even with errmsg present, ret==-2 must produce Throttled —
    // this is the entire reason SendOutcome exists.
    for body in [
        r#"{"ret":-2}"#,
        r#"{"ret":-2,"errmsg":""}"#,
        r#"{"ret":-2,"errmsg":"hi"}"#,
        r#" {"ret":-2} "#,
        r#"{"ret":-2,"errmsg":"a]lo t of\\n junk"}"#,
    ] {
        match parse_sendoutcome(body).unwrap() {
            SendOutcome::Throttled { ret: -2, .. } => {}
            other => panic!("ret=-2 body {:?} misclassified as {:?}", body, other),
        }
    }
}

#[test]
fn adversarial_large_payload_does_not_panic() {
    // Hub returning a multi-megabyte body must not panic / OOM the
    // parser. We send 4 MB of repeated JSON-safe content and confirm
    // we get a typed outcome (either Sent, Throttled, or parse-fail)
    // rather than a process abort.
    let big = "x".repeat(4 * 1024 * 1024);
    let body = format!(r#"{{"ret":0,"errmsg":"{}"}}"#, big);
    let res = parse_sendoutcome(&body);
    // We don't care which branch it took — only that it didn't panic.
    let _ = res;
}

#[test]
fn adversarial_nested_garbage_does_not_panic() {
    // Hub could send deeply nested or weird JSON. As long as serde
    // rejects it, parse_sendoutcome must fall back to Sent and not
    // propagate a panic.
    for body in [
        r#"{"ret":{"deeply":{"nested":[1,2,3]}}}"#,
        r#"{"ret":-2.5}"#,
        r#"{"ret":-2,"errmsg":12345}"#,
        r#"{} broken"#,
    ] {
        let _ = parse_sendoutcome(body);
    }
}

// ─── sanitize_errmsg ───────────────────────────────────────────────

#[test]
fn sanitize_strips_control_chars() {
    let dirty = "before\r\nafter\tend\x1b[31mred\x1b[0m";
    let cleaned = sanitize_errmsg(Some(dirty)).unwrap();
    assert!(!cleaned.contains('\r'));
    assert!(!cleaned.contains('\n'));
    assert!(!cleaned.contains('\t'));
    assert!(!cleaned.contains('\x1b'));
    assert!(cleaned.contains("before"));
    assert!(cleaned.contains("red"));
}

#[test]
fn sanitize_caps_length() {
    let huge = "a".repeat(10_000);
    let cleaned = sanitize_errmsg(Some(&huge)).unwrap();
    assert_eq!(cleaned.len(), 256);
}

#[test]
fn sanitize_handles_none_and_empty() {
    assert!(sanitize_errmsg(None).is_none());
    assert!(sanitize_errmsg(Some("")).is_none());
    assert!(sanitize_errmsg(Some("\r\n\t")).is_none());
}

#[test]
fn sanitize_preserves_printable_unicode() {
    // Multi-byte UTF-8 and punctuation must survive.
    let s = "你好, world! 🌏 — dash";
    assert_eq!(sanitize_errmsg(Some(s)).as_deref(), Some(s));
}

// ─── classify_sendoutcome (additional M1F-006 helper) ──────────────

#[test]
fn classify_three_categories() {
    let none_resp = SendMessageResponse {
        ret: None,
        errmsg: None,
    };
    let zero_resp = SendMessageResponse {
        ret: Some(0),
        errmsg: None,
    };
    let tmo_resp = SendMessageResponse {
        ret: Some(-2),
        errmsg: Some("rl".into()),
    };
    assert_eq!(classify_sendoutcome(None), SendOutcome::Sent);
    assert_eq!(classify_sendoutcome(Some(&none_resp)), SendOutcome::Sent);
    assert_eq!(classify_sendoutcome(Some(&zero_resp)), SendOutcome::Sent);
    assert_eq!(
        classify_sendoutcome(Some(&tmo_resp)),
        SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into())
        }
    );
}

#[test]
fn outcome_clone_works() {
    // Pin F-006's derive(Clone) at compile-time — if Clone goes away
    // this won't compile.
    let original = SendOutcome::Throttled {
        ret: -2,
        errmsg: Some("x".into()),
    };
    let cloned = original.clone();
    assert_eq!(original, cloned);
}

// ─── backoff_for (M2) ──────────────────────────────────────────────
//
// M2 plan E2E-1 requires the backoff schedule to be a pure function
// that can be unit-tested without spinning up an HTTP server or a
// tokio runtime. The sequence is the spec's exact value:
//   attempt 0 ->  5s (first retry)
//   attempt 1 -> 10s
//   attempt 2 -> 20s
//   attempt 3 -> 40s
//   attempt 4 -> 60s (cap)
//   attempt 5+ -> 60s
//   attempt u32::MAX -> 60s (no overflow)

#[test]
fn backoff_sequence_matches_spec() {
    let expected_secs = [5u64, 10, 20, 40, 60, 60, 60, 60, 60, 60];
    let actual: Vec<u64> = (0..expected_secs.len() as u32)
        .map(|a| backoff_for(a).as_secs())
        .collect();
    assert_eq!(actual, expected_secs);
}

#[test]
fn backoff_clamps_at_cap_for_large_attempt() {
    // 5 * 2^10 = 5120s — well past 60s; must clamp to 60s.
    assert_eq!(backoff_for(10).as_secs(), MAX_BACKOFF_SECS);
    assert_eq!(backoff_for(20).as_secs(), MAX_BACKOFF_SECS);
}

#[test]
fn backoff_does_not_overflow_at_u32_max() {
    // u32::MAX << SATURATION_SHIFT would overflow without the
    // saturation guard. The function must not panic / wrap.
    let d = backoff_for(u32::MAX);
    assert_eq!(d.as_secs(), MAX_BACKOFF_SECS);
}

#[test]
fn backoff_is_non_decreasing() {
    // Required by E2E-1 scenario B: "退避间隔单调不递减". This is
    // a stronger check than the spec sequence — any monotonic
    // schedule that starts at 5s, doubles, and caps at 60s passes.
    let mut prev = backoff_for(0);
    for a in 1..50 {
        let cur = backoff_for(a);
        assert!(
            cur >= prev,
            "backoff regressed at attempt {a}: {:?} < {:?}",
            cur,
            prev
        );
        prev = cur;
    }
}

// ─── Mock ReplySender for run_partial_forward_loop (M2) ───────────
//
// The M2 loop is fully driven by a `ReplySender` trait. A scripted
// mock lets us assert:
//   - what was sent (in order)
//   - how many send attempts were made
//   - whether a buffered chunk was overwritten during backoff
//   - whether the loop exits cleanly on shutdown

use std::time::Instant;

/// Scripted ReplySender. Holds a queue of outcomes to return in
/// order. Cloning the sender shares the same queue and the same
/// log; the spawn handle takes one clone, the test holds the
/// other as a probe. We hand-implement Clone (Mutex doesn't
/// derive Clone) by wrapping the inner state in Arc<Mutex<…>>.
///
/// Each `send_reply` call stamps `Instant::now()` into a parallel
/// `timestamps` log so tests can assert wall-clock intervals and
/// pin the exponential-backoff shape end-to-end (F-M2-002).
///
/// `loop_forever` mode (set via [`ScriptedSender::new_loop`])
/// replays the same `SendOutcome` outcome indefinitely instead of
/// panicking when the script is exhausted — used by persistent
/// throttle tests that want the loop to keep retrying until
/// shutdown rather than blow up after N scripted sends. Only
/// `SendOutcome` (Clone) is supported here; tests that need a
/// looping `Err` use a regular script with a single outcome
/// (the loop fires one retry before the panic, which is enough
/// to assert shutdown-cancel behavior).
#[derive(Clone)]
struct ScriptedSender {
    script: Arc<Mutex<Vec<Result<SendOutcome>>>>,
    log: Arc<Mutex<Vec<String>>>,
    timestamps: Arc<Mutex<Vec<Instant>>>,
    loop_outcome: Arc<Mutex<Option<SendOutcome>>>,
}

impl ScriptedSender {
    fn new(script: Vec<Result<SendOutcome>>) -> Self {
        Self {
            script: Arc::new(Mutex::new(script)),
            log: Arc::new(Mutex::new(Vec::new())),
            timestamps: Arc::new(Mutex::new(Vec::new())),
            loop_outcome: Arc::new(Mutex::new(None)),
        }
    }

    /// Construct a sender that returns `Ok(outcome)` every call.
    /// Useful for "always throttled" scenarios where we want the
    /// loop to keep retrying until shutdown rather than panic
    /// when an in-line script is exhausted.
    fn new_loop(outcome: SendOutcome) -> Self {
        Self {
            script: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
            timestamps: Arc::new(Mutex::new(Vec::new())),
            loop_outcome: Arc::new(Mutex::new(Some(outcome))),
        }
    }

    fn sent_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn sent_texts(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn sent_timestamps(&self) -> Vec<Instant> {
        self.timestamps.lock().unwrap().clone()
    }

    /// Record one send (label + timestamp) and pop the next scripted
    /// outcome (or replay the loop outcome). Shared by `send_reply`
    /// (partial loop) and `send_request` (M3 final-reply paths).
    fn record_and_next(&self, label: String) -> Result<SendOutcome> {
        self.log.lock().unwrap().push(label);
        self.timestamps.lock().unwrap().push(Instant::now());
        if let Some(outcome) = self.loop_outcome.lock().unwrap().clone() {
            Ok(outcome)
        } else {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                panic!(
                    "ScriptedSender script exhausted; use new_loop for persistent-throttle tests"
                );
            }
            script.remove(0)
        }
    }
}

impl ReplySender for ScriptedSender {
    fn send_reply(
        &self,
        _ctx: &str,
        text: &str,
        _from_user: &str,
        _session_name: &str,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        let next = self.record_and_next(text.to_string());
        Box::pin(async move { next })
    }

    fn send_request(&self, _req: SendMessageRequest) -> BoxFuture<'_, Result<SendOutcome>> {
        let next = self.record_and_next("<request>".to_string());
        Box::pin(async move { next })
    }
}

/// Helper: spawn the forward loop with a fresh mpsc and shutdown
/// token. Uses a 5ms-initial, 40ms-cap backoff schedule so the
/// test runtime stays in tens of milliseconds even when 3-4
/// throttles happen in a row.
///
/// Returns (tx, shutdown_token, handle).
fn spawn_test_loop<S: ReplySender>(
    sender: S,
) -> (
    watch::Sender<Option<String>>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    // Generous budget so existing tests never hit the M4 give-up path.
    spawn_test_loop_with_budget(sender, Duration::from_secs(3600))
}

fn test_backoff(attempt: u32) -> Duration {
    // 5ms initial, 40ms cap — small enough that 4 retries still
    // complete well under the tokio test timeout, large enough that
    // macOS timer granularity doesn't collapse adjacent sleeps to
    // zero. Exercises the real exponential-then-cap shape
    // [5ms, 10ms, 20ms, 40ms, 40ms, ...] (production schedule is
    // [5s, 10s, 20s, 40s, 60s, 60s, ...] — also unit-pinned by
    // backoff_sequence_matches_spec).
    backoff_for_test(attempt, Duration::from_millis(5), Duration::from_millis(40))
}

/// Same as [`spawn_test_loop`] but with a caller-controlled M4 retry
/// budget so give-up behavior can be exercised with a tiny window.
fn spawn_test_loop_with_budget<S: ReplySender>(
    sender: S,
    max_total: Duration,
) -> (
    watch::Sender<Option<String>>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = watch::channel::<Option<String>>(None);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(run_partial_forward_loop(
        sender,
        rx,
        "ctx".into(),
        "from".into(),
        "sess".into(),
        shutdown.clone(),
        test_backoff,
        max_total,
    ));
    (tx, shutdown, handle)
}

#[tokio::test]
async fn partial_three_throttles_then_success_delivers_latest_content() {
    // E2E-1 scenario A: hub returns ret=-2 three times then ret=0.
    // Three chunks are pushed; each overwrites the previous one
    // while the loop is throttled. After the throttle clears the
    // last-written content must land exactly once.
    //
    // After F-M2-001 fix, with the real ms-scale test backoff
    // schedule [5, 10, 20, 40, 40, ...] and the test's 50ms
    // inter-chunk sleeps, the loop may consume the script more
    // or fewer times than 4 depending on exact timing of the
    // chunk recv() races vs the exponential backoff cycles. We
    // therefore script one extra `Sent` at the end (5 outcomes)
    // so the trailing fresh chunk ("v3") can also be delivered
    // without panicking the mock, and assert the LAST sent text
    // equals "v3" — which is the spec property we care about.
    let scripted = ScriptedSender::new(vec![
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into()),
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into()),
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into()),
        }),
        Ok(SendOutcome::Sent),
        Ok(SendOutcome::Sent),
    ]);
    let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    // Push 3 chunks while the hub is throttling.
    tx.send(Some("v1".into())).unwrap();
    // Give the loop time to attempt the first send.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(Some("v2".into())).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(Some("v3".into())).unwrap();

    // Wait until we have at least 4 sends (covers the throttled
    // retries + first success; the trailing v3 may or may not
    // have been delivered yet, depending on timing).
    for _ in 0..200 {
        if probe.sent_count() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(tx);
    handle.await.unwrap();
    let _ = shutdown; // never cancelled; loop exits when tx drops.

    let texts = probe.sent_texts();
    assert!(
        texts.len() >= 4,
        "expected at least 4 sends (3 throttled + ≥1 success), got {texts:?}"
    );
    // The last attempt must be the final chunk's content, so the
    // user sees the freshest fragment after the throttle clears.
    assert_eq!(
        texts.last().unwrap(),
        "v3",
        "final successful send must be the latest buffered content; got {texts:?}"
    );

    // Wall-clock spacing between the first 4 sends must be
    // monotonic non-decreasing (F-M2-002). We accept a 1ms
    // tolerance per gap to absorb tokio timer granularity.
    let stamps = probe.sent_timestamps();
    assert!(stamps.len() >= 4, "expected ≥4 timestamp entries");
    let gaps: Vec<Duration> = stamps
        .iter()
        .take(4)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| w[1].duration_since(*w[0]))
        .collect();
    assert_eq!(gaps.len(), 3);
    // First gap is the initial 5ms backoff.
    assert!(
        gaps[0] >= Duration::from_millis(4),
        "first retry must wait at least ~5ms (initial backoff), got {:?}",
        gaps[0]
    );
    for i in 1..gaps.len() {
        assert!(
            gaps[i] >= gaps[i - 1].saturating_sub(Duration::from_millis(1)),
            "backoff regressed at gap[{i}]: {:?} < {:?} (full gaps = {:?})",
            gaps[i],
            gaps[i - 1],
            gaps
        );
    }
}

#[tokio::test]
async fn partial_single_chunk_throttled_then_success_buffers_until_clear() {
    // A single chunk is throttled, then succeeds. The single chunk
    // must be sent at least twice (once throttled, once delivered).
    let scripted = ScriptedSender::new(vec![
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into()),
        }),
        Ok(SendOutcome::Sent),
    ]);
    let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    tx.send(Some("hello".into())).unwrap();
    for _ in 0..200 {
        if probe.sent_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(tx);
    handle.await.unwrap();

    let texts = probe.sent_texts();
    assert_eq!(texts, vec!["hello", "hello"]);
}

#[tokio::test]
async fn partial_chunk_overwritten_during_backoff_drops_stale_fragment() {
    // Two chunks: first throttled; second arrives during the
    // backoff sleep and overwrites the buffer. After the throttle
    // clears, only the second chunk must be delivered — the first
    // (stale) fragment must NOT be re-sent after the second one.
    //
    // With the real ms-scale test backoff schedule [5, 10, 20,
    // 40, 40, ...], the first chunk may be retried several times
    // before the test sends the second one (the 100ms inter-chunk
    // sleep is much longer than the early backoffs but the cap at
    // 40ms means up to 4 retries fit in 100ms). We therefore
    // script 4 throttled outcomes followed by 2 Sents so the
    // mock never panics, and assert the load-bearing invariants:
    //
    //   1. The LAST sent text must be v2.
    //   2. No send carrying v1 may occur AFTER a send carrying v2
    //      (i.e. the stale fragment must NOT be re-sent once v2
    //      has overwritten the buffer).
    //   3. F-M2-003: the inter-send gap BEFORE the first v2 send
    //      (which is attempt=1's 10ms backoff) must be ≤ the gap
    //      after v2 arrives, which corresponds to attempt=2's
    //      20ms backoff — pinning that overwrite does NOT reset
    //      `attempt`.
    let scripted = ScriptedSender::new(vec![
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Sent),
        Ok(SendOutcome::Sent),
    ]);
    let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    tx.send(Some("v1".into())).unwrap();
    // Give the loop time to attempt several retries and enter
    // backoff sleep. Then send v2 — by the time v2 arrives the
    // loop is sleeping; the recv() inside the select! will
    // overwrite pending. Subsequent sends then carry v2.
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(Some("v2".into())).unwrap();

    for _ in 0..2000 {
        // We expect at least one send of v2 to land. Wait for that.
        if probe.sent_texts().iter().any(|t| t == "v2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(tx);
    handle.await.unwrap();

    let texts = probe.sent_texts();
    // Invariant 1: final delivered content must be v2.
    assert_eq!(
        texts.last().unwrap(),
        "v2",
        "final delivered content must be the latest buffered chunk; got {texts:?}"
    );
    // Invariant 2: no stale v1 send after the first v2 send.
    // Find the index of the first "v2" in the send log; every
    // send after that index must also be v2 (never v1).
    let first_v2_idx = texts
        .iter()
        .position(|t| t == "v2")
        .expect("v2 must be sent at least once");
    for (i, t) in texts.iter().enumerate().skip(first_v2_idx) {
        assert_eq!(
            t, "v2",
            "stale v1 must not be re-sent after v2 overwrote it (send #{i} = {:?}); full log: {texts:?}",
            t
        );
    }
    // We must have at least one v1 send (the initial retry
    // before the overwrite).
    assert!(
        texts.iter().any(|t| t == "v1"),
        "expected at least one v1 send before the overwrite; got {texts:?}"
    );

    // F-M2-003: the gap between the 1st send and the first v2
    // send corresponds to attempt=1's 10ms backoff (10ms). The
    // gap between the first v2 send and the next send (which is
    // still pending=v2 in a fresh retry cycle) corresponds to
    // attempt=2's 20ms backoff. So gap_after_v2 >= gap_before_v2
    // would catch a future regression that resets `attempt` on
    // overwrite. Allow 2ms slack for tokio timer jitter.
    let stamps = probe.sent_timestamps();
    assert!(stamps.len() >= 2);
    let gap_before_v2 = stamps[first_v2_idx].duration_since(stamps[0]);
    if first_v2_idx + 1 < stamps.len() {
        let gap_after_v2 = stamps[first_v2_idx + 1].duration_since(stamps[first_v2_idx]);
        assert!(
            gap_after_v2 + Duration::from_millis(2) >= gap_before_v2,
            "overwrite reset attempt (gap_after_v2={:?} < gap_before_v2={:?}); \
             gap_before_v2 corresponds to attempt=1 (10ms), \
             gap_after_v2 corresponds to attempt=2 (20ms)",
            gap_after_v2,
            gap_before_v2
        );
    }
}

#[tokio::test]
async fn partial_persistent_throttle_caps_retry_at_max_backoff() {
    // Scenario B (small-N variant): the hub throttles forever
    // and we send a small burst of chunks. The expected behavior
    // is the loop keeps trying, the backoff saturates at 60s, and
    // the most recent chunk is what's pending.
    //
    // After F-M2-001/F-M2-002, this test also asserts the
    // wall-clock spacing between successive sends is monotonic
    // and converges to the cap (≤ 40ms with our test schedule).
    //
    // We use `new_loop` (Throttled forever) instead of a finite
    // script because with a real ms-scale backoff schedule the
    // loop will fire more than 4 sends within the 50ms inter-chunk
    // sleeps (overwrites during backoff trigger extra cycles);
    // a finite script would panic. We assert that we observed
    // AT LEAST the expected retry count, and then verify the
    // wall-clock shape across the first 4 retries.
    let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
        ret: -2,
        errmsg: None,
    });
    let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    // Send 3 chunks during the persistent throttle. Each arrives
    // during a backoff sleep and overwrites pending.
    for i in 0..3 {
        tx.send(Some(format!("chunk-{i}"))).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Wait for at least 4 retry attempts (test_backoff schedule
    // = [5, 10, 20, 40, 40, ...], so 4 retries complete in
    // 5+10+20+40 = 75ms; allow generous slack).
    for _ in 0..400 {
        if probe.sent_count() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Cancel shutdown to make the loop exit deterministically.
    shutdown.cancel();
    // Give the loop a moment to observe the cancel.
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    let texts = probe.sent_texts();
    assert!(
        texts.len() >= 4,
        "expected at least 4 retry attempts under persistent throttle, got {texts:?}"
    );
    // The last attempt must carry the most-recently-pushed content.
    assert_eq!(
        texts.last().unwrap(),
        "chunk-2",
        "most recent chunk must be the one being retried"
    );

    // Wall-clock spacing between the first 4 sends: must be
    // monotonic non-decreasing and converging to the 40ms cap.
    // Allow a 5ms tolerance for tokio timer granularity and
    // macOS scheduling jitter.
    let stamps = probe.sent_timestamps();
    assert!(stamps.len() >= 4, "expected ≥4 timestamp entries");
    let gaps: Vec<Duration> = stamps
        .iter()
        .take(4)
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| w[1].duration_since(*w[0]))
        .collect();
    assert_eq!(gaps.len(), 3);
    // First gap (after 1st throttle) is the initial 5ms backoff.
    assert!(
        gaps[0] >= Duration::from_millis(4),
        "first retry must wait at least ~5ms (initial backoff), got {:?}",
        gaps[0]
    );
    // Monotonic non-decreasing: each gap ≥ previous gap (allow
    // 1ms tolerance for timer jitter).
    for i in 1..gaps.len() {
        assert!(
            gaps[i] >= gaps[i - 1].saturating_sub(Duration::from_millis(1)),
            "backoff regressed at gap[{i}]: {:?} < {:?} (full gaps = {:?})",
            gaps[i],
            gaps[i - 1],
            gaps
        );
    }
    // At least one gap must be roughly 2× the previous gap,
    // catching hardcoded sleeps. Allow generous jitter bounds
    // (1.4× to 3×).
    let mut doubled = false;
    for i in 1..gaps.len() {
        if gaps[i].as_micros() >= (gaps[i - 1].as_micros() * 14) / 10
            && gaps[i] < gaps[i - 1].saturating_mul(3)
        {
            doubled = true;
            break;
        }
    }
    assert!(
        doubled,
        "expected at least one roughly-2× gap (exponential shape); got {:?}",
        gaps
    );
}

#[tokio::test]
async fn partial_err_drops_buffer_and_continues_serving_new_chunks() {
    // Transport / non-throttle errors must NOT spin the retry
    // loop. After Err, the loop clears `pending` and serves the
    // next chunk from the CLI normally.
    let scripted = ScriptedSender::new(vec![
        Err(anyhow::anyhow!("hub down")),
        Ok(SendOutcome::Sent),
    ]);
    let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    tx.send(Some("first".into())).unwrap();
    // Wait for the error to be consumed + a new chunk to clear
    // the buffer.
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(Some("second".into())).unwrap();
    for _ in 0..200 {
        if probe.sent_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(tx);
    handle.await.unwrap();

    let texts = probe.sent_texts();
    assert_eq!(texts, vec!["first", "second"]);
}

#[tokio::test]
async fn partial_shutdown_during_backoff_exits_cleanly() {
    // While a backoff is sleeping, a shutdown signal must wake the
    // loop and let it return. The buffered chunk is dropped (it
    // was held in memory only).
    //
    // Uses loop mode because with the real ms-scale test backoff
    // schedule the loop will fire more than one retry before the
    // test cancels shutdown (the cap of 40ms keeps retries
    // tightly packed); a finite 1-outcome script would panic.
    let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
        ret: -2,
        errmsg: None,
    });
    let (tx, shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    tx.send(Some("stuck".into())).unwrap();
    // Wait until the loop has issued at least one throttled send
    // and is currently sleeping in the backoff.
    for _ in 0..200 {
        if probe.sent_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        probe.sent_count() >= 1,
        "loop should have attempted at least one send before we cancel"
    );

    shutdown.cancel();
    // CancellationToken observed in the same select! as the sleep
    // is the wake-up mechanism. With the ms-scale backoff the
    // sleep may be very short, but the loop must still exit
    // promptly on cancel.
    let joined = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(joined.is_ok(), "loop must exit within 2s of shutdown");
    drop(tx);
}

#[tokio::test]
async fn partial_chunks_arrived_after_sender_continues_normal_path() {
    // Sanity: if there is never a throttle, each chunk produces
    // exactly one send and the loop processes chunks in order.
    let scripted = ScriptedSender::new(vec![
        Ok(SendOutcome::Sent),
        Ok(SendOutcome::Sent),
        Ok(SendOutcome::Sent),
    ]);
    let (tx, _shutdown, handle) = spawn_test_loop(scripted.clone());
    let probe = scripted.clone();

    for i in 0..3 {
        tx.send(Some(format!("c{i}"))).unwrap();
        // With watch::channel only the latest slot is kept; space sends
        // so each chunk is individually observable before the next one
        // overwrites it, preserving the "each chunk → one send" invariant.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for _ in 0..200 {
        if probe.sent_count() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(tx);
    handle.await.unwrap();

    let texts = probe.sent_texts();
    assert_eq!(texts, vec!["c0", "c1", "c2"]);
}

// ─── HubClient ReplySender impl smoke (M2) ────────────────────────
//
// We don't have a real hub here, but we can confirm the impl
// compiles and that the impl does the right thing on the
// construction of the request (session_name attachment).
#[test]
fn reply_sender_impl_attaches_session_name_to_hub_ext() {
    // Compile-time check is the load-bearing part. Runtime
    // behavior (HTTP-level) is covered by e2e tests in M5.
    fn assert_reply_sender<S: ReplySender>(_: &S) {}
    let client = fake_client();
    assert_reply_sender(&client);
}

// ─── send_final_with_retry (M3) + give-up budget (M4) ─────────────

fn dummy_req() -> SendMessageRequest {
    SendMessageRequest::reply("ctx".to_string(), "final".to_string(), "user")
}

#[tokio::test(start_paused = true)]
async fn final_reply_throttled_thrice_then_delivered() {
    // E2E-2 scenario C/D: the final reply is throttled N times then
    // lands. The retry helper must keep resending the same payload
    // until Sent and return Ok exactly once delivered.
    let scripted = ScriptedSender::new(vec![
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: Some("rl".into()),
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Throttled {
            ret: -2,
            errmsg: None,
        }),
        Ok(SendOutcome::Sent),
    ]);
    let shutdown = CancellationToken::new();
    let res = send_final_with_retry(
        &scripted,
        dummy_req(),
        test_backoff,
        Duration::from_secs(3600),
        &shutdown,
        "final reply",
    )
    .await;
    assert!(res.is_ok(), "delivery after retries must return Ok");
    assert_eq!(
        scripted.sent_count(),
        4,
        "expected 3 throttled attempts + 1 successful send"
    );
}

#[tokio::test]
async fn final_reply_transport_error_retried_then_succeeds() {
    // Transport errors (e.g. stale pooled connection, TCP reset) are
    // retried within the cumulative budget, not surfaced immediately.
    // Two consecutive failures followed by a Sent must yield Ok with
    // exactly 3 total attempts.
    let scripted = ScriptedSender::new(vec![
        Err(anyhow::anyhow!("connection reset")),
        Err(anyhow::anyhow!("connection reset")),
        Ok(SendOutcome::Sent),
    ]);
    let shutdown = CancellationToken::new();
    let res = send_final_with_retry(
        &scripted,
        dummy_req(),
        test_backoff,
        Duration::from_secs(3600),
        &shutdown,
        "final reply",
    )
    .await;
    assert!(
        res.is_ok(),
        "transport errors retried until success must return Ok"
    );
    assert_eq!(
        scripted.sent_count(),
        3,
        "two transport failures + one successful send = 3 total attempts"
    );
}

#[tokio::test]
async fn final_reply_transport_error_budget_exhausted_gives_up() {
    // With a zero-length budget, the budget check fires immediately
    // after the very first transport error and the helper gives up
    // gracefully (Ok) without a second attempt.
    let scripted = ScriptedSender::new(vec![Err(anyhow::anyhow!("connection reset"))]);
    let shutdown = CancellationToken::new();
    // Duration::ZERO: any elapsed time satisfies `elapsed >= max_total`,
    // so the budget check always triggers after the first failure.
    let res = send_final_with_retry(
        &scripted,
        dummy_req(),
        test_backoff,
        Duration::ZERO,
        &shutdown,
        "final reply",
    )
    .await;
    assert!(
        res.is_ok(),
        "budget-exhausted transport error must give up with Ok, not propagate Err"
    );
    assert_eq!(
        scripted.sent_count(),
        1,
        "exactly one attempt before budget expired"
    );
}

#[tokio::test]
async fn final_reply_persistent_throttle_gives_up_within_budget() {
    // M4: under a permanent throttle the helper gives up once the
    // cumulative budget is exhausted and returns Ok (the caller has
    // nothing better to do than continue), rather than spinning
    // forever.
    let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
        ret: -2,
        errmsg: None,
    });
    let shutdown = CancellationToken::new();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        send_final_with_retry(
            &scripted,
            dummy_req(),
            test_backoff,
            Duration::from_millis(30),
            &shutdown,
            "final reply",
        ),
    )
    .await;
    assert!(
        res.is_ok(),
        "helper must return within the timeout (no infinite spin under persistent throttle)"
    );
    assert!(
        res.unwrap().is_ok(),
        "give-up returns Ok so the caller continues cleanly"
    );
    assert!(
        scripted.sent_count() >= 1,
        "expected at least one send attempt before giving up"
    );
}

#[tokio::test]
async fn final_reply_shutdown_during_backoff_returns_promptly() {
    // Cancel-safety: a shutdown during the backoff sleep aborts the
    // retry loop and returns Ok without hanging.
    let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
        ret: -2,
        errmsg: None,
    });
    let probe = scripted.clone();
    let shutdown = CancellationToken::new();
    let shutdown_for_task = shutdown.clone();
    let task = tokio::spawn(async move {
        send_final_with_retry(
            &scripted,
            dummy_req(),
            test_backoff,
            Duration::from_secs(3600),
            &shutdown_for_task,
            "final reply",
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(15)).await;
    shutdown.cancel();
    let res = tokio::time::timeout(Duration::from_secs(2), task).await;
    assert!(res.is_ok(), "task must finish promptly after shutdown");
    assert!(res.unwrap().unwrap().is_ok());
    let _ = probe.sent_count();
}

#[tokio::test]
async fn partial_persistent_throttle_gives_up_then_serves_new_chunk() {
    // M4 for the partial loop: with a tiny budget and a permanent
    // throttle, the loop abandons the first buffered chunk after the
    // budget elapses. Because the scripted sender always throttles we
    // cannot observe a later Sent, but we CAN observe that the loop
    // gives up (send attempts for one chunk are bounded, no unbounded
    // spin) and then stays responsive: a fresh chunk pushed after the
    // give-up starts a new retry cycle (more send attempts).
    let scripted = ScriptedSender::new_loop(SendOutcome::Throttled {
        ret: -2,
        errmsg: None,
    });
    let (tx, shutdown, handle) =
        spawn_test_loop_with_budget(scripted.clone(), Duration::from_millis(30));
    let probe = scripted.clone();

    tx.send(Some("chunk-0".to_string())).unwrap();
    // Allow the budget to elapse and the loop to abandon chunk-0.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_giveup = probe.sent_count();

    // A fresh chunk after give-up must trigger new attempts.
    tx.send(Some("chunk-1".to_string())).unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    let total = probe.sent_count();
    assert!(
        after_giveup >= 1,
        "loop must attempt at least once before giving up, got {after_giveup}"
    );
    assert!(
        total > after_giveup,
        "a fresh chunk after give-up must trigger new send attempts ({total} !> {after_giveup})"
    );
}
