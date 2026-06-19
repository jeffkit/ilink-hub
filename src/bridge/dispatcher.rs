use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::executor::{extract_media_env, run_cli, truncate_chars};
use super::AUTH_ERROR_KEYWORDS;
use crate::bridge::config::BridgeApp;
use crate::bridge::connection::hub_response_token_rejected;
use crate::ilink::types::{
    BaseInfo, GetUpdatesRequest, GetUpdatesResponse, HubExt, SendMessageRequest,
    SendMessageResponse, WeixinMessage,
};

/// Returned from [`run_bridge`] when Hub terminates the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStop {
    /// Hub rejected the virtual token (401 / revoked).
    TokenRejected,
    /// CLI reported a fatal auth/credential error; user action required.
    FatalCliError(String),
}

enum GetUpdatesOutcome {
    Ok(GetUpdatesResponse),
    TokenRejected,
}

#[derive(Clone)]
pub(super) struct HubClient {
    http: reqwest::Client,
    hub_url: String,
    token: String,
}

impl HubClient {
    pub(super) fn new(hub_url: String, token: String) -> Self {
        let hub_url = hub_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("reqwest client");
        Self {
            http,
            hub_url,
            token,
        }
    }

    async fn getupdates(&self, buf: &mut String) -> Result<GetUpdatesOutcome> {
        let body = GetUpdatesRequest {
            get_updates_buf: buf.clone(),
            base_info: Some(BaseInfo::default()),
            timeout: None,
        };
        let url = format!("{}/ilink/bot/getupdates", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let out: GetUpdatesResponse = resp.json().await?;
        if hub_response_token_rejected(status, out.ret) {
            warn!(
                status = %status,
                errmsg = ?out.errmsg,
                "hub rejected virtual token during getupdates"
            );
            return Ok(GetUpdatesOutcome::TokenRejected);
        }
        if !status.is_success() {
            anyhow::bail!("getupdates HTTP {status}: {:?}", out.errmsg);
        }
        if let Some(ref newbuf) = out.get_updates_buf {
            *buf = newbuf.clone();
        }
        Ok(GetUpdatesOutcome::Ok(out))
    }

    async fn sendmessage(&self, req: SendMessageRequest) -> Result<()> {
        let url = format!("{}/ilink/bot/sendmessage", self.hub_url);
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token.trim()))
            .json(&req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendmessage HTTP {status}: {t}");
        }
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<SendMessageResponse>(&text) {
            Ok(v) => {
                if v.ret.map(|r| r != 0).unwrap_or(false) {
                    anyhow::bail!("sendmessage ret={:?} errmsg={:?}", v.ret, v.errmsg);
                }
                Ok(())
            }
            Err(_) => Ok(()),
        }
    }
}

fn session_dispatch_key(msg: &WeixinMessage) -> String {
    let ctx = msg.context_token.as_deref().unwrap_or("");
    let session_name = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default");
    format!("{ctx}:{session_name}")
}

