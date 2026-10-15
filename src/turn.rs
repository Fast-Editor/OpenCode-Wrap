//! One OpenAI chat completion == one short-lived OpenCode 2 session:
//! `POST /api/session` (model + agent) -> optional instruction entry for the
//! system prompt -> `POST /api/session/{id}/prompt` -> wait for the run to go
//! idle -> read the projected messages -> `DELETE /api/session/{id}`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::error::{ClientAbortError, OcoTimeoutError, UpstreamError, ValidationError, WrapError};
use crate::oco::{is_retryable_upstream, oco, prompt_with_retry, retry_backoff_ms, OcoClient};
use crate::translate::{
    apply_stop_and_max_tokens, char_chunks, collect_file_parts, guess_mime, msg_text,
    parse_chat_options, parse_tool_calls, render_history, resolve_model_id, rid, tool_instruction,
    ChatOptions, MAX_FILE_URL_BYTES,
};

/// OpenCode 2 has no per-prompt `system` field; the caller's system prompt is
/// attached to the session as an instruction entry under this key.
const SYSTEM_INSTRUCTION_KEY: &str = "opencode-wrap.system";
const SESSION_TITLE: &str = "opencode-wrap";
const MESSAGE_PAGE_LIMIT: &str = "100";
const IDLE_POLL_MS: u64 = 250;
const FILE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub struct PreparedTurn {
    pub model_name: String,
    /// Body for `POST /api/session`; model and agent live on the session in V2.
    pub session_body: Value,
    pub system_text: String,
    pub prompt_text: String,
    /// `PromptInput.FileAttachment` entries (`{uri, name?}`), already inlined
    /// as `data:` URIs because the server rejects remote URLs.
    pub files: Vec<Value>,
    pub known_names: HashSet<String>,
    pub chat_opts: ChatOptions,
}

/// What one completed run produced, independent of transport.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnResult {
    pub text: String,
    pub finish: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

fn upstream(status: u16, body: impl Into<String>) -> WrapError {
    WrapError::Upstream(UpstreamError {
        upstream_status: status,
        upstream_body: body.into(),
    })
}

fn model_ref(provider: &str, id: &str, variant: Option<&str>) -> Value {
    let mut m = json!({ "providerID": provider, "id": id });
    if let Some(v) = variant.filter(|v| !v.is_empty()) {
        m["variant"] = json!(v);
    }
    m
}

/// `model#variant` (OpenCode 2 syntax) -> (`model`, `Some(variant)`).
fn split_variant(id: &str) -> (String, Option<String>) {
    match id.split_once('#') {
        Some((base, variant)) => (
            base.trim().to_string(),
            Some(variant.trim().to_string()).filter(|v| !v.is_empty()),
        ),
        None => (id.trim().to_string(), None),
    }
}

