use std::collections::HashSet;

use serde_json::{json, Value};

use crate::error::{ClientAbortError, ValidationError, WrapError};
use crate::oco::{
    is_rate_limited_upstream, is_retryable_upstream, oco, prompt_with_retry, OcoClient,
};
use crate::translate::{
    apply_stop_and_max_tokens, collect_file_parts, msg_text, parse_chat_options, parse_tool_calls,
    render_history, resolve_model_id, rid, tool_instruction, ChatOptions,
};

#[derive(Debug)]
pub struct PreparedTurn {
    pub model_name: String,
    pub req_body: Value,
    pub known_names: HashSet<String>,
    pub chat_opts: ChatOptions,
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
    let (req_provider, req_id) = if explicit {
        let parts = raw_req.splitn(2, '/').collect::<Vec<_>>();
        let prov = parts[0].trim().to_string();
        let id = resolve_model_id(parts.get(1).unwrap_or(&""));
        (prov, id)
    } else {
        (
            client.config.default_provider.clone(),
            resolve_model_id(&raw_req),
        )
    };

    let ids = client.zen_model_ids().await;
    let model = if ids.contains(&req_id) {
        json!({ "providerID": "opencode", "modelID": req_id })
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
        json!({ "providerID": "opencode", "modelID": req_id })
    } else {
        tracing::info!(
            "[wrap] virtual model \"{}\" -> backend {}/{}",
            raw_req,
            client.config.default_provider,
            client.config.default_model
        );
        json!({
            "providerID": client.config.default_provider,
            "modelID": resolve_model_id(&client.config.default_model)
        })
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

    let mut parts = vec![json!({ "type": "text", "text": prompt_text })];
    for f in collect_file_parts(&messages) {
        parts.push(f);
    }
    let mut req_body = json!({
        "model": model,
        "agent": "build",
        "parts": parts
    });
    if !system_text.is_empty() {
        req_body["system"] = json!(system_text);
    }

    let model_name = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{}/{}",
                model["providerID"].as_str().unwrap_or("opencode"),
                model["modelID"].as_str().unwrap_or("")
            )
        });

    Ok(PreparedTurn {
        model_name,
        req_body,
        known_names,
        chat_opts,
    })
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
            let _ = oco(
                &client,
                &format!("/session/{}", sid),
                "DELETE",
                None,
                Some(15_000),
            )
            .await;
        });
    }
}

fn text_from_parts(resp: &Value) -> String {
    resp.get("parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

async fn assistant_text_from_session(
    client: &OcoClient,
    session_id: &str,
    fallback: &str,
    finish: &str,
) -> Result<String, WrapError> {
    let mut text = fallback.to_string();
    if text.is_empty() || finish == "tool-calls" {
        let all = oco(
            client,
            &format!("/session/{}/message?limit=20", session_id),
            "GET",
            None,
            None,
        )
        .await?;
        let empty: Vec<Value> = Vec::new();
        let messages = all.as_array().unwrap_or(&empty);
        let mut texts = Vec::new();
        for m in messages {
            if m.get("info")
                .and_then(|i| i.get("role"))
                .and_then(|r| r.as_str())
                != Some("assistant")
            {
                continue;
            }
            let parts = m.get("parts").and_then(|p| p.as_array()).unwrap_or(&empty);
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        texts.push(t.to_string());
                    }
                }
            }
        }
        if let Some(last) = texts.last() {
            text = last.clone();
        }
    }
    Ok(text)
}

