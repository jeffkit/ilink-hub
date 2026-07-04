//! Axum router for the MCP Streamable HTTP endpoint.
//!
//! All MCP traffic flows through `POST /mcp`.  The request body is a single
//! JSON-RPC 2.0 object; the response is `application/json` with a single
//! JSON-RPC 2.0 object.  (We do not implement SSE streaming or batching —
//! the two tools are request/response and synchronous enough that plain HTTP
//! JSON suffices.)
//!
//! Authentication: the calling Agent supplies its vtoken as `Bearer <token>`
//! in the `Authorization` header, identical to the existing `/ilink/bot/*` API.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::hub::HubState;
use crate::server::routes::{extract_vtoken_pub, UNKNOWN_VTOKEN_MSG};

use super::protocol::{self, JsonRpcRequest, JsonRpcResponse};
use super::tools::{call_agent, list_agents, CallAgentContext, CallAgentParams, MAX_A2A_DEPTH};

pub fn mcp_router() -> Router<Arc<HubState>> {
    Router::new().route("/mcp", post(handle_mcp))
}

async fn handle_mcp(
    State(state): State<Arc<HubState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    if req.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            Json(JsonRpcResponse::err(
                req.id,
                -32600,
                "Invalid Request: jsonrpc must be \"2.0\"",
            )),
        );
    }

    debug!(method = %req.method, "MCP request");

    // `initialize` and `notifications/initialized` do not require auth.
    match req.method.as_str() {
        "initialize" => {
            return (
                StatusCode::OK,
                Json(JsonRpcResponse::ok(req.id, protocol::server_info())),
            );
        }
        "notifications/initialized" => {
            return (
                StatusCode::OK,
                Json(JsonRpcResponse::ok(req.id, serde_json::json!({}))),
            );
        }
        _ => {}
    }

    // All other methods require a valid vtoken.
    let Some(caller_vtoken) = extract_vtoken_pub(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::err(
                req.id,
                -32001,
                "Missing or invalid Authorization header",
            )),
        );
    };
    {
        let registry = state.clients.registry.read().await;
        if registry.get_by_vtoken(&caller_vtoken).is_none() {
            warn!(method = %req.method, "MCP request with unknown vtoken");
            return (
                StatusCode::UNAUTHORIZED,
                Json(JsonRpcResponse::err(req.id, -32001, UNKNOWN_VTOKEN_MSG)),
            );
        }
    }

    match req.method.as_str() {
        "tools/list" => {
            let tools = serde_json::json!({
                "tools": [
                    protocol::list_agents_def(),
                    protocol::call_agent_def(),
                ]
            });
            (StatusCode::OK, Json(JsonRpcResponse::ok(req.id, tools)))
        }

        "tools/call" => {
            let result = handle_tools_call(&state, &caller_vtoken, &req.params).await;
            match result {
                Ok(value) => (StatusCode::OK, Json(JsonRpcResponse::ok(req.id, value))),
                Err(msg) => (
                    StatusCode::OK, // MCP errors are reported at the JSON-RPC level, not HTTP
                    Json(JsonRpcResponse::err(req.id, -32602, msg)),
                ),
            }
        }

        other => (
            StatusCode::OK,
            Json(JsonRpcResponse::err(
                req.id,
                -32601,
                format!("Method not found: {other}"),
            )),
        ),
    }
}

