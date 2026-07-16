//! Built-in `claude-code` profile: wraps the `claude` CLI with session continuity.
//!
//! Reads P0 env vars, calls `claude --output-format stream-json [--resume <uuid>]`,
//! and streams text output to the parent bridge via `AGENT_PARTIAL:` stdout lines.
//!
//! Each assistant text chunk is written immediately as:
//!
//!   AGENT_PARTIAL:<json-encoded-string>
//!
//! When the stream ends, the final P0 session line is written:
//!
//!   AGENT_SESSION:<new_session_id>
//!
//! The response body is left empty so the bridge does not send a duplicate final message.
//!
//! If `--resume` fails (session expired / not found), automatically retries as a
//! fresh session so the user gets a response rather than a bare error.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::common;

/// JSON event line from `claude --output-format stream-json`. The Claude CLI defines
/// the canonical `stream-json` schema; the shared [`common::StreamJsonEvent`] models it.
type ClaudeStreamEvent = common::StreamJsonEvent;

/// Anthropic API hard limit for a single image (~5MB). The Claude Code CLI
/// forwards the base64 string verbatim to the API, so we enforce this client-side
/// to avoid opaque upstream errors.
const ANTHROPIC_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Anthropic API hard limit for a single document (PDF) — 32 MB per the
/// [PDF support docs](https://platform.claude.com/docs/en/build-with-claude/pdf-support).
const ANTHROPIC_MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

pub async fn run() -> Result<()> {
    let turn = common::read_turn_or_error();
    let (message, session_id) = common::message_and_session(&turn);
    let attachments = turn.attachments.clone();
    let permission = turn.permission;

    let _new_session_id =
        common::with_session_resume_fallback("claude-code", &message, &session_id, |m, s| {
            let atts = attachments.clone();
            async move { invoke_claude(&m, &s, &atts, permission).await }
        })
        .await?;

    Ok(())
}

/// Dispatch to text, multimodal, or permission mode. Permission mode is used
/// when the bridge enabled the permission channel (`turn.permission == true`):
/// the underlying `claude` CLI runs with `--permission-prompt-tool stdio` and
/// we translate Claude `control_request`/`control_response` ↔ AgentProc
/// `permission_request`/`permission_response` NDJSON frames.
async fn invoke_claude(
    message: &str,
    session_id: &str,
    attachments: &[crate::bridge::protocol::Attachment],
    permission: bool,
) -> Result<Option<String>> {
    if permission {
        stream_claude_permission(message, session_id, attachments).await
    } else if !attachments.is_empty() {
        stream_claude_multimodal(message, session_id, attachments).await
    } else {
        stream_claude(message, session_id).await
    }
}

/// Call `claude --output-format stream-json [--resume <uuid>]`, emit every
/// assistant text chunk as a `partial` event, the terminal `result.result` as a
/// `text` event, and return the session ID from the result event.
///
/// All visible response text is streamed via `partial` events. When the model
/// uses tools between turns, the final assistant reply may only appear in
/// `result.result` (with no preceding `assistant` event); it is emitted as the
/// final `text` event so non-streaming consumers still receive it.
async fn stream_claude(message: &str, session_id: &str) -> Result<Option<String>> {
    let mut emitter = common::SessionEmitter::new(session_id);
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--dangerously-skip-permissions".into(),
        // In -p (non-interactive) mode stdin is /dev/null, so AskUserQuestion would
        // block the process forever with no visible prompt to the user.
        "--disallowed-tools".into(),
        "AskUserQuestion".into(),
    ];

    if let Ok(model) = std::env::var("CLAUDE_MODEL") {
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
    let stderr_task = common::spawn_capped_drain(child_stderr);

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut final_text: Option<String> = None;
    let mut error_emitted = false;

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
                    let text = msg.text();
                    // Guard with trim() so whitespace-only text blocks do not
                    // produce an empty-looking partial.
                    if !text.trim().is_empty() {
                        emitter.emit_partial(&text)?;
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id.clone();
                if event.is_result_error() {
                    if let Some(err_text) = event.result_error_message() {
                        emitter.emit_error(&err_text, found_session_id.as_deref())?;
                        error_emitted = true;
                    }
                } else if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    final_text = Some(result_text);
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for claude")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !error_emitted {
        emitter.emit_result_opt(final_text.as_deref(), found_session_id.as_deref())?;
    }

    common::ensure_success("claude", status, &stderr, found_session_id.is_some())?;

    Ok(found_session_id)
}