pub async fn handle_chat_completions(client: &OcoClient, body: &Value) -> Result<Value, WrapError> {
    let prepared = prepare_turn(client, body).await?;
    let req_body = prepared.req_body.clone();
    let known = prepared.known_names.clone();
    let chat_opts = prepared.chat_opts.clone();
    let model_name = prepared.model_name.clone();

    struct AttemptResult {
        resp: Value,
        text: String,
        session_id: String,
    }

    let result = prompt_with_retry(
        || {
            let client = client.clone();
            let req_body = req_body.clone();
            async move {
                let session = oco(&client, "/session", "POST", Some(&json!({})), None).await?;
                let sid = session
                    .get("id")
                    .and_then(|i| i.as_str())
                    .ok_or_else(|| {
                        WrapError::Upstream(crate::error::UpstreamError {
                            upstream_status: 502,
                            upstream_body: "missing session id".into(),
                        })
                    })?
                    .to_string();
                match oco(
                    &client,
                    &format!("/session/{}/message", sid),
                    "POST",
                    Some(&req_body),
                    None,
                )
                .await
                {
                    Ok(r) => {
                        let t = text_from_parts(&r);
                        if t.is_empty() {
                            let _ = oco(
                                &client,
                                &format!("/session/{}", sid),
                                "DELETE",
                                None,
                                Some(15_000),
                            )
                            .await;
                            return Err(WrapError::Upstream(crate::error::UpstreamError {
                                upstream_status: 502,
                                upstream_body: "empty completion".into(),
                            }));
                        }
                        Ok(AttemptResult {
                            resp: r,
                            text: t,
                            session_id: sid,
                        })
                    }
                    Err(e) => {
                        let _ = oco(
                            &client,
                            &format!("/session/{}", sid),
                            "DELETE",
                            None,
                            Some(15_000),
                        )
                        .await;
                        Err(e)
                    }
                }
            }
        },
        3,
    )
    .await?;

    let created_id = result.session_id.clone();
    let _session = SessionGuard::new(client.clone(), created_id.clone());
    let resp = result.resp;
    let mut text = result.text;
    let finish = resp
        .get("info")
        .and_then(|i| i.get("finish"))
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    text = assistant_text_from_session(client, &created_id, &text, finish).await?;

    let known_opt = if known.is_empty() { None } else { Some(&known) };
    let (content, calls) = parse_tool_calls(&text, known_opt);
    let limited = apply_stop_and_max_tokens(
        if content.is_empty() { &text } else { &content },
        &chat_opts,
    );
    let finish_reason = if !calls.is_empty() {
        "tool_calls"
    } else if limited.truncated_by.as_deref() == Some("length") {
        "length"
    } else {
        "stop"
    };

    let id = rid("chatcmpl");
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let input_tok = resp
        .get("info")
        .and_then(|i| i.get("tokens"))
        .and_then(|t| t.get("input"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tok = resp
        .get("info")
        .and_then(|i| i.get("tokens"))
        .and_then(|t| t.get("output"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let usage = json!({
        "prompt_tokens": input_tok,
        "completion_tokens": output_tok,
        "total_tokens": input_tok + output_tok
    });

    if !calls.is_empty() {
        let tool_calls = calls
            .iter()
            .map(|c| {
                json!({
                    "id": rid("call"),
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.args).unwrap_or("{}".into())
                    }
                })
            })
            .collect::<Vec<_>>();
        return Ok(json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model_name,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": if limited.text.is_empty() { Value::Null } else { json!(limited.text) },
                    "tool_calls": tool_calls
                },
                "finish_reason": "tool_calls"
            }],
            "usage": usage
        }));
    }

    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model_name,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": limited.text },
            "finish_reason": finish_reason
        }],
        "usage": usage
    }))
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
            let _ = tx.send("data: [DONE]\n\n".into()).await;
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