async fn run_session_worker(
    key: String,
    mut rx: mpsc::Receiver<WeixinMessage>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
) {
    const SESSION_WORKER_MAX_BACKOFF_SECS: u64 = 60;
    let mut consecutive_failures: u32 = 0;

    while let Some(msg) = rx.recv().await {
        match handle_one_message(&client, &app, msg).await {
            Ok(()) => {
                consecutive_failures = 0;
            }
            Err(HandleError::Fatal(reason)) => {
                error!(session_key = %key, reason = ?reason, "fatal CLI error; signalling bridge stop");
                let _ = stop_tx.send(Some(reason));
                return;
            }
            Err(HandleError::Transient(e)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff_secs =
                    SESSION_WORKER_MAX_BACKOFF_SECS.min(1_u64 << consecutive_failures.min(63));
                error!(
                    session_key = %key,
                    error = %e,
                    consecutive_failures,
                    backoff_secs,
                    "message handler failed; backing off before next message"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
        }
    }
    info!(session_key = %key, "session worker exiting");
}

enum HandleError {
    Transient(anyhow::Error),
    Fatal(BridgeStop),
}

impl From<anyhow::Error> for HandleError {
    fn from(e: anyhow::Error) -> Self {
        HandleError::Transient(e)
    }
}

const DEFAULT_SESSION_QUEUE_SIZE: usize = 200;

struct SessionDispatcher {
    senders: tokio::sync::Mutex<HashMap<String, mpsc::Sender<WeixinMessage>>>,
    client: HubClient,
    app: Arc<BridgeApp>,
    stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
}

impl SessionDispatcher {
    fn new(
        client: HubClient,
        app: Arc<BridgeApp>,
        stop_tx: tokio::sync::watch::Sender<Option<BridgeStop>>,
    ) -> Self {
        Self {
            senders: tokio::sync::Mutex::new(HashMap::new()),
            client,
            app,
            stop_tx,
        }
    }

    async fn dispatch(&self, msg: WeixinMessage) {
        let key = session_dispatch_key(&msg);
        let mut senders = self.senders.lock().await;

        senders.retain(|_, tx| !tx.is_closed());

        let needs_new = match senders.get(&key) {
            Some(tx) => tx.is_closed(),
            None => true,
        };

        if needs_new {
            let (tx, rx) = mpsc::channel(DEFAULT_SESSION_QUEUE_SIZE);
            senders.insert(key.clone(), tx.clone());
            let client = self.client.clone();
            let app = Arc::clone(&self.app);
            let stop_tx = self.stop_tx.clone();
            tokio::spawn(run_session_worker(key.clone(), rx, client, app, stop_tx));
        }

        if let Some(tx) = senders.get(&key) {
            match tx.try_send(msg) {
                Ok(_) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(session_key = %key, "session queue full, dropping message");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    #[cfg(test)]
    async fn sender_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.senders.lock().await.keys().cloned().collect();
        keys.sort();
        keys
    }
}

/// Long-poll Hub and dispatch inbound user text to the configured CLI.
///
/// Returns when Hub signals a stop condition (token rejected or fatal CLI auth error).
pub async fn run_bridge(hub_url: String, token: String, app: BridgeApp) -> BridgeStop {
    let client = HubClient::new(hub_url, token);
    let app = Arc::new(app);
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(None::<BridgeStop>);
    let dispatcher = SessionDispatcher::new(client.clone(), Arc::clone(&app), stop_tx);
    let mut buf = String::new();
    let mut backoff_secs: u64 = 3;
    const MAX_BACKOFF_SECS: u64 = 60;

    info!(
        routing = %app.routing_label(),
        profiles = ?app.profile_names(),
        "ilink-hub-bridge connected; waiting for getupdates"
    );

    loop {
        // Check if any session worker signalled a fatal stop.
        if stop_rx.has_changed().unwrap_or(false) {
            if let Some(reason) = stop_rx.borrow_and_update().clone() {
                return reason;
            }
        }

        let resp = match client.getupdates(&mut buf).await {
            Ok(GetUpdatesOutcome::Ok(r)) => {
                backoff_secs = 3;
                r
            }
            Ok(GetUpdatesOutcome::TokenRejected) => return BridgeStop::TokenRejected,
            Err(e) => {
                error!(error = %e, backoff_secs, "getupdates failed; retrying with backoff");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                continue;
            }
        };

        if resp.ret != Some(0) {
            warn!(
                ret = ?resp.ret,
                errcode = ?resp.errcode,
                errmsg = ?resp.errmsg,
                "getupdates returned non-zero ret"
            );
        }

        for msg in resp.msgs.unwrap_or_default() {
            dispatcher.dispatch(msg).await;
        }
    }
}

fn dump_inbound_weixin_message_for_debug(msg: &WeixinMessage) {
    let Ok(flag) = std::env::var("ILINKHUB_BRIDGE_DUMP_MSG") else {
        return;
    };
    let f = flag.trim().to_ascii_lowercase();
    if !matches!(f.as_str(), "1" | "true" | "yes") {
        return;
    }

    let full = serde_json::to_string_pretty(msg)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialize WeixinMessage: {e}\"}}"));
    eprintln!("========== ILINKHUB_BRIDGE_DUMP_MSG: full WeixinMessage (JSON) ==========");
    eprintln!("{full}");
    eprintln!("========== end full message ==========");

    if let Some(items) = msg.item_list.as_ref() {
        for (i, item) in items.iter().enumerate() {
            let extra = serde_json::to_string_pretty(&item.extra)
                .unwrap_or_else(|_| "\"<extra serialize error>\"".to_string());
            eprintln!("---------- item_list[{i}] ----------");
            eprintln!("  type (item_type): {:?}", item.item_type);
            eprintln!("  text_item: {:?}", item.text_item);
            eprintln!("  extra (flattened fields from iLink, not in text_item):");
            eprintln!("{extra}");
        }
        eprintln!("========== end item_list dump ==========");
    } else {
        eprintln!("========== item_list: <none> ==========");
    }
}

#[tracing::instrument(
    skip_all,
    fields(
        from    = msg.from_user_id.as_deref().unwrap_or("?"),
        ctx     = msg.context_token.as_deref().unwrap_or("(none)"),
        profile = tracing::field::Empty,
    )
)]
async fn handle_one_message(
    client: &HubClient,
    app: &BridgeApp,
    msg: WeixinMessage,
) -> Result<(), HandleError> {
    dump_inbound_weixin_message_for_debug(&msg);

    if app.skip_bot_messages && msg.message_type == Some(2) {
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t.to_string(),
        None if !app.require_text => String::new(),
        None => return Ok(()),
    };
    if text.trim().is_empty() && app.require_text {
        return Ok(());
    }

    let media_env = extract_media_env(&msg);

    let (profile_name, profile, payload) = app
        .resolve(&text)
        .with_context(|| format!("route message for profile (text prefix): {text:?}"))?;

    let ctx = msg
        .context_token
        .clone()
        .filter(|s| !s.is_empty())
        .context("inbound message missing context_token")?;
    let from_user = msg.from_user_id.clone().unwrap_or_default();
    let session_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_id.as_deref())
        .unwrap_or("")
        .to_string();
    let session_name_for_cli = msg
        .ilink_hub_ext
        .as_ref()
        .and_then(|e| e.session_name.as_deref())
        .unwrap_or("default")
        .to_string();

    tracing::Span::current().record("profile", profile_name);
    info!(%profile_name, %profile.command, session_name = %session_name_for_cli, "running bridge profile");

    let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<String>();

    let fwd_client = client.clone();
    let fwd_ctx = ctx.clone();
    let fwd_from_user = from_user.clone();
    let fwd_session_name = session_name_for_cli.clone();
    let forward_handle = tokio::spawn(async move {
        while let Some(chunk) = partial_rx.recv().await {
            let mut req =
                SendMessageRequest::reply_text(fwd_ctx.clone(), chunk, &fwd_from_user, None);
            if let Some(ref mut msg) = req.msg {
                let ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                ext.session_name = Some(fwd_session_name.clone());
            }
            if let Err(e) = fwd_client.sendmessage(req).await {
                warn!(error = %e, "failed to send partial reply");
            }
        }
    });

    let cli_result = run_cli(
        profile,
        profile_name,
        &payload,
        &session_for_cli,
        &session_name_for_cli,
        &from_user,
        &ctx,
        &media_env,
        partial_tx,
    )
    .await;

    let _ = forward_handle.await;

    match cli_result {
        Ok((raw_body, cli_session)) => {
            let body = truncate_chars(
                &raw_body,
                profile.max_reply_chars,
                &profile.truncation_suffix,
            );
            if body.trim().is_empty() {
                if let Some(sid) = cli_session {
                    if !sid.trim().is_empty() {
                        let mut req = SendMessageRequest::reply_text(
                            ctx,
                            String::new(),
                            &from_user,
                            Some(sid),
                        );
                        if let Some(ref mut msg) = req.msg {
                            let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                            hub_ext.session_name = Some(session_name_for_cli.clone());
                        }
                        if let Err(e) = client.sendmessage(req).await {
                            warn!(error = %e, "failed to persist cli_session_id after ILINK_PARTIAL-only reply");
                        }
                    }
                }
                return Ok(());
            }
            let mut req = SendMessageRequest::reply_text(ctx, body, &from_user, cli_session);
            if let Some(ref mut msg) = req.msg {
                let hub_ext = msg.ilink_hub_ext.get_or_insert_with(HubExt::default);
                hub_ext.session_name = Some(session_name_for_cli.clone());
            }
            client
                .sendmessage(req)
                .await
                .context("sendmessage reply")?;
        }
        Err(e) => {
            if app.send_error_reply {
                let err_text = format!("（本地 CLI 失败）\n{e:#}");
                let req = SendMessageRequest::reply(ctx, err_text, &from_user);
                if let Err(send_e) = client.sendmessage(req).await {
                    warn!(error = %send_e, "failed to send error reply");
                }
            }
            let err_str = e.to_string().to_lowercase();
            if AUTH_ERROR_KEYWORDS.iter().any(|&k| err_str.contains(k))
                || err_str.contains("not found")
                || err_str.contains("no such file")
            {
                return Err(HandleError::Fatal(BridgeStop::FatalCliError(
                    e.to_string(),
                )));
            }
            return Err(HandleError::Transient(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ilink::types::{MessageItem, TextItem};

    fn make_msg(ctx: &str, session_name: &str) -> WeixinMessage {
        WeixinMessage {
            context_token: Some(ctx.into()),
            ilink_hub_ext: Some(HubExt {
                session_id: Some(String::new()),
                session_name: Some(session_name.into()),
                cli_session_id: None,
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
        HubClient::new("http://127.0.0.1:1".into(), "test-token".into())
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
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        let msg = make_msg("ctx-a", "default");
        disp.dispatch(msg.clone()).await;
        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys().await, vec!["ctx-a:default"]);
    }

    #[tokio::test]
    async fn different_ctx_tokens_get_separate_senders() {
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        disp.dispatch(make_msg("ctx-a", "default")).await;
        disp.dispatch(make_msg("ctx-b", "default")).await;
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-a:default", "ctx-b:default"]
        );
    }

    #[tokio::test]
    async fn different_session_names_get_separate_senders() {
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        disp.dispatch(make_msg("ctx-a", "feature-x")).await;
        disp.dispatch(make_msg("ctx-a", "feature-y")).await;
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-a:feature-x", "ctx-a:feature-y"]
        );
    }

    #[tokio::test]
    async fn three_distinct_sessions_create_three_senders() {
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        disp.dispatch(make_msg("ctx-1", "default")).await;
        disp.dispatch(make_msg("ctx-2", "default")).await;
        disp.dispatch(make_msg("ctx-1", "feature-a")).await;
        assert_eq!(
            disp.sender_keys().await,
            vec!["ctx-1:default", "ctx-1:feature-a", "ctx-2:default"]
        );
    }

    #[tokio::test]
    async fn repeated_same_key_does_not_grow_sender_map() {
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        let msg = make_msg("ctx-x", "s1");
        for _ in 0..5 {
            disp.dispatch(msg.clone()).await;
        }
        assert_eq!(disp.sender_keys().await.len(), 1);
    }

    #[tokio::test]
    async fn dead_sender_triggers_new_worker_on_next_dispatch() {
        let disp =
            SessionDispatcher::new(fake_client(), Arc::new(make_fast_app()), make_stop_tx());
        let msg = make_msg("ctx-z", "default");

        disp.dispatch(msg.clone()).await;

        {
            let mut senders = disp.senders.lock().await;
            senders.remove("ctx-z:default");
        }
        assert_eq!(disp.sender_keys().await.len(), 0);

        disp.dispatch(msg.clone()).await;
        assert_eq!(disp.sender_keys().await, vec!["ctx-z:default"]);
    }
}