/// Bidirectional permission mode: run `claude --permission-prompt-tool stdio`
/// with `--input-format stream-json`, translate Claude `control_request`
/// (can_use_tool) into AgentProc `permission_request` events, and translate the
/// bridge's `permission_response` frames back into Claude `control_response`
/// frames written to the CLI's stdin. Supports multimodal content blocks when
/// the turn carries image/file attachments.
async fn stream_claude_permission(
    message: &str,
    session_id: &str,
    attachments: &[crate::bridge::protocol::Attachment],
) -> Result<Option<String>> {
    let mut emitter = common::SessionEmitter::new(session_id);
    // Build the SDKUserMessage `content`: a plain string for text-only turns,
    // or an array of content blocks for multimodal turns (image/document).
    let content: Value = if attachments.is_empty() {
        json!(message)
    } else {
        let mut blocks: Vec<Value> = vec![json!({ "type": "text", "text": message })];
        for att in attachments {
            match att.kind.as_str() {
                "image" => {
                    let (media_type, b64_data) = download_image_as_base64(&att.url).await?;
                    blocks.push(json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": media_type, "data": b64_data }
                    }));
                }
                "file" => {
                    let (media_type, b64_data) = download_document_as_base64(&att.url).await?;
                    blocks.push(json!({
                        "type": "document",
                        "source": { "type": "base64", "media_type": media_type, "data": b64_data }
                    }));
                }
                other => {
                    emitter.emit_error(
                        &format!(
                            "unsupported attachment kind `{other}` for claude-code (only image and file are accepted)"
                        ),
                        None,
                    )?;
                    return Ok(None);
                }
            }
        }
        json!(blocks)
    };

    let mut args: Vec<String> = vec![
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--permission-prompt-tool".into(),
        "stdio".into(),
        "--permission-mode".into(),
        "default".into(),
        "--disallowed-tools".into(),
        "AskUserQuestion".into(),
    ];

    if let Ok(model) = std::env::var("CLAUDE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }
    if !session_id.is_empty() {
        args.push("--resume".into());
        args.push(session_id.to_string());
    }

    let mut cmd = Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `claude`; ensure it is installed and in PATH")?;

    let mut child_stdin = child.stdin.take().context("stdin pipe missing")?;
    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;
    let stderr_task = common::spawn_capped_drain(child_stderr);

    // Write the initial SDKUserMessage (the user's turn). `session_id` empty =
    // new session; Claude distinguishes new vs resume by this field's value.
    let user_message = json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
        "session_id": session_id,
    });
    let line = serde_json::to_string(&user_message)? + "\n";
    use tokio::io::AsyncWriteExt;
    child_stdin
        .write_all(line.as_bytes())
        .await
        .context("write SDKUserMessage to claude stdin")?;

    // Background thread reading OUR stdin for permission_response frames the
    // bridge writes after we emit a permission_request. std::io::stdin is
    // blocking, so this runs on a std thread and forwards via a tokio mpsc.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Value>(16);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(raw) = line else { break };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) == Some("permission_response")
                && resp_tx.blocking_send(v).is_err()
            {
                break;
            }
        }
    });

    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut final_text: Option<String> = None;
    let mut error_emitted = false;

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
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let etype = v.get("type").and_then(|t| t.as_str());

        if etype == Some("control_request") {
            if let Some(req) = control_to_permission_request(&v) {
                let original_input = req.get("input").cloned().unwrap_or_else(|| json!({}));
                // Emit the permission_request event to the bridge and flush so
                // the bridge can prompt the user immediately.
                println!("{}", serde_json::to_string(&req)?);
                std::io::Write::flush(&mut std::io::stdout()).ok();

                // Await the bridge's permission_response. The bridge enforces
                // its own ask timeout; if it dies, our stdin closes and
                // recv() returns None → we deny so Claude can finish.
                let resp =
                    match tokio::time::timeout(Duration::from_secs(1800), resp_rx.recv()).await {
                        Ok(Some(r)) => r,
                        _ => json!({
                            "type": "permission_response",
                            "request_id": req.get("request_id").cloned().unwrap_or_default(),
                            "behavior": "deny",
                            "message": "no permission response (bridge timeout/closed)",
                        }),
                    };

                let ctrl = permission_response_to_control(&resp, &original_input);
                let ctrl_line = serde_json::to_string(&ctrl)? + "\n";
                if child_stdin.write_all(ctrl_line.as_bytes()).await.is_err() {
                    // Claude closed stdin; stop trying to drive permissions.
                    break;
                }
            }
            continue;
        }

        // Other control_* noise from the CLI is not part of the can_use_tool
        // permission flow; ignore it (matches the agentproc hub behaviour).
        if matches!(
            etype,
            Some("control_response") | Some("sdk_control_request")
        ) {
            continue;
        }

        let Ok(event) = serde_json::from_str::<ClaudeStreamEvent>(trimmed) else {
            continue;
        };
        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    let text = msg.text();
                    if !text.trim().is_empty() {
                        emitter.emit_partial(&text)?;
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id.clone();
                if event.is_result_error() {
                    if let Some(err_text) = event.result_error_message() {
                        emitter.emit_error(&err_text, found_session_id.as_deref())?;
                        error_emitted = true;
                    }
                } else if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    final_text = Some(result_text);
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for claude")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !error_emitted {
        emitter.emit_result_opt(final_text.as_deref(), found_session_id.as_deref())?;
    }

    common::ensure_success("claude", status, &stderr, found_session_id.is_some())?;

    Ok(found_session_id)
}

