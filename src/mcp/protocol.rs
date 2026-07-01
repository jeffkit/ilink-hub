//! Minimal MCP JSON-RPC 2.0 types for the Streamable HTTP transport.
//!
//! We implement only what the two tools require:
//!   initialize / initialized, tools/list, tools/call.
//!
//! The spec allows either a single JSON-RPC object or an array (batch) per
//! request.  We only handle the single-object form; batches are rejected with
//! an error response.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── JSON-RPC envelope ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

// ─── MCP tool definitions ─────────────────────────────────────────────────────

/// Returned by `tools/list`.
#[derive(Debug, Serialize)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub fn list_agents_def() -> ToolDef {
    ToolDef {
        name: "list_agents",
        description: "List all registered Agent backends and their online status.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

pub fn call_agent_def() -> ToolDef {
    ToolDef {
        name: "call_agent",
        description: concat!(
            "Send a message to another Agent backend and wait for its reply.\n",
            "The Hub will display the call and the reply to the WeChat user.\n",
            "Pass `session` to continue a previous conversation with the same Agent; ",
            "omit it (or pass null) to start a fresh session."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Registered name of the target Agent backend."
                },
                "message": {
                    "type": "string",
                    "description": "The message to send to the target Agent."
                },
                "session": {
                    "type": ["string", "null"],
                    "description": "Session name to resume; null/omitted starts a new session."
                }
            },
            "required": ["name", "message"]
        }),
    }
}

// ─── MCP initialize result ────────────────────────────────────────────────────

pub fn server_info() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "ilink-hub",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}