pub async fn prepare_turn(client: &OcoClient, body: &Value) -> Result<PreparedTurn, WrapError> {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let chat_opts = parse_chat_options(body).map_err(WrapError::from)?;

    let raw_req = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            format!(
                "{}/{}",
                client.config.default_provider, client.config.default_model
            )
        });

    let explicit = raw_req.contains('/');
    let (req_provider, req_id, variant) = if explicit {
        let parts = raw_req.splitn(2, '/').collect::<Vec<_>>();
        let prov = parts[0].trim().to_string();
        let (id, variant) = split_variant(parts.get(1).unwrap_or(&""));
        (prov, resolve_model_id(&id), variant)
    } else {
        let (id, variant) = split_variant(&raw_req);
        (
            client.config.default_provider.clone(),
            resolve_model_id(&id),
            variant,
        )
    };

    let ids = client.zen_model_ids().await;
    let model = if ids.contains(&req_id) {
        model_ref("opencode", &req_id, variant.as_deref())
    } else if explicit && req_provider == "opencode" && !ids.is_empty() {
        let needle = req_id.replace("-free", "");
        let sug: Vec<_> = ids
            .iter()
            .filter(|id| id.contains(&needle))
            .take(3)
            .cloned()
            .collect();
        let msg = if sug.is_empty() {
            format!("unknown Zen model \"{}\"", req_id)
        } else {
            format!(
                "unknown Zen model \"{}\", did you mean: {}?",
                req_id,
                sug.join(", ")
            )
        };
        return Err(WrapError::Validation(ValidationError::new(
            msg,
            400,
            "model_not_found",
        )));
    } else if explicit && req_provider == "opencode" && ids.is_empty() {
        model_ref("opencode", &req_id, variant.as_deref())
    } else {
        tracing::info!(
            "[wrap] virtual model \"{}\" -> backend {}/{}",
            raw_req,
            client.config.default_provider,
            client.config.default_model
        );
        model_ref(
            &client.config.default_provider,
            &resolve_model_id(&client.config.default_model),
            None,
        )
    };

    let system = messages
        .iter()
        .filter(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("system")
                || m.get("role").and_then(|r| r.as_str()) == Some("developer")
        })
        .map(msg_text)
        .collect::<Vec<_>>()
        .join("\n\n");

    let tools: Vec<Value> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let tool_choice = body.get("tool_choice");
    let known_names: HashSet<String> = tools
        .iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("function"))
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    let history = if messages.len() > 1 {
        render_history(&messages[..messages.len() - 1])
    } else {
        String::new()
    };
    let last = messages.last();
    let last_user = last.map(msg_text).unwrap_or_default();
    let last_role = last
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("user");
    let turn = if last_role == "tool" || last_role == "assistant" {
        last_user
    } else {
        format!("USER: {}", last_user)
    };

    let instruction = tool_instruction(&tools, tool_choice);
    let format_instruction = match chat_opts.response_format.as_deref() {
        Some("json_object") => {
            "You MUST reply with a single valid JSON object and nothing else (no markdown fences, no prose)."
        }
        Some("json_schema") => {
            "You MUST reply with valid JSON matching the requested schema and nothing else (no markdown fences, no prose)."
        }
        _ => "",
    };
    let prompt_text = [history, turn]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let system_text = [system, instruction, format_instruction.to_string()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let files = to_v2_files(client, &collect_file_parts(&messages)?).await?;

    let model_name = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{}/{}",
                model["providerID"].as_str().unwrap_or("opencode"),
                model["id"].as_str().unwrap_or("")
            )
        });

    let session_body = json!({
        "model": model,
        "agent": client.config.agent,
        "title": SESSION_TITLE
    });

    Ok(PreparedTurn {
        model_name,
        session_body,
        system_text,
        prompt_text,
        files,
        known_names,
        chat_opts,
    })
}

/// Convert the OpenAI-side file parts (`{mime, url}`) into V2
/// `PromptInput.FileAttachment`s. The server only accepts `data:` URIs, so
/// `http(s)` URLs are downloaded (bounded) and inlined here.
async fn to_v2_files(client: &OcoClient, parts: &[Value]) -> Result<Vec<Value>, WrapError> {
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("data:") {
            out.push(json!({ "uri": url }));
        } else if lower.starts_with("http://") || lower.starts_with("https://") {
            out.push(fetch_as_data_uri(client, url).await?);
        } else {
            return Err(WrapError::Validation(ValidationError::new(
                "file/image URLs must be data: URIs or http(s) URLs",
                400,
                "invalid_request_error",
            )));
        }
    }
    Ok(out)
}

fn file_name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    if name.is_empty() || name.contains(':') {
        None
    } else {
        Some(name.to_string())
    }
}

fn bad_file(url: &str, why: impl std::fmt::Display) -> WrapError {
    WrapError::Validation(ValidationError::new(
        format!("could not fetch file url {url}: {why}"),
        400,
        "invalid_request_error",
    ))
}

async fn fetch_as_data_uri(client: &OcoClient, url: &str) -> Result<Value, WrapError> {
    let mut res = client
        .http
        .get(url)
        .timeout(FILE_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| bad_file(url, e))?;
    if !res.status().is_success() {
        return Err(bad_file(url, format!("HTTP {}", res.status())));
    }
    if res.content_length().unwrap_or(0) > MAX_FILE_URL_BYTES as u64 {
        return Err(bad_file(url, format!("exceeds {MAX_FILE_URL_BYTES} bytes")));
    }
    let header_mime = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty() && v != "application/octet-stream");
    let mime = header_mime.unwrap_or_else(|| guess_mime(url));
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| bad_file(url, e))? {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_FILE_URL_BYTES {
            return Err(bad_file(url, format!("exceeds {MAX_FILE_URL_BYTES} bytes")));
        }
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    let mut file = json!({ "uri": format!("data:{mime};base64,{b64}") });
    if let Some(name) = file_name_from_url(url) {
        file["name"] = json!(name);
    }
    Ok(file)
}