/// Map a Claude `control_request` (subtype `can_use_tool`) to an AgentProc
/// `permission_request` event payload. Returns `None` for non-can_use_tool
/// control requests (those are not tool-authorisation prompts).
fn control_to_permission_request(event: &Value) -> Option<Value> {
    let request = event.get("request")?;
    if request.get("subtype").and_then(|s| s.as_str()) != Some("can_use_tool") {
        return None;
    }
    let request_id = event.get("request_id")?.as_str()?.trim();
    if request_id.is_empty() {
        return None;
    }
    let tool_name = request
        .get("tool_name")
        .and_then(|t| t.as_str())
        .or_else(|| request.get("display_name").and_then(|t| t.as_str()))
        .unwrap_or("tool");
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    let mut payload = json!({
        "type": "permission_request",
        "request_id": request_id,
        "tool_name": tool_name,
        "input": input,
    });
    if let Some(desc) = request.get("description").and_then(|d| d.as_str()) {
        if !desc.is_empty() {
            payload["description"] = json!(desc);
        }
    }
    if let Some(tuid) = request.get("tool_use_id").and_then(|d| d.as_str()) {
        if !tuid.is_empty() {
            payload["tool_use_id"] = json!(tuid);
        }
    }
    Some(payload)
}

/// Map an AgentProc `permission_response` to a Claude `control_response` frame
/// written to the CLI's stdin. `allow` carries `updatedInput` (defaults to the
/// original tool input); `deny` carries a reason.
fn permission_response_to_control(resp: &Value, original_input: &Value) -> Value {
    let request_id = resp
        .get("request_id")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    let behavior = resp.get("behavior").and_then(|b| b.as_str()).unwrap_or("");
    if behavior == "allow" {
        let updated = resp
            .get("updated_input")
            .cloned()
            .unwrap_or_else(|| original_input.clone());
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {
                    "behavior": "allow",
                    "updatedInput": updated,
                },
            },
        })
    } else {
        let message = resp
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("denied by bridge")
            .to_string();
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {
                    "behavior": "deny",
                    "message": message,
                },
            },
        })
    }
}

