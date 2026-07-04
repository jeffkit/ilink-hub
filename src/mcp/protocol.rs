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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── JsonRpcResponse ─────────────────────────────────────────────────────

    #[test]
    fn json_rpc_response_ok_sets_result_clears_error() {
        let id = Some(serde_json::json!(42));
        let result = serde_json::json!({"foo": "bar"});
        let resp = JsonRpcResponse::ok(id.clone(), result.clone());

        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, id);
        assert_eq!(resp.result, Some(result));
        assert!(resp.error.is_none());
    }

    #[test]
    fn json_rpc_response_err_sets_error_clears_result() {
        let id = Some(serde_json::json!("req-1"));
        let resp = JsonRpcResponse::err(id.clone(), -32600, "Invalid Request");

        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, id);
        assert!(resp.result.is_none());
        let err = resp.error.expect("error must be set");
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid Request");
    }

    #[test]
    fn json_rpc_response_ok_with_null_id() {
        let resp = JsonRpcResponse::ok(None, serde_json::json!({}));
        assert!(resp.id.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn json_rpc_response_serialized_ok_omits_error_field() {
        let resp = JsonRpcResponse::ok(Some(serde_json::json!(1)), serde_json::json!("result"));
        let json_str = serde_json::to_string(&resp).expect("serialize ok");
        // skip_serializing_if = "Option::is_none" must suppress the "error" key
        assert!(
            !json_str.contains("\"error\""),
            "error field must be absent: {json_str}"
        );
        assert!(json_str.contains("\"result\""));
    }

    #[test]
    fn json_rpc_response_serialized_err_omits_result_field() {
        let resp = JsonRpcResponse::err(Some(serde_json::json!(1)), -32601, "Method not found");
        let json_str = serde_json::to_string(&resp).expect("serialize err");
        assert!(
            !json_str.contains("\"result\""),
            "result field must be absent: {json_str}"
        );
        assert!(json_str.contains("\"error\""));
        assert!(json_str.contains("-32601"));
    }

    // ─── Tool definitions ────────────────────────────────────────────────────

    #[test]
    fn list_agents_def_has_correct_name_and_empty_schema() {
        let def = list_agents_def();
        assert_eq!(def.name, "list_agents");
        assert!(!def.description.is_empty());
        // inputSchema.required must be an empty array
        let required = def.input_schema.get("required").expect("required present");
        assert_eq!(required, &serde_json::json!([]));
        // No required parameters
        let props = def
            .input_schema
            .get("properties")
            .expect("properties present");
        assert_eq!(props, &serde_json::json!({}));
    }

    #[test]
    fn call_agent_def_has_correct_name_and_required_fields() {
        let def = call_agent_def();
        assert_eq!(def.name, "call_agent");
        assert!(!def.description.is_empty());

        let required = def
            .input_schema
            .get("required")
            .expect("required present")
            .as_array()
            .expect("required is array");
        let required_strs: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(required_strs.contains(&"name"), "name must be required");
        assert!(
            required_strs.contains(&"message"),
            "message must be required"
        );

        let props = def
            .input_schema
            .get("properties")
            .expect("properties present");
        assert!(props.get("name").is_some());
        assert!(props.get("message").is_some());
        assert!(props.get("session").is_some());
    }

    #[test]
    fn call_agent_def_session_is_not_required() {
        let def = call_agent_def();
        let required = def
            .input_schema
            .get("required")
            .expect("required present")
            .as_array()
            .expect("required is array");
        let required_strs: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(
            !required_strs.contains(&"session"),
            "session must NOT be required (it is optional)"
        );
    }

    // ─── server_info ─────────────────────────────────────────────────────────

    #[test]
    fn server_info_contains_mcp_protocol_version() {
        let info = server_info();
        assert_eq!(
            info.get("protocolVersion").and_then(Value::as_str),
            Some("2024-11-05")
        );
    }

    #[test]
    fn server_info_contains_tools_capability() {
        let info = server_info();
        let caps = info.get("capabilities").expect("capabilities present");
        assert!(
            caps.get("tools").is_some(),
            "tools capability must be declared"
        );
    }

    #[test]
    fn server_info_server_info_field_has_name() {
        let info = server_info();
        let srv = info.get("serverInfo").expect("serverInfo present");
        assert_eq!(srv.get("name").and_then(Value::as_str), Some("ilink-hub"));
        // version is set from Cargo.toml at compile time — just check it's a non-empty string
        let version = srv.get("version").and_then(Value::as_str).unwrap_or("");
        assert!(!version.is_empty(), "version must not be empty");
    }

    // ─── Deserialization of JsonRpcRequest ───────────────────────────────────

    #[test]
    fn json_rpc_request_deserializes_with_all_fields() {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {"foo": "bar"}
        }"#;
        let req: JsonRpcRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(serde_json::json!(1)));
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.params.get("foo").and_then(Value::as_str), Some("bar"));
    }

    #[test]
    fn json_rpc_request_params_defaults_to_null_when_absent() {
        let json = r#"{"jsonrpc": "2.0", "id": null, "method": "initialize"}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).expect("deserialize");
        assert!(
            req.params.is_null(),
            "params must default to null when absent"
        );
    }
}
