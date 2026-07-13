//! Unit tests for the inbound dispatch pipeline.
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::super::*;
use super::hub_ext::*;
use super::pipeline::*;
use super::queue::*;
use super::quote::*;
use crate::hub::{AdminConfig, InMemoryQueue};
use crate::ilink::types::{MessageItem, TextItem, WeixinMessage};
use crate::store::Store;

async fn make_state_with_client() -> (Arc<HubState>, String) {
    let (state, vtoken, _mock) =
        make_state_with_client_and_mock(crate::hub::tests::MockUpstream::returning_ok()).await;
    (state, vtoken)
}

async fn make_state_with_client_and_mock(
    upstream: Arc<dyn crate::ilink::UpstreamSink>,
) -> (Arc<HubState>, String, Arc<dyn crate::ilink::UpstreamSink>) {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let mock_ref = Arc::clone(&upstream);
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(
        upstream,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    );
    let (_, vtoken, _) =
        state
            .clients
            .registry
            .write()
            .await
            .register("test-backend".to_string(), None, None);
    (state, vtoken, mock_ref)
}

/// Build a WeixinMessage that carries a quote-reply ref_msg with the given
/// `create_time_ms` and optional quoted text.
fn make_quote_msg(
    from_user_id: &str,
    ref_create_time_ms: i64,
    quoted_text: Option<&str>,
) -> WeixinMessage {
    let mut mi_obj = serde_json::Map::new();
    mi_obj.insert(
        "create_time_ms".to_string(),
        serde_json::Value::Number(ref_create_time_ms.into()),
    );
    if let Some(t) = quoted_text {
        mi_obj.insert("text_item".to_string(), serde_json::json!({"text": t}));
    }

    let extra = serde_json::json!({
        "ref_msg": {
            "message_item": serde_json::Value::Object(mi_obj)
        }
    });

    let item = MessageItem {
        item_type: Some(1),
        text_item: Some(TextItem {
            text: Some("this is the follow-up reply".to_string()),
        }),
        extra,
        ..Default::default()
    };

    WeixinMessage {
        from_user_id: Some(from_user_id.to_string()),
        item_list: Some(Arc::new(vec![item])),
        ..Default::default()
    }
}