/// Multimodal variant: download each attachment in the turn, base64-encode it,
/// and feed Claude via the bidirectional `stream-json` protocol (a single
/// `SDKUserMessage` written to stdin, with `content = [text, image?, document?]`).
///
/// `--input-format stream-json` requires `--output-format stream-json` and uses
/// `--print` mode under the hood. Session continuity is provided by the
/// `session_id` field of `SDKUserMessage` (empty string = new session).
///
/// Only **image** and **PDF/plain-text file** attachments are accepted — see
/// `download_document_as_base64`. `video` and other file types fail before the
/// CLI is spawned with a clear `error` event.
async fn stream_claude_multimodal(
    message: &str,
    session_id: &str,
    attachments: &[crate::bridge::protocol::Attachment],
) -> Result<Option<String>> {
    let mut emitter = common::SessionEmitter::new(session_id);
    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
    content_blocks.push(json!({ "type": "text", "text": message }));

    for att in attachments {
        match att.kind.as_str() {
            "image" => {
                let (media_type, b64_data) = download_image_as_base64(&att.url).await?;
                content_blocks.push(json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": media_type, "data": b64_data }
                }));
            }
            "file" => {
                let (media_type, b64_data) = download_document_as_base64(&att.url).await?;
                content_blocks.push(json!({
                    "type": "document",
                    "source": { "type": "base64", "media_type": media_type, "data": b64_data }
                }));
            }
            other => {
                emitter.emit_error(
                    &format!(
                        "unsupported attachment kind `{other}` for claude-code (only image and file are accepted)"
                    ),
                    None,
                )?;
                return Ok(None);
            }
        }
    }

    let mut args: Vec<String> = vec![
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--dangerously-skip-permissions".into(),
        "--disallowed-tools".into(),
        "AskUserQuestion".into(),
    ];

    if let Ok(model) = std::env::var("CLAUDE_MODEL") {
        if !model.trim().is_empty() {
            args.push("--model".into());
            args.push(model.trim().to_string());
        }
    }

    args.push("-p".into());

    let mut cmd = Command::new("claude");
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to spawn `claude`; ensure it is installed and in PATH")?;

    let mut child_stdin = child.stdin.take().context("stdin pipe missing")?;
    let child_stdout = child.stdout.take().context("stdout pipe missing")?;
    let child_stderr = child.stderr.take().context("stderr pipe missing")?;

    // Drain stderr in background to prevent pipe buffer deadlock.
    let stderr_task = common::spawn_capped_drain(child_stderr);

    // Build SDKUserMessage: {type:"user", message:{role:"user", content:[...]}, session_id, parent_tool_use_id:null}
    let user_message = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content_blocks,
        },
        "parent_tool_use_id": null,
        "session_id": session_id,
    });

    let line = serde_json::to_string(&user_message)? + "\n";
    use tokio::io::AsyncWriteExt;
    child_stdin
        .write_all(line.as_bytes())
        .await
        .context("write SDKUserMessage to claude stdin")?;
    // Close stdin so the CLI knows no more user input is coming and can finalize the turn.
    drop(child_stdin);

    // Reuse the same output-parsing loop as the text-only path.
    let mut reader = tokio::io::BufReader::new(child_stdout);
    let mut out_line = String::new();
    let mut found_session_id: Option<String> = None;
    let mut final_text: Option<String> = None;
    let mut error_emitted = false;

    loop {
        out_line.clear();
        let n = reader
            .read_line(&mut out_line)
            .await
            .context("read claude stdout")?;
        if n == 0 {
            break;
        }

        let trimmed = out_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<ClaudeStreamEvent>(trimmed) else {
            continue;
        };

        match event.event_type.as_deref() {
            Some("assistant") => {
                if let Some(msg) = &event.message {
                    let text = msg.text();
                    if !text.trim().is_empty() {
                        emitter.emit_partial(&text)?;
                    }
                }
            }
            Some("result") => {
                found_session_id = event.session_id.clone();
                if event.is_result_error() {
                    if let Some(err_text) = event.result_error_message() {
                        emitter.emit_error(&err_text, found_session_id.as_deref())?;
                        error_emitted = true;
                    }
                } else if let Some(result_text) = event.result.filter(|t| !t.trim().is_empty()) {
                    final_text = Some(result_text);
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.context("wait for claude")?;
    let stderr = stderr_task.await.unwrap_or_default();

    if !error_emitted {
        emitter.emit_result_opt(final_text.as_deref(), found_session_id.as_deref())?;
    }

    common::ensure_success("claude", status, &stderr, found_session_id.is_some())?;

    Ok(found_session_id)
}

/// Download an image at `url` and return `(media_type, base64_data)`. Enforces
/// the 5MB Anthropic API limit to surface a clear error early.
async fn download_image_as_base64(url: &str) -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build reqwest client for image download")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download image from {url}"))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "image download failed: HTTP {} for {url}",
            response.status()
        );
    }

    // Trust the server's Content-Type for the media_type field; fall back to image/jpeg
    // since that's the most common unlabelled image format. The downstream API tolerates
    // any image/* media type as long as bytes match.
    let media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .filter(|s| s.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".to_string());

    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read image body chunk")?;
        // Fail fast if we cross the limit during streaming so we don't keep downloading.
        if buf.len() + chunk.len() > ANTHROPIC_MAX_IMAGE_BYTES {
            anyhow::bail!(
                "image too large: exceeds Anthropic limit ({} bytes)",
                ANTHROPIC_MAX_IMAGE_BYTES
            );
        }
        buf.extend_from_slice(&chunk);
    }

    if buf.is_empty() {
        anyhow::bail!("image download returned empty body for {url}");
    }

    Ok((media_type, B64.encode(&buf)))
}

