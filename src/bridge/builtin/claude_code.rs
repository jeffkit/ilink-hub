//! Built-in `claude-code` profile: wraps the `claude` CLI with session continuity.
//!
//! Reads P0 env vars, calls `claude --output-format stream-json [--resume <uuid>]`,
//! and streams text output to the parent bridge via `ILINK_PARTIAL:` stdout lines.
//!
//! Each assistant text chunk is written immediately as:
//!
//!   ILINK_PARTIAL:<json-encoded-string>
//!
//! When the stream ends, the final P0 session line is written:
//!
//!   ILINK_SESSION:<new_session_id>
//!
//! The response body is left empty so the bridge does not send a duplicate final message.
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// JSON shape of a single event line from `claude --output-format stream-json`.
#[derive(Debug, Deserialize)]
struct ClaudeStreamEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    /// Final result text (present on `type == "result"` events).
    result: Option<String>,
    /// Session ID (present on `type == "result"` events).
    session_id: Option<String>,
    /// Present on `type == "result"` events (`"success"` or error subtype).
    #[allow(dead_code)]
    subtype: Option<String>,
    /// Present on `type == "assistant"` events.
    message: Option<ClaudeMessage>,
}

/// Nested message structure in `type == "assistant"` stream events.
#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    content: Option<Vec<ClaudeContentBlock>>,
}

/// A single content block within a `ClaudeMessage`.
#[derive(Debug, Deserialize)]
struct ClaudeContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
}

pub async fn run() -> Result<()> {
    let message = std::env::var("ILINK_MESSAGE").unwrap_or_default();
    let session_id = std::env::var("ILINK_SESSION_ID").unwrap_or_default();
    // ILINK_STREAMING is injected by the bridge: "1" (default) = stream partials,
    // "0" = one-shot mode (emit full text to stdout at the end, no ILINK_PARTIAL lines).
    let streaming = std::env::var("ILINK_STREAMING")
        .map(|v| v.trim() != "0")
        .unwrap_or(true);

    let new_session_id = if !session_id.is_empty() {
        match invoke_claude(&message, &session_id, streaming).await {
            Ok(sid) => sid,
            Err(e) => {
                eprintln!("[claude-code] --resume failed ({e:#}), retrying as new session");
                invoke_claude(&message, "", streaming).await?
            }
        }
    } else {
        invoke_claude(&message, "", streaming).await?
    };

    // P0 output: optional session line only.
    // All response text was already streamed via ILINK_PARTIAL during execution.
    if let Some(sid) = &new_session_id {
        if !sid.is_empty() {
            println!("ILINK_SESSION:{sid}");
        }
    }

    Ok(())
}

/// Dispatch to streaming or one-shot mode based on `streaming`.
async fn invoke_claude(message: &str, session_id: &str, streaming: bool) -> Result<Option<String>> {
    if streaming {
        stream_claude(message, session_id).await
    } else {
        oneshot_claude(message, session_id).await
    }
}

/// Call `claude --output-format stream-json`, emit every assistant text chunk as an
/// `ILINK_PARTIAL:` stdout line, and return the session ID from the result event.
///
/// All visible response text is streamed via ILINK_PARTIAL. When the model uses tools
/// between turns, the final assistant reply may only appear in `result.result` (with no
/// preceding `assistant` event); we emit it as an extra ILINK_PARTIAL in that case.
async fn stream_claude(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "stream-json".into(),
        "--dangerously-skip-permissions".into(),
    ];

    if let Ok(model) = std::env::var("ILINK_CLAUDE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    args.push("-p".into());
    args.push(message.to_string());

    let mut cmd = Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `claude`; ensure it is installed and in PATH")?;

    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        tokio::io::BufReader::new(child_stderr)
            .read_to_end(&mut buf)
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    // Track the last partial text sent so we can detect when result.result differs.
    let mut last_partial: Option<String> = None;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read claude stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<ClaudeStreamEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    if let Some(blocks) = &msg.content {
                        let text: String = blocks
                            .iter()
                            .filter(|b| b.block_type.as_deref() == Some("text"))
                            .filter_map(|b| b.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            println!("ILINK_PARTIAL:{}", serde_json::to_string(&text)?);
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                            last_partial = Some(text);
                        }
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id;
                // When the model uses tools between turns, the final assistant reply text
                // may only appear in result.result and have no corresponding assistant event.
                // Send it as a partial if it differs from the last streamed chunk.
                if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    let already_sent = last_partial.as_deref() == Some(result_text.as_str());
                    if !already_sent {
                        println!("ILINK_PARTIAL:{}", serde_json::to_string(&result_text)?);
                        std::io::Write::flush(&mut std::io::stdout()).ok();
                    }
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for claude")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() && found_session_id.is_none() {
        let detail = if !stderr.is_empty() {
            stderr
        } else {
            String::from("(no output)")
        };
        anyhow::bail!(
            "claude exited with status {:?}\nstderr: {detail}",
            status.code()
        );
    }

    Ok(found_session_id)
}