/// Build a WeixinMessage that carries a quote-reply ref_msg with the given
/// iLink `msg_id` (wire form: a JSON string, matching real iLink payloads)
/// and an old `create_time_ms` so L1 timestamp lookup cannot match — isolating
/// L0 msg_id routing.
fn make_quote_msg_with_id(from_user_id: &str, ref_msg_id: i64) -> WeixinMessage {
    let extra = serde_json::json!({
        "ref_msg": {
            "message_item": {
                "create_time_ms": 1_000_000i64,
                "msg_id": ref_msg_id.to_string(),
            }
        }
    });
    let item = MessageItem {
        item_type: Some(1),
        text_item: Some(TextItem {
            text: Some("this is the follow-up reply".to_string()),
        }),
        extra,
        ..Default::default()
    };
    WeixinMessage {
        from_user_id: Some(from_user_id.to_string()),
        item_list: Some(Arc::new(vec![item])),
        ..Default::default()
    }
}
/// L0: quote-reply exact routing via `ref_msg.message_item.msg_id`.
///
/// Inserts an assistant message with a stored `ilink_msg_id`, then builds a
/// quote-reply whose ref_msg carries that same id (and an old create_time_ms so
/// L1 cannot match). Verifies `resolve_quote_from_msg_id` returns the exact
/// `(vtoken, session_name)` — no timestamp window involved.
#[tokio::test]
async fn quote_reply_l0_msg_id_exact_routing() {
    let (state, vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user1";
    let session_name = "at-20260713-090000000";
    let ilink_msg_id = 1_783_912_191_000_000_123i64;

    state
        .store
        .save_message_with_msg_id(
            "vctx-test",
            Some(&vtoken),
            session_name,
            peer_user_id,
            "assistant",
            "exact-match reply",
            Some(ilink_msg_id),
        )
        .await
        .expect("save message with msg_id");

    let msg = make_quote_msg_with_id("user1", ilink_msg_id);
    let result = resolve_quote_from_msg_id(&state, &msg).await;

    assert!(result.is_some(), "L0 msg_id lookup must find the exact row");
    match result.unwrap() {
        QuoteOrigin::Client {
            session_name: sn,
            vtoken: vt,
            ..
        } => {
            assert_eq!(sn, Some(session_name.to_string()));
            assert_eq!(vt, vtoken);
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// L0 miss: a ref_msg.msg_id that no assistant row carries must return `None`
/// so the pipeline falls through to L1/L2/L3.
#[tokio::test]
async fn quote_reply_l0_msg_id_miss_returns_none() {
    let (state, _vtoken) = make_state_with_client().await;
    // No row stored for this id.
    let msg = make_quote_msg_with_id("user1", 9_000_000_000_000_000_000);
    let result = resolve_quote_from_msg_id(&state, &msg).await;
    assert!(result.is_none(), "unknown msg_id must not resolve via L0");
}

/// L0 does not cross peers: even if the msg_id matches a row stored under a
/// different peer scope, the lookup must miss (defence-in-depth peer filter).
#[tokio::test]
async fn quote_reply_l0_msg_id_scoped_to_peer() {
    let (state, vtoken) = make_state_with_client().await;
    let ilink_msg_id = 1_783_912_191_000_000_456i64;
    state
        .store
        .save_message_with_msg_id(
            "vctx-other",
            Some(&vtoken),
            "at-20260713-090000000",
            "peer:other-user",
            "assistant",
            "other-peer reply",
            Some(ilink_msg_id),
        )
        .await
        .expect("save message with msg_id");

    // Quote-reply arrives from a different peer but with the same msg_id.
    let msg = make_quote_msg_with_id("user1", ilink_msg_id);
    let result = resolve_quote_from_msg_id(&state, &msg).await;
    assert!(result.is_none(), "L0 must not match across peer scopes");
}

/// F5 / AT1: @mention → quote-reply L1 timestamp routing.
///
/// Inserts an assistant message for peer:user1 with session_name="at-20260704-103000000".
/// Constructs a WeixinMessage whose ref_msg.create_time_ms falls within ±10 s of the
/// inserted row's timestamp. Verifies that `resolve_quote_from_timestamp` returns the
/// correct QuoteOrigin with `session_name = Some("at-20260704-103000000")`.
#[tokio::test]
async fn at_mention_quote_reply_l1_timestamp_routing() {
    let (state, vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user1";
    let session_name = "at-20260704-103000000";

    state
        .store
        .save_message(
            "vctx-test",
            Some(&vtoken),
            session_name,
            peer_user_id,
            "assistant",
            "Hello from @mention session",
        )
        .await
        .expect("save message");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Build a quote-reply message with create_time_ms at the current second —
    // within the ±10 s window used by find_assistant_message_by_timestamp.
    let msg = make_quote_msg("user1", now_ms, None);
    let result = resolve_quote_from_timestamp(&state, &msg).await;

    assert!(
        result.is_some(),
        "L1 timestamp lookup must find the at-mention session"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            session_name: sn,
            vtoken: vt,
            ..
        } => {
            assert_eq!(sn, Some(session_name.to_string()));
            assert_eq!(vt, vtoken);
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// F5 / AT1 (variant): @mention → quote-reply L2 content-prefix routing.
///
/// Inserts an assistant message with a known content prefix. Constructs a WeixinMessage
/// whose ref_msg carries that same text via text_item. Verifies that
/// `resolve_quote_from_db` returns QuoteOrigin::Client with the correct session_name.
#[tokio::test]
async fn at_mention_quote_reply_l2_content_routing() {
    let (state, vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user1";
    let session_name = "at-20260704-103000000";
    let content = "Hello from @mention session — this content prefix will match the quote";

    state
        .store
        .save_message(
            "vctx-test",
            Some(&vtoken),
            session_name,
            peer_user_id,
            "assistant",
            content,
        )
        .await
        .expect("save message");

    // Use a very old timestamp so L1 timestamp lookup won't match; only L2 content
    // lookup should succeed.
    let old_ms = 1_000_000i64;
    let msg = make_quote_msg("user1", old_ms, Some(content));
    let result = resolve_quote_from_db(&state, &msg).await;

    assert!(
        result.is_some(),
        "L2 content lookup must find the at-mention session"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            session_name: sn, ..
        } => assert_eq!(sn, Some(session_name.to_string())),
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// F5 / AT2: @mention → quote-reply L3 footer routing.
///
/// Registers a client named "ilink-claude". Constructs a WeixinMessage whose
/// ref_msg.text_item carries a footer `---\nilink-claude · at-20260704-103000`.
/// Uses a very old timestamp so L1 timestamp and L2 content-prefix DB lookups
/// won't match. Verifies that `resolve_quote_from_footer` returns
/// `QuoteOrigin::Client` with `session_name = Some("at-20260704-103000")` and
/// the correct vtoken for the registered "ilink-claude" client.
#[tokio::test]
async fn at_mention_quote_reply_l3_footer_routing() {
    let (state, _vtoken) = make_state_with_client().await;

    // Register the specific client that the footer references by name.
    let (_, ilink_claude_vtoken, _) =
        state
            .clients
            .registry
            .write()
            .await
            .register("ilink-claude".to_string(), None, None);

    let session_name = "at-20260704-103000";
    // The footer format is: `\n---\n{name} · {session}`.
    // `parse_footer_from_quoted_text` extracts name="ilink-claude" and
    // session=Some("at-20260704-103000") from this footer.
    let quoted_text = format!("Some reply text\n\n---\nilink-claude · {session_name}");

    // Very old timestamp — ensures L1 timestamp lookup won't match any DB row.
    let old_ms = 1_000_000i64;
    let msg = make_quote_msg("user1", old_ms, Some(&quoted_text));
    let result = resolve_quote_from_footer(&state, &msg).await;

    assert!(
        result.is_some(),
        "L3 footer lookup must find the ilink-claude client"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            vtoken: vt,
            session_name: sn,
            ..
        } => {
            assert_eq!(
                vt, ilink_claude_vtoken,
                "vtoken must match the registered ilink-claude client"
            );
            assert_eq!(
                sn,
                Some(session_name.to_string()),
                "session_name must be parsed from the footer"
            );
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// AT3: LIKE injection protection in content-prefix lookup.
///
/// Inserts two rows — one with content containing a literal `%` character, another
/// that would match if `%` were interpreted as a LIKE wildcard. Verifies that
/// `resolve_quote_from_db` returns only the exact-prefix match.
#[tokio::test]
async fn like_injection_protection_in_content_lookup() {
    let (state, vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user1";

    // "100%bonus plan" — contains a literal % that must NOT become a wildcard.
    state
        .store
        .save_message(
            "vctx-test",
            Some(&vtoken),
            "session-percent",
            peer_user_id,
            "assistant",
            "100%bonus plan",
        )
        .await
        .expect("save message: percent row");

    // "100xplan" — would match LIKE '100%' if % is unescaped.
    state
        .store
        .save_message(
            "vctx-test",
            Some(&vtoken),
            "session-x",
            peer_user_id,
            "assistant",
            "100xplan",
        )
        .await
        .expect("save message: x row");

    // Quote the first row.
    let msg = make_quote_msg("user1", 1_000_000, Some("100%bonus plan"));
    let result = resolve_quote_from_db(&state, &msg).await;

    assert!(
        result.is_some(),
        "LIKE lookup must find the percent-content row"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            session_name: sn, ..
        } => assert_eq!(
            sn,
            Some("session-percent".to_string()),
            "must return the percent row, not the wildcard-hit x row"
        ),
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// AT1: Persona-mode footer slow path.
///
/// When the footer is `---\nat-20260704-103000` (name starts with `at-`, no explicit
/// backend name), `resolve_quote_from_footer` must take the slow path: look up the vctx
/// via `find_vctx_for_scope`, then look up the vtoken via `find_vtoken_for_session`.
/// Verifies that a pre-seeded backend_sessions_v2 row is found and the correct
/// QuoteOrigin is returned.
#[tokio::test]
async fn at_mention_quote_reply_l3_footer_persona_routing() {
    let (state, _vtoken) = make_state_with_client().await;

    // Register the named client that owns this persona session.
    let (_, ilink_claude_vtoken, _) =
        state
            .clients
            .registry
            .write()
            .await
            .register("ilink-claude".to_string(), None, None);

    // Create a vctx for "user1" so the scope lookup can find it.
    let vctx = state
        .store
        .find_or_create_vctx("user1", None, "ctx-at1")
        .await
        .expect("find_or_create_vctx");

    // Seed the backend session entry that the slow path will resolve.
    state
        .store
        .set_backend_session(
            &vctx,
            &ilink_claude_vtoken,
            "at-20260704-103000",
            "cli-uuid",
        )
        .await
        .expect("set_backend_session");

    // Footer contains only the session identifier (no explicit backend name).
    // parse_footer_from_quoted_text will return name="at-20260704-103000", session=Some("at-20260704-103000").
    let quoted_text = "body\n\n---\nat-20260704-103000";
    let msg = make_quote_msg("user1", 1_000_000, Some(quoted_text));
    let result = resolve_quote_from_footer(&state, &msg).await;

    assert!(
        result.is_some(),
        "persona-mode footer slow path must resolve the session to a client"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            vtoken: vt,
            session_name: sn,
            ..
        } => {
            assert_eq!(
                vt, ilink_claude_vtoken,
                "vtoken must match the registered ilink-claude client"
            );
            assert_eq!(
                sn,
                Some("at-20260704-103000".to_string()),
                "session_name must be the at-key from the footer"
            );
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// AT2: Unknown backend name in footer returns None.
///
/// When the footer is `---\nghost-client · at-20260704-103000` and "ghost-client" is not
/// in the registry (and no matching session exists in the DB), `resolve_quote_from_footer`
/// must return `None` without panicking.
#[tokio::test]
async fn at_mention_quote_reply_l3_footer_unregistered_name_returns_none() {
    let (state, _vtoken) = make_state_with_client().await;

    // "ghost-client" is deliberately never registered.
    let quoted_text = "body\n\n---\nghost-client · at-20260704-103000";
    let msg = make_quote_msg("user1", 1_000_000, Some(quoted_text));
    let result = resolve_quote_from_footer(&state, &msg).await;

    assert!(
        result.is_none(),
        "unregistered backend name must yield None, not panic"
    );
}

/// AT3: Full L1→L2→L3 cascade fallback.
///
/// With an empty message history (L1 timestamp miss, L2 content miss), a quote-reply
/// whose footer names a registered backend must fall through all three layers and be
/// resolved by L3. Exercises the cascade block at dispatch.rs lines 93-103.
#[tokio::test]
async fn quote_reply_l3_full_cascade_fallback() {
    let (state, _vtoken) = make_state_with_client().await;

    // Register the specific client the footer references.
    let (_, ilink_claude_vtoken, _) =
        state
            .clients
            .registry
            .write()
            .await
            .register("ilink-claude".to_string(), None, None);

    // DB is empty — L1 and L2 will both miss.
    let old_ms = 1_000_000i64;
    let quoted_text = "body\n\n---\nilink-claude · at-20260704-103000";
    let msg = make_quote_msg("user1", old_ms, Some(quoted_text));

    // L1: timestamp lookup — must return None (no messages in DB).
    let from_ts = resolve_quote_from_timestamp(&state, &msg).await;
    assert!(from_ts.is_none(), "L1 must miss on empty DB");

    // L2: content-prefix DB lookup — must return None (no messages in DB).
    let from_db = resolve_quote_from_db(&state, &msg).await;
    assert!(from_db.is_none(), "L2 must miss on empty DB");

    // L3: footer lookup — must succeed because "ilink-claude" is registered.
    let from_footer = resolve_quote_from_footer(&state, &msg).await;
    assert!(
        from_footer.is_some(),
        "L3 footer lookup must succeed after L1 and L2 both miss"
    );
    match from_footer.unwrap() {
        QuoteOrigin::Client {
            vtoken: vt,
            name: n,
            session_name: sn,
            ..
        } => {
            assert_eq!(vt, ilink_claude_vtoken, "vtoken must match ilink-claude");
            assert_eq!(n, "ilink-claude", "name must be ilink-claude");
            assert_eq!(
                sn,
                Some("at-20260704-103000".to_string()),
                "session_name must be parsed from footer"
            );
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// AT4: Three-part footer with label — `ilink-claude · office · at-20260704-103000`.
///
/// Verifies that `resolve_quote_from_footer` correctly extracts `name = "ilink-claude"`
/// and `session_name = Some("at-20260704-103000")` when a label segment is present in
/// the middle, and that the label does not interfere with the registry lookup.
#[tokio::test]
async fn at_mention_quote_reply_l3_footer_with_label_routing() {
    let (state, _vtoken) = make_state_with_client().await;

    // Register the client whose name appears in the first footer segment.
    let (_, ilink_claude_vtoken, _) =
        state
            .clients
            .registry
            .write()
            .await
            .register("ilink-claude".to_string(), None, None);

    // Three-part footer: name · label · session.
    let quoted_text = "body\n\n---\nilink-claude · office · at-20260704-103000";
    let msg = make_quote_msg("user1", 1_000_000, Some(quoted_text));
    let result = resolve_quote_from_footer(&state, &msg).await;

    assert!(
        result.is_some(),
        "three-part footer must resolve the named client"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            vtoken: vt,
            name: n,
            session_name: sn,
            ..
        } => {
            assert_eq!(
                vt, ilink_claude_vtoken,
                "vtoken must match ilink-claude (first footer segment)"
            );
            assert_eq!(n, "ilink-claude", "name must be the first footer segment");
            assert_eq!(
                sn,
                Some("at-20260704-103000".to_string()),
                "session_name must be the last footer segment"
            );
        }
        other => panic!("expected QuoteOrigin::Client, got {other:?}"),
    }
}

/// M3-1: build_no_backend_reply for non-command text must return NO_BACKEND_ONLINE.
/// Catches the `→ String::new()` and `→ "xyzzy"` mutants (781:5).
#[test]
fn build_no_backend_reply_non_command_returns_no_backend_online() {
    let result = build_no_backend_reply(Some("hello, tell me about Rust"));
    assert_eq!(
        result,
        messages::NO_BACKEND_ONLINE,
        "non-command text must produce NO_BACKEND_ONLINE"
    );
}

/// M3-2: build_no_backend_reply for None input returns NO_BACKEND_ONLINE.
#[test]
fn build_no_backend_reply_none_returns_no_backend_online() {
    let result = build_no_backend_reply(None);
    assert_eq!(result, messages::NO_BACKEND_ONLINE);
}

/// M3-3: build_no_backend_reply for a command (/) returns UNRECOGNIZED_COMMAND.
#[test]
fn build_no_backend_reply_command_returns_unrecognized_command() {
    let result = build_no_backend_reply(Some("/unknown_cmd"));
    assert_eq!(
        result,
        messages::UNRECOGNIZED_COMMAND,
        "slash command without a backend must return UNRECOGNIZED_COMMAND"
    );
}

/// M3-4: push_to_queue_pub must increment messages_dispatched for a fresh queue.
/// Catches the `push_to_queue_pub → ()` mutant (623:5) — if the function becomes
/// a no-op, the metrics counter stays at 0 instead of incrementing.
#[tokio::test]
async fn push_to_queue_pub_increments_dispatched_metric() {
    let queue: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::new());
    let metrics = Metrics::default();
    let msg = WeixinMessage::default();
    let before = metrics.messages_dispatched.load(Ordering::Relaxed);

    push_to_queue_pub(&queue, &metrics, "vhub_test", msg).await;

    assert_eq!(
        metrics.messages_dispatched.load(Ordering::Relaxed),
        before + 1,
        "push_to_queue_pub must increment messages_dispatched by 1"
    );
}

/// M3-5: resolve_quote_from_footer with "session-" prefix must resolve via the
/// session-name slow path. Catches the `||` → `&&` mutant (482:50) which would
/// require BOTH "at-" AND "session-" prefixes to match, breaking "session-" alone.
///
/// Footer is two-part (`---\nsession-alpha · at-decoy`): name="session-alpha",
/// session=Some("at-decoy"). With `||`, session_key uses the name and the DB
/// hit succeeds. With `&&`, the if-arm is false and session_key becomes
/// "at-decoy" (no DB row) → None.
#[tokio::test]
async fn resolve_quote_from_footer_session_prefix_uses_session_path() {
    let (state, vtoken) = make_state_with_client().await;
    let session_key = "session-alpha";

    // Seed scope → vctx → (vctx, session) → vtoken. Do NOT register a client
    // named "session-alpha" so the fast name lookup misses and we take the
    // session-prefix slow path.
    let vctx = state
        .store
        .find_or_create_vctx("user1", None, "real-ctx-session-prefix")
        .await
        .expect("vctx");
    state
        .store
        .set_backend_session(&vctx, &vtoken, session_key, "cli-sid")
        .await
        .expect("backend session");

    // Two-part footer: name starts with session-, but parsed session_name is
    // a different at-* key. This is the only shape that distinguishes || from &&.
    let quoted_text = "body\n\n---\nsession-alpha · at-decoy";
    let msg = make_quote_msg("user1", 1_000_000, Some(quoted_text));
    let result = resolve_quote_from_footer(&state, &msg).await;
    assert!(
        result.is_some(),
        "session- prefix alone must resolve via slow path (|| not &&)"
    );
    match result.unwrap() {
        QuoteOrigin::Client {
            vtoken: vt,
            session_name,
            ..
        } => {
            assert_eq!(vt, vtoken);
            assert_eq!(session_name.as_deref(), Some(session_key));
        }
        other => panic!("expected Client origin, got {other:?}"),
    }
}

/// M3-6: build_hub_ext_for_vctx must return a non-empty session_id when the
/// session override is set and the DB has a non-empty session value.
/// Catches the `delete !` mutant (728:18) which reverses the `is_empty` guard and
/// would return None for non-empty session values.
#[tokio::test]
async fn build_hub_ext_for_vctx_session_override_returns_non_empty_session_id() {
    let (state, vtoken) = make_state_with_client().await;
    let vctx = "vctx-test-hub-ext";
    let session_name = "my-session";
    let session_value = "claude-session-abc123";

    state
        .store
        .set_backend_session(vctx, &vtoken, session_name, session_value)
        .await
        .expect("set_backend_session must succeed");

    let hub_ext =
        build_hub_ext_for_vctx(&state.store, vctx, &vtoken, Some(session_name.to_string())).await;

    assert!(
        hub_ext.is_some(),
        "hub_ext must be Some when session override is provided"
    );
    let ext = hub_ext.unwrap();
    assert_eq!(
        ext.session_id.as_deref(),
        Some(session_value),
        "session_id must equal the stored session value (not-empty guard must work)"
    );
}

/// Empty context_token on ForwardTo must skip upstream send and queue push.
/// Catches `!ctx.is_empty()` match guard → true at the ForwardTo arm.
#[tokio::test]
async fn dispatch_message_empty_context_skips_forward() {
    let mock = crate::hub::tests::MockUpstream::returning_ok();
    let (state, vtoken, mock_ref) = make_state_with_client_and_mock(mock).await;
    {
        let mut router = state.routing.router.lock().await;
        router.set_route("user-empty-ctx", vtoken);
    }
    let before_dropped = state.metrics.messages_dropped.load(Ordering::Relaxed);
    let before_dispatched = state.metrics.messages_dispatched.load(Ordering::Relaxed);

    let msg = WeixinMessage {
        context_token: Some(String::new()),
        from_user_id: Some("user-empty-ctx".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    dispatch_message(Arc::clone(&state), msg).await;

    assert_eq!(
        mock_ref.polls_ok(),
        0,
        "empty context must not call upstream send_message"
    );
    assert_eq!(
        state.metrics.messages_dispatched.load(Ordering::Relaxed),
        before_dispatched,
        "empty context must not push to queue"
    );
    let _ = before_dropped;
}

/// Broadcast with no online clients and empty context must not call upstream.
/// Catches the Broadcast-arm `!c.is_empty()` filter mutant.
#[tokio::test]
async fn dispatch_broadcast_empty_context_skips_no_backend_reply() {
    let mock = crate::hub::tests::MockUpstream::returning_ok();
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let mock_ref = Arc::clone(&mock);
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(
        mock,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    );
    // No clients registered → no-backend path.
    let msg = WeixinMessage {
        context_token: Some(String::new()),
        from_user_id: Some("user-bcast".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello world".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    // Call handle_broadcast directly: Router::route never returns Broadcast today.
    handle_broadcast(Arc::clone(&state), msg).await;
    assert_eq!(
        mock_ref.polls_ok(),
        0,
        "empty context on no-backend path must not send reply"
    );
}

/// No-backend path with a non-empty context must call upstream once.
/// Complements the empty-context skip test and pins the filter guard.
#[tokio::test]
async fn dispatch_broadcast_no_backend_sends_reply_with_context() {
    let mock = crate::hub::tests::MockUpstream::returning_ok();
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let mock_ref = Arc::clone(&mock);
    let queue = Arc::new(InMemoryQueue::new());
    let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = HubState::new(
        mock,
        Arc::new(store),
        queue,
        shutdown_rx,
        "test-relay-secret".to_string(),
        AdminConfig::from_env(),
    );
    let msg = WeixinMessage {
        context_token: Some("real-ctx-no-backend".into()),
        from_user_id: Some("user-nobackend".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello world".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    handle_broadcast(Arc::clone(&state), msg).await;
    assert_eq!(
        mock_ref.polls_ok(),
        1,
        "no-backend path with context must send fallback reply"
    );
}

/// Broadcast with online clients + empty context must skip queue push.
/// Catches match-guard mutants at the Broadcast arm (`!ctx.is_empty()`).
#[tokio::test]
async fn dispatch_broadcast_online_empty_context_skips_queue() {
    let (state, vtoken) = make_state_with_client().await;
    state.clients.registry.write().await.mark_online(&vtoken);
    let before = state.metrics.messages_dispatched.load(Ordering::Relaxed);

    let msg = WeixinMessage {
        context_token: Some(String::new()),
        from_user_id: Some("user-bcast-online".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello broadcast".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    handle_broadcast(Arc::clone(&state), msg).await;
    assert_eq!(
        state.metrics.messages_dispatched.load(Ordering::Relaxed),
        before,
        "empty context must not push_shared to online clients"
    );
    let drained = state.clients.queue.drain(&vtoken).await.expect("drain");
    assert!(
        drained.is_empty(),
        "queue must stay empty when broadcast context is empty"
    );
}

/// Broadcast with online clients + real context must push via push_shared_to_queue.
/// Catches `push_shared_to_queue → ()` no-op mutant.
#[tokio::test]
async fn dispatch_broadcast_online_pushes_shared_to_queue() {
    let (state, vtoken) = make_state_with_client().await;
    state.clients.registry.write().await.mark_online(&vtoken);
    let before = state.metrics.messages_dispatched.load(Ordering::Relaxed);

    let msg = WeixinMessage {
        context_token: Some("real-ctx-bcast".into()),
        from_user_id: Some("user-bcast-push".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("hello broadcast".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    handle_broadcast(Arc::clone(&state), msg).await;
    assert_eq!(
        state.metrics.messages_dispatched.load(Ordering::Relaxed),
        before + 1,
        "broadcast must call push_shared_to_queue for each online client"
    );
    let drained = state.clients.queue.drain(&vtoken).await.expect("drain");
    assert_eq!(
        drained.len(),
        1,
        "exactly one shared message must be queued"
    );
    assert!(
        drained[0]
            .context_token
            .as_deref()
            .is_some_and(|c| !c.is_empty()),
        "queued message must carry a non-empty vctx"
    );
}

/// @mention with empty context_token must not push to the queue.
/// Catches `!ctx.is_empty()` match guard → true in handle_at_mention.
#[tokio::test]
async fn dispatch_at_mention_empty_context_skips_queue() {
    let (state, vtoken) = make_state_with_client().await;
    state.clients.registry.write().await.mark_online(&vtoken);
    let before = state.metrics.messages_dispatched.load(Ordering::Relaxed);

    let msg = WeixinMessage {
        context_token: Some(String::new()),
        from_user_id: Some("user-at".into()),
        item_list: Some(Arc::new(vec![MessageItem {
            item_type: Some(1),
            text_item: Some(TextItem {
                text: Some("@test-backend please help".into()),
            }),
            ..Default::default()
        }])),
        ..Default::default()
    };
    dispatch_message(Arc::clone(&state), msg).await;
    assert_eq!(
        state.metrics.messages_dispatched.load(Ordering::Relaxed),
        before,
        "@mention with empty context must not push to queue"
    );
    let drained = state.clients.queue.drain(&vtoken).await.expect("drain");
    assert!(
        drained.is_empty(),
        "queue must stay empty for empty-ctx @mention"
    );
}

/// Timestamp quote lookup must ignore rows whose vtoken is empty.
/// Catches `!vtoken.is_empty()` match guard → true in resolve_quote_from_timestamp.
#[tokio::test]
async fn resolve_quote_from_timestamp_rejects_empty_vtoken() {
    let (state, _vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user-empty-vt";
    state
        .store
        .save_message(
            "vctx-empty-vt",
            Some(""),
            "at-empty-vtoken",
            peer_user_id,
            "assistant",
            "reply with empty vtoken",
        )
        .await
        .expect("save");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let msg = make_quote_msg("user-empty-vt", now_ms, None);
    let result = resolve_quote_from_timestamp(&state, &msg).await;
    assert!(
        result.is_none(),
        "empty vtoken row must not become QuoteOrigin::Client"
    );
}

/// DB content quote lookup must ignore rows whose vtoken is empty.
/// Catches `!vtoken.is_empty()` match guard → true in resolve_quote_from_db.
#[tokio::test]
async fn resolve_quote_from_db_rejects_empty_vtoken() {
    let (state, _vtoken) = make_state_with_client().await;
    let peer_user_id = "peer:user-empty-vt-db";
    let body = "unique-empty-vtoken-prefix-content-xyz";
    state
        .store
        .save_message(
            "vctx-empty-vt-db",
            Some(""),
            "at-empty-vtoken-db",
            peer_user_id,
            "assistant",
            body,
        )
        .await
        .expect("save");

    let msg = make_quote_msg("user-empty-vt-db", 1_000_000, Some(body));
    let result = resolve_quote_from_db(&state, &msg).await;
    assert!(
        result.is_none(),
        "empty vtoken row must not become QuoteOrigin::Client via DB lookup"
    );
}