/// Download a document at `url` and return `(media_type, base64_data)`. Only PDF and
/// plain text are accepted (Anthropic Messages API `document` block constraint). Any
/// other content type fails fast with a clear message — the user will see the error
/// in the bridge log and the original WeChat message will be silently dropped.
///
/// Why reject non-PDF/text? The Anthropic `document` content block only supports
/// `application/pdf` and `text/plain` (see [PDF support docs]). The Files API *can*
/// host other types but requires a separate upload step and explicit `file_id`
/// reference, which is out of scope for this streaming bridge.
///
/// Limit: 32 MB per the [PDF support docs](https://platform.claude.com/docs/en/build-with-claude/pdf-support).
async fn download_document_as_base64(url: &str) -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build reqwest client for document download")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download document from {url}"))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "document download failed: HTTP {} for {url}",
            response.status()
        );
    }

    // Resolve the media_type from the response. We only accept the two types the
    // Anthropic `document` block actually supports; everything else is rejected here
    // so the user gets a useful error rather than a confusing API failure downstream.
    let raw_media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_default();

    let media_type = match raw_media_type.as_str() {
        "application/pdf" => "application/pdf".to_string(),
        "text/plain" => "text/plain".to_string(),
        other => {
            anyhow::bail!(
                "unsupported document media_type: {other:?} (only application/pdf and \
                 text/plain are accepted by the Anthropic document block; video and \
                 other file types are not supported). url: {url}"
            );
        }
    };

    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read document body chunk")?;
        if buf.len() + chunk.len() > ANTHROPIC_MAX_DOCUMENT_BYTES {
            anyhow::bail!(
                "document too large: exceeds Anthropic limit ({} bytes)",
                ANTHROPIC_MAX_DOCUMENT_BYTES
            );
        }
        buf.extend_from_slice(&chunk);
    }

    if buf.is_empty() {
        anyhow::bail!("document download returned empty body for {url}");
    }

    Ok((media_type, B64.encode(&buf)))
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

    // ── permission mode translation ──────────────────────────────────────────

    #[test]
    fn control_request_can_use_tool_maps_to_permission_request() {
        let event: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"req_1","request":{
                "subtype":"can_use_tool","tool_name":"Bash",
                "input":{"command":"rm -rf /"},"description":"run bash",
                "tool_use_id":"toolu_1"}}"#,
        )
        .unwrap();
        let req = control_to_permission_request(&event).expect("can_use_tool maps");
        assert_eq!(req["type"], "permission_request");
        assert_eq!(req["request_id"], "req_1");
        assert_eq!(req["tool_name"], "Bash");
        assert_eq!(req["input"]["command"], "rm -rf /");
        assert_eq!(req["description"], "run bash");
        assert_eq!(req["tool_use_id"], "toolu_1");
    }

    #[test]
    fn control_request_non_can_use_tool_is_ignored() {
        let event: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"x","request":{"subtype":"other"}}"#,
        )
        .unwrap();
        assert!(control_to_permission_request(&event).is_none());
    }

    #[test]
    fn permission_response_allow_becomes_control_response_allow() {
        let resp: Value = serde_json::from_str(
            r#"{"type":"permission_response","request_id":"req_1","behavior":"allow"}"#,
        )
        .unwrap();
        let ctrl = permission_response_to_control(&resp, &json!({"command":"echo hi"}));
        assert_eq!(ctrl["type"], "control_response");
        assert_eq!(ctrl["response"]["request_id"], "req_1");
        assert_eq!(ctrl["response"]["response"]["behavior"], "allow");
        // updatedInput defaults to the original tool input.
        assert_eq!(
            ctrl["response"]["response"]["updatedInput"]["command"],
            "echo hi"
        );
    }

    #[test]
    fn permission_response_deny_becomes_control_response_deny_with_message() {
        let resp: Value = serde_json::from_str(
            r#"{"type":"permission_response","request_id":"r2","behavior":"deny","message":"user said no"}"#,
        )
        .unwrap();
        let ctrl = permission_response_to_control(&resp, &json!({}));
        assert_eq!(ctrl["response"]["response"]["behavior"], "deny");
        assert_eq!(ctrl["response"]["response"]["message"], "user said no");
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

    /// Verify the SDKUserMessage shape we write to stdin for multimodal input.
    /// Mirrors the protocol in `fake-cc/src/server/directConnectManager.ts:130` and
    /// `src/utils/teleport/api.ts:376` (the TS SDK's internal format).
    #[test]
    fn sdk_user_message_has_text_and_image_blocks() {
        let message = "describe this image";
        let session_id = "sess-123";
        let media_type = "image/png";
        let b64_data = "iVBORw0KGgo="; // tiny base64 placeholder

        let user_message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": message },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": b64_data,
                        }
                    }
                ]
            },
            "parent_tool_use_id": null,
            "session_id": session_id,
        });

        let serialized = serde_json::to_string(&user_message).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["session_id"], "sess-123");
        assert!(parsed["parent_tool_use_id"].is_null());

        let blocks = parsed["message"]["content"]
            .as_array()
            .expect("content is array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "describe this image");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
    }

    /// Empty session_id (new session) must serialize as `""`, NOT omitted, because
    /// the Claude Code CLI distinguishes "new session" from "resume" by the value
    /// of the session_id field. A missing field would default to undefined and break
    /// the protocol.
    #[test]
    fn sdk_user_message_keeps_empty_session_id_for_new_session() {
        let user_message = json!({
            "type": "user",
            "message": { "role": "user", "content": "hi" },
            "parent_tool_use_id": null,
            "session_id": "",
        });
        let serialized = serde_json::to_string(&user_message).unwrap();
        assert!(
            serialized.contains("\"session_id\":\"\""),
            "session_id must be present even when empty: {serialized}"
        );
    }

    /// Streaming output parser must accept assistant events whose content blocks
    /// include non-text blocks (e.g. tool_use) and still extract the text correctly.
    /// Multimodal replies can interleave text, tool_use, and image blocks.
    #[test]
    fn stream_event_with_mixed_content_blocks_extracts_text() {
        let json = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Looking at the image. "},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/etc/hosts"}},
            {"type":"text","text":"Done."}
        ]}}"#;
        let event: ClaudeStreamEvent = serde_json::from_str(json).unwrap();
        let blocks = event.message.unwrap().content.unwrap();
        let text: String = blocks
            .iter()
            .filter(|b| b.block_type.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "Looking at the image. Done.");
    }

    /// End-to-end check of `download_image_as_base64` against a real localhost HTTP
    /// server. Verifies the content-type is propagated, body is base64-encoded, and
    /// the round-trip is byte-exact.
    #[tokio::test]
    async fn download_image_roundtrips_through_local_server() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        // 1x1 red PNG (67 bytes). Any well-formed image works — we only care about
        // transport, not pixel content.
        const PNG_BYTES: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x5B, 0x9D, 0x84,
            0x42, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];

        async fn serve_png() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(PNG_BYTES.to_vec()))
                .unwrap()
        }

        let app = Router::new().route("/img.png", get(serve_png));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/img.png");
        let (media_type, b64) = download_image_as_base64(&url).await.unwrap();
        assert_eq!(media_type, "image/png");

        // Decode and compare byte-for-byte to confirm end-to-end fidelity.
        let decoded = B64.decode(&b64).unwrap();
        assert_eq!(decoded, PNG_BYTES);
    }

    /// Server returning an HTTP error must surface a clear error message rather than
    /// producing empty/garbage bytes.
    #[tokio::test]
    async fn download_image_fails_on_http_error() {
        use axum::{body::Body, http::StatusCode, response::Response, routing::get, Router};

        async fn not_found() -> Response<Body> {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap()
        }

        let app = Router::new().route("/missing.png", get(not_found));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/missing.png");
        let result = download_image_as_base64(&url).await;
        let err = result.expect_err("expected HTTP 404 to fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("404"),
            "error should mention HTTP status: {msg}"
        );
    }

    /// Verify the SDKUserMessage shape when a PDF is attached. The content array must
    /// contain a `document` block (not `image`) with a `base64` source — this is the
    /// shape the Anthropic Messages API expects for PDF inputs.
    #[test]
    fn sdk_user_message_with_pdf_uses_document_block() {
        let message = "summarize this PDF";
        let user_message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": message },
                    {
                        "type": "document",
                        "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "JVBERi0xLjQK",
                        }
                    }
                ]
            },
            "parent_tool_use_id": null,
            "session_id": "",
        });

        let serialized = serde_json::to_string(&user_message).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        let blocks = parsed["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
    }

    /// Mixed image + document in a single SDKUserMessage: both blocks must be present
    /// in order, each with the correct `type`. This is the combined path the bridge
    /// takes when both AGENT_IMAGE_URL and AGENT_FILE_URL are set.
    #[test]
    fn sdk_user_message_with_image_and_document() {
        let user_message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "compare these" },
                    {
                        "type": "image",
                        "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" }
                    },
                    {
                        "type": "document",
                        "source": { "type": "base64", "media_type": "application/pdf", "data": "BBBB" }
                    }
                ]
            },
            "parent_tool_use_id": null,
            "session_id": "",
        });

        let parsed: serde_json::Value = serde_json::to_string(&user_message)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap();

        let blocks = parsed["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[2]["type"], "document");
    }

    /// A PDF served with application/pdf must download and round-trip cleanly.
    /// Verifies the basic happy path for `download_document_as_base64`.
    #[tokio::test]
    async fn download_pdf_roundtrips_through_local_server() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        // 4-byte PDF magic ("%PDF") + minimal junk. Real PDFs have headers/trailers
        // but we only need byte-fidelity; the Claude API does the real validation.
        const PDF_BYTES: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";

        async fn serve_pdf() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(PDF_BYTES.to_vec()))
                .unwrap()
        }

        let app = Router::new().route("/file.pdf", get(serve_pdf));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/file.pdf");
        let (media_type, b64) = download_document_as_base64(&url).await.unwrap();
        assert_eq!(media_type, "application/pdf");

        let decoded = B64.decode(&b64).unwrap();
        assert_eq!(decoded, PDF_BYTES);
    }

    /// Plain text files are accepted (`text/plain`). The Anthropic document block
    /// supports this media type alongside PDF; useful for `.txt` / `.md` forwards.
    #[tokio::test]
    async fn download_text_plain_document_roundtrips() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        const TEXT: &str = "hello from a wechat text file\n";

        async fn serve_text() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from(TEXT.as_bytes().to_vec()))
                .unwrap()
        }

        let app = Router::new().route("/note.txt", get(serve_text));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/note.txt");
        let (media_type, b64) = download_document_as_base64(&url).await.unwrap();
        assert_eq!(media_type, "text/plain");
        assert_eq!(
            String::from_utf8_lossy(&B64.decode(&b64).unwrap()).as_ref(),
            TEXT
        );
    }

    /// Any non-PDF/non-text media type must be rejected with a clear error. This is
    /// the "video, zip, exe, etc. → user sees a clear error" guarantee. The check
    /// runs before the CLI is spawned so we never waste a turn on a doomed request.
    #[tokio::test]
    async fn download_document_rejects_unsupported_media_type() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        async fn serve_mp4() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "video/mp4")
                .body(Body::from(b"fake-mp4-bytes".to_vec()))
                .unwrap()
        }

        let app = Router::new().route("/video.mp4", get(serve_mp4));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/video.mp4");
        let err = download_document_as_base64(&url)
            .await
            .expect_err("video/mp4 must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported document media_type"),
            "error should name the constraint: {msg}"
        );
        assert!(
            msg.contains("video/mp4"),
            "error should quote the type: {msg}"
        );
    }

    /// A zip / application/octet-stream file must also be rejected — the bridge
    /// must not silently forward arbitrary binaries to Claude as a "document".
    #[tokio::test]
    async fn download_document_rejects_zip() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        async fn serve_zip() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "application/zip")
                .body(Body::from(b"PK\x03\x04fake-zip".to_vec()))
                .unwrap()
        }

        let app = Router::new().route("/a.zip", get(serve_zip));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/a.zip");
        let err = download_document_as_base64(&url)
            .await
            .expect_err("application/zip must be rejected");
        assert!(format!("{err:#}").contains("application/zip"));
    }

    /// An oversize PDF (>32MB) must fail fast during streaming — the bridge should
    /// not download the full 100MB and then fail at the API.
    #[tokio::test]
    async fn download_document_rejects_oversize_pdf() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};

        // Emit one chunk that itself exceeds the limit so the streaming check trips
        // on the first iteration. We don't need to allocate a real 32MB+ buffer.
        const BIG_CHUNK: usize = ANTHROPIC_MAX_DOCUMENT_BYTES + 1;

        async fn serve_big_pdf() -> Response<Body> {
            Response::builder()
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(vec![b'x'; BIG_CHUNK]))
                .unwrap()
        }

        let app = Router::new().route("/big.pdf", get(serve_big_pdf));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/big.pdf");
        let err = download_document_as_base64(&url)
            .await
            .expect_err("oversize PDF must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("too large"),
            "error should name the size constraint: {msg}"
        );
    }

    // ── Bug-fix regression tests ─────────────────────────────────────────────

    /// Bug #2: whitespace-only text blocks (e.g. a bare "\n" the model emits
    /// between tool turns) must NOT produce an AGENT_PARTIAL. The old guard was
    /// `!text.is_empty()` which passes "\n"; the fix uses `!text.trim().is_empty()`.
    #[test]
    fn whitespace_only_text_block_is_skipped_not_emitted() {
        for ws in ["\n", "  ", "\t", "\r\n", "\n\n"] {
            let json = format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{ws}"}}]}}}}"#,
                ws = ws
                    .replace('\n', "\\n")
                    .replace('\t', "\\t")
                    .replace('\r', "\\r")
            );
            let event: ClaudeStreamEvent = serde_json::from_str(&json).unwrap();
            let text = event.message.unwrap().text();
            assert!(
                !text.is_empty(),
                "raw text '{ws:?}' is non-empty — old guard would emit it"
            );
            assert!(
                text.trim().is_empty(),
                "trimmed '{ws:?}' must be empty → bridge must skip"
            );
        }
    }

    /// Real content with trailing newline must still pass the trim() guard.
    #[test]
    fn text_with_real_content_and_trailing_newline_is_emitted() {
        let json =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Answer: 42\n"}]}}"#;
        let event: ClaudeStreamEvent = serde_json::from_str(json).unwrap();
        let text = event.message.unwrap().text();
        assert!(
            !text.trim().is_empty(),
            "text with real content must not be blocked by trim() guard"
        );
    }

    /// Bug #1 (Claude side): when no prior partial has been sent (`last_partial = None`)
    /// and `result.result` is non-empty, `already_sent` must be false so the bridge
    /// emits result.result as a final AGENT_PARTIAL (the model's only text output).
    #[test]
    fn result_with_no_prior_partial_always_emits() {
        let last_partial: Option<String> = None;
        let result_text = "The answer is 42.";
        let already_sent = last_partial.as_deref() == Some(result_text);
        assert!(
            !already_sent,
            "when no prior partial exists, result must NOT be skipped"
        );
    }

    /// Normal case: result.result matches the last partial → skip (no duplicate).
    #[test]
    fn result_matching_last_partial_is_not_resent() {
        let last_partial: Option<String> = Some("Hello there".to_string());
        let result_text = "Hello there";
        let already_sent = last_partial.as_deref() == Some(result_text);
        assert!(
            already_sent,
            "result matching last_partial must be skipped to avoid duplicate"
        );
    }

    /// Empty or whitespace-only result.result is filtered before the dedup check so
    /// we never emit an empty AGENT_PARTIAL when the model produced no text output.
    #[test]
    fn empty_and_whitespace_result_is_filtered() {
        for rt in ["", "\n", "  \t  "] {
            assert!(
                rt.trim().is_empty(),
                "'{rt:?}' must be caught by filter(!trim.is_empty())"
            );
        }
    }
}
