//! Issue #30 — driver portability of the session lookup and the default
//! (opt-in, dry-run) retention behaviour, exercised through the public store
//! API only.
//!
//! The in-crate tests cover the per-driver SQL text; these two cases pin the
//! observable behaviour a caller depends on.

use ilink_hub::store::{RetentionConfig, RetentionReport, Store};

#[tokio::test]
async fn find_vtoken_for_session_returns_newest_row_for_same_session() {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");

    let peer_user_id = "issue30-portability@im.wechat";
    let scope = "peer:issue30-portability@im.wechat";
    let session_name = "at-20261004-120000000";

    let vctx = store
        .find_or_create_vctx(peer_user_id, None, "ctx-issue30-portability")
        .await
        .expect("find_or_create_vctx");
    store
        .set_backend_session(&vctx, "vtoken-issue30-old", session_name, "cli-old")
        .await
        .expect("insert older row");
    store
        .set_backend_session(&vctx, "vtoken-issue30-new", session_name, "cli-new")
        .await
        .expect("insert newer row");

    // This is the persona-footer slow path: scope → vctx → vtoken.
    let found_vctx = store
        .find_vctx_for_scope(scope)
        .await
        .expect("find_vctx_for_scope")
        .expect("scope must resolve");
    assert_eq!(found_vctx, vctx);

    let vtoken = store
        .find_vtoken_for_session(&found_vctx, session_name)
        .await
        .expect("find_vtoken_for_session")
        .expect("session must resolve to a vtoken");
    assert_eq!(
        vtoken, "vtoken-issue30-new",
        "the newest row for (vctx, session_name) must win"
    );
}

/// The default retention config must never delete anything: it is disabled, and
/// even a forced sweep has no TTL configured.
#[tokio::test]
async fn retention_default_config_deletes_nothing() {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("in-memory store");
    let peer_user_id = "peer:issue30-retention@im.wechat";

    let vctx = store
        .find_or_create_vctx("issue30-retention@im.wechat", None, "ctx-issue30-retention")
        .await
        .expect("find_or_create_vctx");
    for i in 0..3 {
        store
            .save_message(
                &vctx,
                Some("vtoken-issue30"),
                "default",
                peer_user_id,
                "user",
                &format!("retained body {i}"),
            )
            .await
            .expect("save_message");
    }

    let cfg = RetentionConfig::default();
    assert!(!cfg.enabled, "retention must be opt-in");
    assert!(cfg.dry_run, "retention must default to dry-run");

    let now_epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let report = store
        .sweep_retention(&cfg, now_epoch_secs)
        .await
        .expect("sweep_retention");
    assert_eq!(
        report,
        RetentionReport {
            dry_run: true,
            ..RetentionReport::default()
        }
    );

    let rows = store
        .list_messages(&vctx, 100)
        .await
        .expect("list_messages");
    assert_eq!(rows.len(), 3, "default retention must keep every row");
}