async fn run_stream_chat(
    client: &OcoClient,
    prepared: PreparedTurn,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Result<(), WrapError> {
    let req_body = prepared.req_body;
    let known = prepared.known_names;
    let chat_opts = prepared.chat_opts;
    let model_name = prepared.model_name;

    let id = rid("chatcmpl");
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let base = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model_name
    });

    tx.send(sse_chunk(
        &base,
        json!({ "role": "assistant", "content": "" }),
        None,
    ))
    .await
    .map_err(|_| WrapError::ClientAbort(ClientAbortError::default()))?;

    let mut last_err: Option<WrapError> = None;
    for attempt in 1..=3 {
        let session = oco(client, "/session", "POST", Some(&json!({})), None).await?;
        let sid = session
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| {
                WrapError::Upstream(crate::error::UpstreamError {
                    upstream_status: 502,
                    upstream_body: "missing session id".into(),
                })
            })?
            .to_string();

        let streamed = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let streamed_ev = streamed.clone();
        let tx_ev = tx.clone();
        let base_ev = base.clone();
        let event_task = subscribe_events(client, &sid, move |delta| {
            let tx = tx_ev.clone();
            let base = base_ev.clone();
            let streamed = streamed_ev.clone();
            tokio::spawn(async move {
                streamed.lock().await.push_str(&delta);
                let _ = tx
                    .send(sse_chunk(&base, json!({ "content": delta }), None))
                    .await;
            });
        });

        let resp = match oco(
            client,
            &format!("/session/{}/message", sid),
            "POST",
            Some(&req_body),
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = oco(
                    client,
                    &format!("/session/{}", sid),
                    "DELETE",
                    None,
                    Some(15_000),
                )
                .await;
                if let Some(handle) = event_task {
                    handle.abort();
                }
                let streamed_len = streamed.lock().await.len();
                last_err = Some(e.clone());
                if streamed_len > 0 || !is_retryable_upstream(&e) || attempt == 3 {
                    let msg = if let WrapError::Upstream(u) = &e {
                        format!(
                            "Upstream model backend failed: {}",
                            u.upstream_body.chars().take(200).collect::<String>()
                        )
                    } else {
                        format!("{}", e)
                    };
                    tx.send(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&json!({
                            "id": base["id"],
                            "object": base["object"],
                            "created": base["created"],
                            "model": base["model"],
                            "error": { "message": msg }
                        }))
                        .unwrap_or_default()
                    ))
                    .await
                    .ok();
                    tx.send("data: [DONE]\n\n".into()).await.ok();
                    return Ok(());
                }
                let delay = if is_rate_limited_upstream(&e) {
                    4000
                } else {
                    1500 * attempt as u64
                };
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                continue;
            }
        };

        if let Some(handle) = event_task {
            handle.abort();
        }

        let mut text = text_from_parts(&resp);
        let finish = resp
            .get("info")
            .and_then(|i| i.get("finish"))
            .and_then(|f| f.as_str())
            .unwrap_or("stop");
        text = assistant_text_from_session(client, &sid, &text, finish).await?;
        if text.is_empty() {
            let _ = oco(
                client,
                &format!("/session/{}", sid),
                "DELETE",
                None,
                Some(15_000),
            )
            .await;
            last_err = Some(WrapError::Upstream(crate::error::UpstreamError {
                upstream_status: 502,
                upstream_body: "empty completion".into(),
            }));
            if attempt == 3 {
                return Err(last_err.unwrap());
            }
            continue;
        }

        let streamed_text = streamed.lock().await.clone();
        let known_opt = if known.is_empty() { None } else { Some(&known) };
        let (_content, calls) = parse_tool_calls(&text, known_opt);
        let limited_full = apply_stop_and_max_tokens(&text, &chat_opts).text;
        let target = if !calls.is_empty() {
            text.as_str()
        } else {
            limited_full.as_str()
        };
        if target.len() > streamed_text.len() && target.starts_with(&streamed_text) {
            let tail = &target[streamed_text.len()..];
            for i in (0..tail.len()).step_by(500) {
                let piece = tail[i..std::cmp::min(i + 500, tail.len())].to_string();
                tx.send(sse_chunk(&base, json!({ "content": piece }), None))
                    .await
                    .ok();
            }
        }

        if !calls.is_empty() {
            for (i, c) in calls.iter().enumerate() {
                tx.send(sse_chunk(
                    &base,
                    json!({
                        "tool_calls": [{
                            "index": i,
                            "id": rid("call"),
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.args).unwrap_or("{}".into())
                            }
                        }]
                    }),
                    None,
                ))
                .await
                .ok();
            }
            tx.send(sse_chunk(&base, json!({}), Some("tool_calls")))
                .await
                .ok();
        } else {
            let tr = apply_stop_and_max_tokens(&text, &chat_opts);
            let fr = if tr.truncated_by.as_deref() == Some("length") {
                "length"
            } else {
                "stop"
            };
            tx.send(sse_chunk(&base, json!({}), Some(fr))).await.ok();
        }
        tx.send("data: [DONE]\n\n".into()).await.ok();
        let _ = oco(
            client,
            &format!("/session/{}", sid),
            "DELETE",
            None,
            Some(15_000),
        )
        .await;
        return Ok(());
    }
    Err(
        last_err.unwrap_or(WrapError::Upstream(crate::error::UpstreamError {
            upstream_status: 502,
            upstream_body: "stream failed".into(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::oco::OcoClient;
    use serde_json::json;

    fn test_client() -> OcoClient {
        OcoClient::new(Config {
            wrap_port: 8000,
            opencode_port: 4100,
            opencode_base: "http://127.0.0.1:4100".into(),
            default_model: "muse-spark-1.3-contributor-free".into(),
            default_provider: "opencode".into(),
            wrap_cwd: "/tmp".into(),
            oco_timeout_ms: 180_000,
            wrap_max_body_bytes: 8 * 1024 * 1024,
            zen_models_url: "http://127.0.0.1:1/nope".into(),
        })
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
                            { "type": "image_url", "image_url": { "url": "https://x/i.png" } }
                        ]
                    }
                ],
                "response_format": { "type": "json_object" }
            }),
        )
        .await
        .unwrap();
        let sys = prepared.req_body["system"].as_str().unwrap();
        assert!(sys.contains("sys"));
        assert!(sys.contains("dev-rules"));
        assert!(sys.contains("valid JSON"));
        let parts = prepared.req_body["parts"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert!(parts
            .iter()
            .any(|p| p["type"] == "file" && p["url"] == "https://x/i.png"));
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
}

fn subscribe_events(
    client: &OcoClient,
    sid: &str,
    on_delta: impl Fn(String) + Send + Sync + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let base = client.config.opencode_base.clone();
    let sid = sid.to_string();
    let http = client.http.clone();
    let on_delta: std::sync::Arc<dyn Fn(String) + Send + Sync> = std::sync::Arc::new(on_delta);
    Some(tokio::spawn(async move {
        let res = http
            .get(format!("{}/event", base))
            .header("Accept", "text/event-stream")
            .send()
            .await;
        let Ok(mut res) = res else { return };
        if !res.status().is_success() {
            return;
        }
        let mut buf = String::new();
        while let Ok(Some(chunk)) = res.chunk().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buf.find("\n\n") {
                let block = buf[..idx].to_string();
                buf = buf[idx + 2..].to_string();
                for ev in crate::translate::parse_event_block(&block) {
                    if ev.get("type").and_then(|t| t.as_str()) != Some("message.part.delta") {
                        continue;
                    }
                    let p = ev.get("properties").cloned().unwrap_or(Value::Null);
                    if p.get("sessionID").and_then(|s| s.as_str()) != Some(sid.as_str()) {
                        continue;
                    }
                    if p.get("field").and_then(|f| f.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(delta) = p.get("delta").and_then(|d| d.as_str()) {
                        if !delta.is_empty() {
                            on_delta(delta.to_string());
                        }
                    }
                }
            }
        }
    }))
}