pub async fn list_models(client: &OcoClient) -> Value {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let def_id = format!(
        "{}/{}",
        client.config.default_provider,
        resolve_model_id(&client.config.default_model)
    );
    let ids = client.zen_model_ids().await;
    if !ids.is_empty() {
        let mut sorted: Vec<_> = ids.iter().cloned().collect();
        sorted.sort();
        let mut data = vec![json!({
            "id": def_id,
            "object": "model",
            "created": created,
            "owned_by": "opencode-wrap"
        })];
        let mut seen = HashSet::from([def_id.clone()]);
        for id in sorted {
            let full = format!("opencode/{}", id);
            if seen.contains(&full) {
                continue;
            }
            seen.insert(full.clone());
            data.push(json!({
                "id": full,
                "object": "model",
                "created": created,
                "owned_by": "opencode"
            }));
        }
        return json!({ "object": "list", "data": data });
    }
    json!({
        "object": "list",
        "data": [{
            "id": def_id,
            "object": "model",
            "created": created,
            "owned_by": "opencode-wrap"
        }]
    })
}

/// Deletes the backend session even when the handler future is dropped
/// mid-flight (axum cancels on client disconnect), so an aborted caller
/// doesn't leave the session running upstream.
struct SessionGuard {
    client: OcoClient,
    sid: Option<String>,
}

impl SessionGuard {
    fn new(client: OcoClient, sid: String) -> Self {
        Self {
            client,
            sid: Some(sid),
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let Some(sid) = self.sid.take() else { return };
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = delete_session(&client, &sid).await {
                tracing::warn!("[wrap] failed to delete session {sid}: {e}");
            }
        });
    }
}

async fn create_session(client: &OcoClient, session_body: &Value) -> Result<String, WrapError> {
    let res = oco(client, "/api/session", "POST", Some(session_body), None).await?;
    res.get("data")
        .and_then(|d| d.get("id"))
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| upstream(502, "missing session id"))
}

async fn delete_session(client: &OcoClient, sid: &str) -> Result<Value, WrapError> {
    oco(
        client,
        &format!("/api/session/{sid}"),
        "DELETE",
        None,
        Some(15_000),
    )
    .await
}

async fn set_system_instructions(
    client: &OcoClient,
    sid: &str,
    text: &str,
) -> Result<(), WrapError> {
    oco(
        client,
        &format!("/api/experimental/session/{sid}/instructions/entries/{SYSTEM_INSTRUCTION_KEY}"),
        "PUT",
        Some(&json!({ "value": text })),
        None,
    )
    .await
    .map(|_| ())
}

fn fold_system_into_prompt(system: &str, prompt: &str) -> String {
    format!("SYSTEM INSTRUCTIONS (apply to the whole conversation):\n{system}\n\n{prompt}")
}

async fn send_prompt(
    client: &OcoClient,
    sid: &str,
    text: &str,
    files: &[Value],
) -> Result<Value, WrapError> {
    let mut body = json!({ "text": text });
    if !files.is_empty() {
        body["files"] = Value::Array(files.to_vec());
    }
    oco(
        client,
        &format!("/api/session/{sid}/prompt"),
        "POST",
        Some(&body),
        None,
    )
    .await
}

fn created_at(m: &Value) -> f64 {
    m.get("time")
        .and_then(|t| t.get("created"))
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0)
}