/// Call `claude --output-format json` (one-shot) and print the full reply text to stdout
/// so the bridge captures it as the final response body.  No `ILINK_PARTIAL:` lines are
/// emitted; the session ID is written as `ILINK_SESSION:<id>` on the first stdout line.
async fn oneshot_claude(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "json".into(),
        "--dangerously-skip-permissions".into(),
    ];

    if let Ok(model) = std::env::var("ILINK_CLAUDE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    args.push("-p".into());
    args.push(message.to_string());

    let mut cmd = tokio::process::Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd
        .spawn()
        .context("failed to spawn `claude`; ensure it is installed and in PATH")?;

    let output = child.wait_with_output().await.context("wait for claude")?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();

    // Parse `--output-format json` result.
    // Claude CLI ≥ 2.1.153 emits a JSON array of all events (system, assistant,
    // rate_limit_event, result, …) instead of a single result object.
    // Older versions output a single JSON object with `result` and `session_id`.
    // Handle both formats so a CLI upgrade doesn't silently break one-shot mode.
    let event: ClaudeStreamEvent = {
        let trimmed = stdout_str.trim();
        if trimmed.starts_with('[') {
            let events: Vec<ClaudeStreamEvent> = serde_json::from_str(trimmed)
                .with_context(|| format!("parse claude json output: {stdout_str}"))?;
            events
                .into_iter()
                .find(|e| e.event_type.as_deref() == Some("result"))
                .ok_or_else(|| {
                    anyhow::anyhow!("no result event in claude json output: {stdout_str}")
                })?
        } else {
            serde_json::from_str(trimmed)
                .with_context(|| format!("parse claude json output: {stdout_str}"))?
        }
    };

    if !output.status.success() && event.session_id.is_none() {
        let detail = if !stderr.is_empty() {
            stderr
        } else {
            String::from("(no output)")
        };
        anyhow::bail!(
            "claude exited with status {:?}\nstderr: {detail}",
            output.status.code()
        );
    }

    let found_session_id = event.session_id.clone();

    if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
        // Emit session id first (bridge splits on cli_session_first_line_prefix).
        if let Some(ref sid) = found_session_id {
            if !sid.is_empty() {
                println!("ILINK_SESSION:{sid}");
            }
        }
        println!("{result_text}");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        // Return None so the outer run() does not emit a duplicate ILINK_SESSION line.
        return Ok(None);
    }

    Ok(found_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_result_event() {
        let json =
            r#"{"type":"result","subtype":"success","result":"Hello!","session_id":"sess-abc"}"#;
        let event: ClaudeStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("result"));
        assert_eq!(event.result.as_deref(), Some("Hello!"));
        assert_eq!(event.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(event.subtype.as_deref(), Some("success"));
    }

    #[test]
    fn deserialize_assistant_event_with_text_block() {
        let json =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi there"}]}}"#;
        let event: ClaudeStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("assistant"));
        let blocks = event.message.unwrap().content.unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type.as_deref(), Some("text"));
        assert_eq!(blocks[0].text.as_deref(), Some("Hi there"));
    }

    #[test]
    fn deserialize_unknown_event_does_not_panic() {
        let json = r#"{"type":"system","subtype":"init","session_id":"sess-new"}"#;
        let event: ClaudeStreamEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type.as_deref(), Some("system"));
        assert!(event.result.is_none());
        assert!(event.message.is_none());
    }

    /// Claude CLI ≥ 2.1.153 changed `--output-format json` to emit a JSON array of all
    /// events (system, assistant, rate_limit_event, result) instead of a single result
    /// object.  The oneshot parser must extract the result event from the array.
    #[test]
    fn oneshot_parses_json_array_format() {
        let json = r#"[
            {"type":"system","subtype":"init","session_id":"sess-xyz"},
            {"type":"assistant","message":{"content":[{"type":"text","text":"Hi"}]}},
            {"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}},
            {"type":"result","subtype":"success","result":"Final answer","session_id":"sess-xyz"}
        ]"#;

        let trimmed = json.trim();
        assert!(trimmed.starts_with('['), "test input must be an array");

        let events: Vec<ClaudeStreamEvent> = serde_json::from_str(trimmed).unwrap();
        let result_event = events
            .into_iter()
            .find(|e| e.event_type.as_deref() == Some("result"))
            .expect("result event must be found");

        assert_eq!(result_event.result.as_deref(), Some("Final answer"));
        assert_eq!(result_event.session_id.as_deref(), Some("sess-xyz"));
    }

    /// Regression test using the full real-world JSON array from CLI v2.1.153+ with extra fields.
    #[test]
    fn oneshot_parses_real_world_json_array() {
        let json = r#"[{"type":"system","subtype":"init","cwd":"/Users/kongjie/projects/ilink-hub","session_id":"7cd2894b-14b2-4f85-b5e8-6fb8bc571cf0","tools":["Task","AskUserQuestion","Bash"],"mcp_servers":[{"name":"plugin:argusai:argusai","status":"pending"}],"model":"claude-sonnet-4-6","permissionMode":"bypassPermissions","slash_commands":["daily-standup"],"apiKeySource":"none","claude_code_version":"2.1.153","output_style":"default","agents":[],"skills":[],"plugins":[],"analytics_disabled":false,"product_feedback_disabled":false,"uuid":"9372f0d2-4f9c-4c0a-b183-0d81451aed9b","memory_paths":{"auto":"/tmp/memory/"},"fast_mode_state":"off"},{"type":"assistant","message":{"model":"claude-sonnet-4-6","id":"msg_01VmAu37HNMuZLz92EAcAwv1","type":"message","role":"assistant","content":[{"type":"text","text":"你好！有什么我可以帮你的吗？"}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":13924,"cache_read_input_tokens":12758},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"7cd2894b-14b2-4f85-b5e8-6fb8bc571cf0","uuid":"3f3d7d18-41da-4d87-ba6a-a9f50251b82c","request_id":"req_011Cc6yh3haovj7ig9Fr6ozw"},{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1781618400,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false},"uuid":"8308bf99-2c6e-460c-b9df-ccc105251444","session_id":"7cd2894b-14b2-4f85-b5e8-6fb8bc571cf0"},{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":2628,"duration_api_ms":2587,"ttft_ms":2549,"num_turns":1,"result":"你好！有什么我可以帮你的吗？","stop_reason":"end_turn","session_id":"7cd2894b-14b2-4f85-b5e8-6fb8bc571cf0","total_cost_usd":0.0563514,"usage":{"input_tokens":3,"cache_creation_input_tokens":13924,"cache_read_input_tokens":12758,"output_tokens":20},"modelUsage":{"claude-sonnet-4-6":{"inputTokens":3,"outputTokens":20}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","uuid":"c2fd8595-2544-4c3f-bc2d-8d7db4bf941d"}]"#;

        let trimmed = json.trim();
        assert!(trimmed.starts_with('['), "test input must be an array");

        let events: Vec<ClaudeStreamEvent> = serde_json::from_str(trimmed).unwrap();
        let result_event = events
            .into_iter()
            .find(|e| e.event_type.as_deref() == Some("result"))
            .expect("result event must be found");

        assert_eq!(
            result_event.result.as_deref(),
            Some("你好！有什么我可以帮你的吗？")
        );
        assert_eq!(
            result_event.session_id.as_deref(),
            Some("7cd2894b-14b2-4f85-b5e8-6fb8bc571cf0")
        );
    }
}