pub(crate) async fn handle_tools_call(
    state: &Arc<HubState>,
    caller_vtoken: &str,
    params: &Value,
) -> Result<Value, String> {
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("Missing 'name' field in tools/call params")?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    match tool_name {
        "list_agents" => Ok(list_agents(state).await),

        "call_agent" => {
            let target_name = arguments
                .get("name")
                .and_then(Value::as_str)
                .ok_or("Missing 'name' in call_agent arguments")?
                .to_string();
            let message = arguments
                .get("message")
                .and_then(Value::as_str)
                .ok_or("Missing 'message' in call_agent arguments")?
                .to_string();
            let session = arguments
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string);

            // Auto-fill context from DB: look up the most recently active conversation
            // for the caller's vtoken.  This eliminates the need for the LLM to supply
            // hidden `_hub_vctx` / `_hub_real_ctx` / `_hub_peer` arguments.
            let ctx_info = state
                .store
                .get_active_ctx_for_vtoken(caller_vtoken)
                .await
                .map_err(|e| format!("DB error resolving caller context: {e}"))?
                .ok_or_else(|| {
                    "No active conversation found for caller; \
                     the Hub must have delivered at least one message to this Agent first."
                        .to_string()
                })?;

            // Depth guard: reject calls that would exceed the maximum chain length.
            if ctx_info.a2a_depth >= MAX_A2A_DEPTH {
                return Err(format!(
                    "A2A call rejected: maximum call depth ({MAX_A2A_DEPTH}) reached. \
                     This prevents runaway recursive agent loops."
                ));
            }

            let ctx = CallAgentContext {
                caller_vtoken: caller_vtoken.to_string(),
                vctx: ctx_info.vctx,
                real_ctx: ctx_info.real_ctx,
                peer_user_id: ctx_info.peer_user_id,
                a2a_depth: ctx_info.a2a_depth,
            };
            let call_params = CallAgentParams {
                target_name,
                message,
                session,
            };

            Ok(call_agent(state, ctx, call_params).await)
        }

        other => Err(format!("Unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request, http::StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::{
        hub::{AdminConfig, HubState, InMemoryQueue},
        ilink::UpstreamClient,
        server::build_router,
        store::Store,
    };

    async fn make_state() -> Arc<HubState> {
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        let upstream =
            Arc::new(UpstreamClient::new("sk-test".to_string(), None).expect("upstream"));
        let queue = Arc::new(InMemoryQueue::new());
        let (_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        HubState::new(
            upstream,
            store,
            queue,
            shutdown_rx,
            "test-relay-secret".to_string(),
            AdminConfig::from_env(),
        )
    }

    fn json_rpc_body(method: &str, id: i32) -> Body {
        Body::from(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#
        ))
    }

    // ─── jsonrpc version validation ──────────────────────────────────────────

    #[tokio::test]
    async fn handle_mcp_rejects_invalid_jsonrpc_version() {
        let state = make_state().await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"1.0","id":1,"method":"initialize","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ─── initialize / notifications/initialized ──────────────────────────────

    #[tokio::test]
    async fn handle_mcp_initialize_succeeds_without_auth() {
        let state = make_state().await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(json_rpc_body("initialize", 1))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("result").is_some(), "must have result field");
        assert!(
            json["result"].get("protocolVersion").is_some(),
            "result must include protocolVersion"
        );
    }

    #[tokio::test]
    async fn handle_mcp_notifications_initialized_succeeds_without_auth() {
        let state = make_state().await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(json_rpc_body("notifications/initialized", 2))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─── authentication ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn handle_mcp_tools_list_without_auth_returns_401() {
        let state = make_state().await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(json_rpc_body("tools/list", 3))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "tools/list must require auth"
        );
    }

    #[tokio::test]
    async fn handle_mcp_tools_list_with_unknown_vtoken_returns_401() {
        let state = make_state().await;
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    // Use a well-formed but unregistered vtoken
                    .header(
                        "authorization",
                        "Bearer vhub_0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .body(json_rpc_body("tools/list", 4))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handle_mcp_tools_list_with_valid_vtoken_returns_tools() {
        let state = make_state().await;

        // Register a client so we have a valid vtoken.
        let out = crate::server::pairing::register_client_in_hub(
            &state,
            "test-backend".to_string(),
            None,
            None,
        )
        .await;
        let vtoken = &out.plaintext;

        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {vtoken}"))
                    .body(json_rpc_body("tools/list", 5))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = json["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(
            names.contains(&"list_agents"),
            "list_agents must be in tools"
        );
        assert!(names.contains(&"call_agent"), "call_agent must be in tools");
    }

    // ─── unknown method ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn handle_mcp_unknown_method_returns_method_not_found() {
        let state = make_state().await;

        let out = crate::server::pairing::register_client_in_hub(
            &state,
            "test-backend".to_string(),
            None,
            None,
        )
        .await;
        let vtoken = &out.plaintext;

        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {vtoken}"))
                    .body(json_rpc_body("some/unknown/method", 6))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let err = json.get("error").expect("error field");
        assert_eq!(err["code"], -32601);
        assert!(err["message"].as_str().unwrap_or("").contains("not found"));
    }

    // ─── tools/call adversarial ──────────────────────────────────────────────

    #[tokio::test]
    async fn handle_mcp_tools_call_missing_name_returns_error() {
        let state = make_state().await;

        let out = crate::server::pairing::register_client_in_hub(
            &state,
            "caller-backend".to_string(),
            None,
            None,
        )
        .await;
        let vtoken = &out.plaintext;

        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {vtoken}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"arguments":{}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Missing 'name' → JSON-RPC error code -32602
        assert_eq!(json["error"]["code"], -32602, "expected -32602: {json}");
    }
}