fn sort_newest_first(msgs: &mut [Value]) {
    msgs.sort_by(|a, b| {
        created_at(b)
            .partial_cmp(&created_at(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

async fn list_messages(client: &OcoClient, sid: &str) -> Result<Vec<Value>, WrapError> {
    let res = oco(
        client,
        &format!("/api/session/{sid}/message?order=desc&limit={MESSAGE_PAGE_LIMIT}"),
        "GET",
        None,
        None,
    )
    .await?;
    let mut msgs = res
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    sort_newest_first(&mut msgs);
    Ok(msgs)
}

fn msg_type(m: &Value) -> Option<&str> {
    m.get("type").and_then(|t| t.as_str())
}

/// Block until the session's run has finished. The wait endpoint returns
/// once the session is idle; the message list is the source of truth (a
/// fresh session has no `idle` message until its first run completes).
async fn wait_for_idle(client: &OcoClient, sid: &str) -> Result<Vec<Value>, WrapError> {
    let total = client.config.oco_timeout_ms;
    let deadline = Instant::now() + Duration::from_millis(total);
    let wait_path = format!("/api/experimental/session/{sid}/wait");
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WrapError::Timeout(OcoTimeoutError::new(
                "WAIT", &wait_path, total,
            )));
        }
        let remaining_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        match oco(client, &wait_path, "POST", None, Some(remaining_ms)).await {
            Ok(_) => {}
            Err(WrapError::Timeout(t)) => return Err(WrapError::Timeout(t)),
            Err(e) => tracing::debug!("[wrap] wait endpoint unavailable ({e}); polling"),
        }
        let msgs = list_messages(client, sid).await?;
        if msgs.iter().any(|m| msg_type(m) == Some("idle")) {
            return Ok(msgs);
        }
        sleep(Duration::from_millis(IDLE_POLL_MS)).await;
    }
}

fn structured_error(err: Option<&Value>, outcome: &str) -> WrapError {
    let err = err.filter(|e| e.is_object());
    let status = err
        .and_then(|e| e.get("status"))
        .and_then(|s| s.as_u64())
        .and_then(|s| u16::try_from(s).ok())
        .unwrap_or(502);
    let body = match err {
        Some(e) => format!(
            "run {outcome}: {} {} {}",
            e.get("type").and_then(|t| t.as_str()).unwrap_or("error"),
            e.get("message").and_then(|m| m.as_str()).unwrap_or(""),
            e.get("response")
                .and_then(|r| r.get("body"))
                .and_then(|b| b.as_str())
                .unwrap_or("")
        )
        .trim()
        .to_string(),
        None => format!("run {outcome}"),
    };
    upstream(status, body.chars().take(2000).collect::<String>())
}

fn assistant_text(m: &Value) -> String {
    m.get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|c| msg_type(c) == Some("text"))
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn token_count(m: &Value, field: &str) -> u64 {
    m.get("tokens")
        .and_then(|t| t.get(field))
        .and_then(|v| v.as_f64())
        .map(|v| v.max(0.0) as u64)
        .unwrap_or(0)
}

/// Project the session's messages (newest first) into one completion.
pub(crate) fn turn_result_from_messages(messages: &[Value]) -> Result<TurnResult, WrapError> {
    let idle = messages
        .iter()
        .find(|m| msg_type(m) == Some("idle"))
        .ok_or_else(|| upstream(502, "session never went idle"))?;
    match idle.get("outcome").and_then(|o| o.as_str()) {
        Some("succeeded") | None => {}
        Some(other) => return Err(structured_error(idle.get("error"), other)),
    }
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| msg_type(m) == Some("assistant"))
        .collect();
    let text = assistants
        .iter()
        .map(|m| assistant_text(m))
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default();
    if text.is_empty() {
        if let Some(err) = assistants
            .iter()
            .find_map(|m| m.get("error").filter(|e| e.is_object()))
        {
            return Err(structured_error(Some(err), "failed"));
        }
    }
    let finish = assistants
        .first()
        .and_then(|m| m.get("finish"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
        .to_string();
    let prompt_tokens = assistants.iter().map(|m| token_count(m, "input")).sum();
    let completion_tokens = assistants
        .iter()
        .map(|m| token_count(m, "output") + token_count(m, "reasoning"))
        .sum();
    Ok(TurnResult {
        text,
        finish,
        prompt_tokens,
        completion_tokens,
    })
}

/// Run one prompt in an already-created session and return its projection.
async fn run_turn(
    client: &OcoClient,
    sid: &str,
    prepared: &PreparedTurn,
) -> Result<TurnResult, WrapError> {
    let mut text = prepared.prompt_text.clone();
    if !prepared.system_text.is_empty() {
        if let Err(e) = set_system_instructions(client, sid, &prepared.system_text).await {
            tracing::warn!(
                "[wrap] instruction entry rejected ({e}); folding system text into the prompt"
            );
            text = fold_system_into_prompt(&prepared.system_text, &text);
        }
    }
    send_prompt(client, sid, &text, &prepared.files).await?;
    let messages = wait_for_idle(client, sid).await?;
    turn_result_from_messages(&messages)
}

struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

fn sse_done() -> &'static str {
    "data: [DONE]\n\n"
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn usage_json(r: &TurnResult) -> Value {
    json!({
        "prompt_tokens": r.prompt_tokens,
        "completion_tokens": r.completion_tokens,
        "total_tokens": r.prompt_tokens + r.completion_tokens
    })
}

fn tool_call_json(name: &str, args: &Value) -> Value {
    json!({
        "id": rid("call"),
        "type": "function",
        "function": {
            "name": name,
            "arguments": serde_json::to_string(args).unwrap_or("{}".into())
        }
    })
}

fn completion_response(prepared: &PreparedTurn, r: &TurnResult) -> Value {
    let known_opt = if prepared.known_names.is_empty() {
        None
    } else {
        Some(&prepared.known_names)
    };
    let (content, calls) = parse_tool_calls(&r.text, known_opt);
    let visible = if calls.is_empty() { &r.text } else { &content };
    let limited = apply_stop_and_max_tokens(visible, &prepared.chat_opts);
    let id = rid("chatcmpl");
    let created = now_secs();

    if !calls.is_empty() {
        let tool_calls = calls
            .iter()
            .map(|c| tool_call_json(&c.name, &c.args))
            .collect::<Vec<_>>();
        return json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": prepared.model_name,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": if limited.text.is_empty() { Value::Null } else { json!(limited.text) },
                    "tool_calls": tool_calls
                },
                "finish_reason": "tool_calls"
            }],
            "usage": usage_json(r)
        });
    }

    let finish_reason = if limited.truncated_by.as_deref() == Some("length") || r.finish == "length"
    {
        "length"
    } else {
        "stop"
    };
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": prepared.model_name,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": limited.text },
            "finish_reason": finish_reason
        }],
        "usage": usage_json(r)
    })
}

