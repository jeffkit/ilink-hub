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
use super::tools::{call_agent, list_agents, CallAgentContext, CallAgentParams};

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

async fn handle_tools_call(
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

            // We need the caller's vctx to push WeChat messages.  The caller
            // must supply it in the `_hub_vctx` meta-argument, which the Hub
            // injects into every inbound message via HubExt.  When the bridge
            // relays the message to the Agent, the Agent receives this in its
            // tool call context (stored in `HubExt.session_name` and decoded
            // from the message's `context_token`).
            //
            // Practical approach: the caller passes `_hub_vctx` and
            // `_hub_real_ctx` and `_hub_peer` as hidden arguments.  The bridge
            // SDK should fill these automatically from the HubExt it received.
            let vctx = arguments
                .get("_hub_vctx")
                .and_then(Value::as_str)
                .ok_or("Missing '_hub_vctx' in call_agent arguments (should be set by the bridge)")?
                .to_string();
            let real_ctx = arguments
                .get("_hub_real_ctx")
                .and_then(Value::as_str)
                .ok_or("Missing '_hub_real_ctx' in call_agent arguments")?
                .to_string();
            let peer_user_id = arguments
                .get("_hub_peer")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let ctx = CallAgentContext {
                caller_vtoken: caller_vtoken.to_string(),
                vctx,
                real_ctx,
                peer_user_id,
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