pub async fn handle_chat_completions(client: &OcoClient, body: &Value) -> Result<Value, WrapError> {
    let prepared = prepare_turn(client, body).await?;
    let result = prompt_with_retry(
        || {
            let client = client.clone();
            let prepared = &prepared;
            async move {
                let sid = create_session(&client, &prepared.session_body).await?;
                let _session = SessionGuard::new(client.clone(), sid.clone());
                let r = run_turn(&client, &sid, prepared).await?;
                if r.text.is_empty() {
                    return Err(upstream(502, "empty completion"));
                }
                Ok(r)
            }
        },
        MAX_ATTEMPTS,
    )
    .await?;
    Ok(completion_response(&prepared, &result))
}

pub async fn stream_chat_completions(
    client: &OcoClient,
    body: &Value,
) -> Result<tokio::sync::mpsc::Receiver<String>, WrapError> {
    let prepared = prepare_turn(client, body).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(e) = run_stream_chat(&client, prepared, tx.clone()).await {
            let msg = format!("{}", e);
            let _ = tx
                .send(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&json!({ "error": { "message": msg } }))
                        .unwrap_or_default()
                ))
                .await;
            let _ = tx.send(sse_done().to_string()).await;
        }
    });
    Ok(rx)
}

fn sse_chunk(base: &Value, delta: Value, finish: Option<&str>) -> String {
    let choice = json!({
        "index": 0,
        "delta": delta,
        "finish_reason": finish
    });
    format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "id": base["id"],
            "object": base["object"],
            "created": base["created"],
            "model": base["model"],
            "choices": [choice]
        }))
        .unwrap_or_default()
    )
}

fn sse_error(base: &Value, msg: String) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "id": base["id"],
            "object": base["object"],
            "created": base["created"],
            "model": base["model"],
            "error": { "message": msg }
        }))
        .unwrap_or_default()
    )
}

async fn run_stream_chat(
    client: &OcoClient,
    prepared: PreparedTurn,
    tx: Sender<String>,
) -> Result<(), WrapError> {
    let base = json!({
        "id": rid("chatcmpl"),
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": prepared.model_name
    });

    tx.send(sse_chunk(
        &base,
        json!({ "role": "assistant", "content": "" }),
        None,
    ))
    .await
    .map_err(|_| WrapError::ClientAbort(ClientAbortError::default()))?;

    let mut last_err: Option<WrapError> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        let sid = match create_session(client, &prepared.session_body).await {
            Ok(sid) => sid,
            Err(e) => {
                last_err = Some(e.clone());
                if !is_retryable_upstream(&e) || attempt == MAX_ATTEMPTS {
                    return Err(e);
                }
                sleep(Duration::from_millis(retry_backoff_ms(&e, attempt))).await;
                continue;
            }
        };
        let session = SessionGuard::new(client.clone(), sid.clone());

        let streamed = Arc::new(Mutex::new(String::new()));
        let streamed_ev = streamed.clone();
        let tx_ev = tx.clone();
        let base_ev = base.clone();
        let events = AbortOnDrop(
            subscribe_text_deltas(client, &sid, &tx, move |delta| {
                let tx = tx_ev.clone();
                let base = base_ev.clone();
                let streamed = streamed_ev.clone();
                tokio::spawn(async move {
                    if tx.is_closed() {
                        return;
                    }
                    streamed.lock().await.push_str(&delta);
                    let _ = tx
                        .send(sse_chunk(&base, json!({ "content": delta }), None))
                        .await;
                });
            })
            .await,
        );

        let outcome = tokio::select! {
            res = run_turn(client, &sid, &prepared) => res,
            _ = tx.closed() => {
                return Err(WrapError::ClientAbort(ClientAbortError::default()));
            }
        };
        drop(events);

        let result = match outcome {
            Ok(r) if r.text.is_empty() => Err(upstream(502, "empty completion")),
            other => other,
        };
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                drop(session);
                let streamed_len = streamed.lock().await.len();
                last_err = Some(e.clone());
                if streamed_len > 0 || !is_retryable_upstream(&e) || attempt == MAX_ATTEMPTS {
                    let msg = if let WrapError::Upstream(u) = &e {
                        format!(
                            "Upstream model backend failed: {}",
                            u.upstream_body.chars().take(200).collect::<String>()
                        )
                    } else {
                        format!("{e}")
                    };
                    tx.send(sse_error(&base, msg)).await.ok();
                    tx.send(sse_done().to_string()).await.ok();
                    return Ok(());
                }
                sleep(Duration::from_millis(retry_backoff_ms(&e, attempt))).await;
                continue;
            }
        };

        let text = result.text.as_str();
        let streamed_text = streamed.lock().await.clone();
        let known_opt = if prepared.known_names.is_empty() {
            None
        } else {
            Some(&prepared.known_names)
        };
        let (_content, calls) = parse_tool_calls(text, known_opt);
        let limited = apply_stop_and_max_tokens(text, &prepared.chat_opts);
        let target = if !calls.is_empty() {
            text
        } else {
            limited.text.as_str()
        };
        if target.len() > streamed_text.len() && target.starts_with(&streamed_text) {
            let tail = &target[streamed_text.len()..];
            for piece in char_chunks(tail, 500) {
                tx.send(sse_chunk(&base, json!({ "content": piece }), None))
                    .await
                    .ok();
            }
        }

        if !calls.is_empty() {
            for (i, c) in calls.iter().enumerate() {
                let mut call = tool_call_json(&c.name, &c.args);
                call["index"] = json!(i);
                tx.send(sse_chunk(&base, json!({ "tool_calls": [call] }), None))
                    .await
                    .ok();
            }
            tx.send(sse_chunk(&base, json!({}), Some("tool_calls")))
                .await
                .ok();
        } else {
            let fr =
                if limited.truncated_by.as_deref() == Some("length") || result.finish == "length" {
                    "length"
                } else {
                    "stop"
                };
            tx.send(sse_chunk(&base, json!({}), Some(fr))).await.ok();
        }
        tx.send(sse_done().to_string()).await.ok();
        drop(session);
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| upstream(502, "stream failed")))
}

/// `session.text.delta` payload for `sid`, if `ev` is one.
pub(crate) fn text_delta_for_session(ev: &Value, sid: &str) -> Option<String> {
    if msg_type(ev)? != "session.text.delta" {
        return None;
    }
    let data = ev.get("data")?;
    if data.get("sessionID")?.as_str()? != sid {
        return None;
    }
    let delta = data.get("delta")?.as_str()?;
    if delta.is_empty() {
        None
    } else {
        Some(delta.to_string())
    }
}

/// Subscribe to the global V2 event bus and forward this session's text
/// deltas to `on_delta`. The connection is established before returning so
/// no delta emitted right after the prompt is missed.
async fn subscribe_text_deltas(
    client: &OcoClient,
    sid: &str,
    tx: &Sender<String>,
    on_delta: impl Fn(String) + Send + Sync + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let timeout = Duration::from_millis(client.config.oco_timeout_ms);
    let req = client
        .authorize(
            client
                .http
                .get(format!("{}/api/event", client.config.opencode_base)),
        )
        .header("Accept", "text/event-stream")
        .timeout(timeout);
    let mut res = match req.send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!(
                "[wrap] event stream unavailable (status {}); streaming final text only",
                r.status()
            );
            return None;
        }
        Err(e) => {
            tracing::warn!("[wrap] event stream unavailable ({e}); streaming final text only");
            return None;
        }
    };
    let deadline = Instant::now() + timeout;
    let sid = sid.to_string();
    let tx = tx.clone();
    let on_delta: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_delta);
    Some(tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if tx.is_closed() || Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll = remaining.min(Duration::from_secs(1));
            let chunk = match tokio::time::timeout(poll, res.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) | Ok(Err(_)) => break,
                Err(_) => continue,
            };
            buf.extend_from_slice(&chunk);
            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                let block = String::from_utf8_lossy(&buf[..idx]).into_owned();
                buf.drain(..idx + 2);
                if tx.is_closed() {
                    return;
                }
                for ev in crate::translate::parse_event_block(&block) {
                    if let Some(delta) = text_delta_for_session(&ev, &sid) {
                        on_delta(delta);
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::oco::OcoClient;
    use serde_json::json;

    fn test_client() -> OcoClient {
        OcoClient::new(Config::for_tests())
    }

    #[tokio::test]
    async fn prepare_turn_merges_developer_and_files() {
        let client = test_client();
        let prepared = prepare_turn(
            &client,
            &json!({
                "model": "wrap/test-model",
                "messages": [
                    { "role": "system", "content": "sys" },
                    { "role": "developer", "content": "dev-rules" },
                    {
                        "role": "user",
                        "content": [
                            { "type": "text", "text": "describe" },
                            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                        ]
                    }
                ],
                "response_format": { "type": "json_object" }
            }),
        )
        .await
        .unwrap();
        assert!(prepared.system_text.contains("sys"));
        assert!(prepared.system_text.contains("dev-rules"));
        assert!(prepared.system_text.contains("valid JSON"));
        assert_eq!(prepared.prompt_text, "USER: describe");
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.files[0]["uri"], "data:image/png;base64,AAAA");
        // "wrap/..." is a virtual model: the session runs on the default backend.
        assert_eq!(prepared.session_body["model"]["providerID"], "opencode");
        assert_eq!(
            prepared.session_body["model"]["id"],
            "muse-spark-1.3-contributor-free"
        );
        assert_eq!(prepared.session_body["agent"], "build");
        assert_eq!(prepared.model_name, "wrap/test-model");
    }

    #[tokio::test]
    async fn prepare_turn_passes_explicit_model_and_variant() {
        let client = test_client();
        let prepared = prepare_turn(
            &client,
            &json!({
                "model": "opencode/muse-spark-1.3-contributor#high",
                "messages": [{ "role": "user", "content": "hi" }]
            }),
        )
        .await
        .unwrap();
        let model = &prepared.session_body["model"];
        assert_eq!(model["providerID"], "opencode");
        assert_eq!(model["id"], "muse-spark-1.3-contributor-free");
        assert_eq!(model["variant"], "high");
        assert!(prepared.system_text.is_empty());
    }

    #[tokio::test]
    async fn prepare_turn_rejects_bad_options() {
        let client = test_client();
        let err = prepare_turn(
            &client,
            &json!({
                "model": "wrap/t",
                "messages": [{ "role": "user", "content": "hi" }],
                "n": 4
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WrapError::Validation(_)));
        let err2 = prepare_turn(&client, &json!({ "model": "wrap/t", "messages": [] }))
            .await
            .unwrap_err();
        assert!(matches!(err2, WrapError::Validation(_)));
    }

    #[tokio::test]
    async fn prepare_turn_rejects_unsupported_file_scheme() {
        let client = test_client();
        let err = prepare_turn(
            &client,
            &json!({
                "model": "wrap/t",
                "messages": [{
                    "role": "user",
                    "content": [{ "type": "image_url", "image_url": { "url": "ftp://x/i.png" } }]
                }]
            }),
        )
        .await
        .unwrap_err();
        match err {
            WrapError::Validation(v) => assert_eq!(v.http_status, 400),
            other => panic!("expected validation error, got {other}"),
        }
    }

    fn assistant(created: u64, finish: &str, text: &str, input: u64, output: u64) -> Value {
        json!({
            "id": format!("msg_{created}"),
            "type": "assistant",
            "time": { "created": created },
            "agent": "build",
            "model": { "providerID": "opencode", "id": "m" },
            "finish": finish,
            "tokens": { "input": input, "output": output, "reasoning": 2, "cache": { "read": 0, "write": 0 } },
            "content": [
                { "type": "reasoning", "text": "" },
                { "type": "text", "text": text }
            ]
        })
    }

    #[test]
    fn turn_result_projects_latest_text_and_sums_tokens() {
        let mut msgs = vec![
            json!({ "id": "u", "type": "user", "time": { "created": 1 }, "text": "q" }),
            assistant(2, "tool-calls", "Let me check.", 100, 5),
            assistant(3, "stop", "pong", 120, 7),
            json!({ "id": "i", "type": "idle", "time": { "created": 4 }, "outcome": "succeeded" }),
        ];
        sort_newest_first(&mut msgs);
        let r = turn_result_from_messages(&msgs).unwrap();
        assert_eq!(r.text, "pong");
        assert_eq!(r.finish, "stop");
        assert_eq!(r.prompt_tokens, 220);
        assert_eq!(r.completion_tokens, 5 + 2 + 7 + 2);
    }

    #[test]
    fn turn_result_requires_idle_and_surfaces_failures() {
        let no_idle = [assistant(1, "stop", "x", 1, 1)];
        assert!(matches!(
            turn_result_from_messages(&no_idle),
            Err(WrapError::Upstream(_))
        ));
        let failed = [
            json!({ "type": "idle", "time": { "created": 2 }, "outcome": "failed",
                    "error": { "type": "ProviderError", "message": "rate limit exceeded", "status": 429 } }),
            assistant(1, "error", "", 1, 0),
        ];
        match turn_result_from_messages(&failed).unwrap_err() {
            WrapError::Upstream(u) => {
                assert_eq!(u.upstream_status, 429);
                assert!(u.upstream_body.contains("rate limit"));
            }
            other => panic!("expected upstream error, got {other}"),
        }
    }

    #[test]
    fn text_delta_filter_matches_v2_event_shape() {
        let ev = json!({
            "id": "evt_1", "type": "session.text.delta",
            "data": { "sessionID": "ses_a", "assistantMessageID": "msg_1", "ordinal": 0, "delta": "pong" }
        });
        assert_eq!(
            text_delta_for_session(&ev, "ses_a").as_deref(),
            Some("pong")
        );
        assert_eq!(text_delta_for_session(&ev, "ses_b"), None);
        let other = json!({ "type": "session.text.ended", "data": { "sessionID": "ses_a", "text": "pong" } });
        assert_eq!(text_delta_for_session(&other, "ses_a"), None);
    }

    #[test]
    fn variant_and_file_name_helpers() {
        assert_eq!(split_variant("m#high"), ("m".into(), Some("high".into())));
        assert_eq!(split_variant("m#"), ("m".into(), None));
        assert_eq!(split_variant("m"), ("m".into(), None));
        assert_eq!(
            file_name_from_url("https://x.example/a/b.png?x=1#f"),
            Some("b.png".into())
        );
        assert_eq!(file_name_from_url("https://x.example/"), None);
    }

    #[test]
    fn sse_done_frame() {
        assert_eq!(sse_done(), "data: [DONE]\n\n");
    }
}
