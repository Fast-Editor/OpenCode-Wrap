use regex::Regex;
use serde_json::{json, Value};

use crate::error::ValidationError;

pub fn resolve_model_id(raw: &str) -> String {
    let id = raw.trim();
    let re = Regex::new(r"^(muse-spark-.+-contributor)$").unwrap();
    if re.is_match(id) && !id.ends_with("-free") {
        tracing::info!(
            "[wrap] aliasing unknown model \"{}\" -> \"{}-free\"",
            id,
            id
        );
        return format!("{}-free", id);
    }
    id.to_string()
}

pub fn part_text(p: &Value) -> String {
    if let Some(s) = p.as_str() {
        return s.to_string();
    }
    if !p.is_object() {
        return String::new();
    }
    let t = p.get("type").and_then(|v| v.as_str());
    if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
        if t.is_none() || t == Some("text") || t == Some("input_text") {
            return text.to_string();
        }
    }
    if let Some(s) = p.get("input_text").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if t == Some("text") {
        if let Some(c) = p.get("content").and_then(|v| v.as_str()) {
            return c.to_string();
        }
    }
    String::new()
}

pub fn msg_text(m: &Value) -> String {
    let c = m.get("content");
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().map(part_text).collect(),
        _ => String::new(),
    }
}

pub fn guess_mime(url: &str) -> String {
    let u = url;
    if let Some(m) = Regex::new(r"^data:([^;,]+)?(;base64)?,")
        .unwrap()
        .captures(u)
    {
        if let Some(mime) = m.get(1) {
            return mime.as_str().to_lowercase();
        }
    }
    let clean = u
        .split('?')
        .next()
        .unwrap_or(u)
        .split('#')
        .next()
        .unwrap_or(u)
        .to_lowercase();
    if clean.ends_with(".png") {
        return "image/png".into();
    }
    if clean.ends_with(".jpg") || clean.ends_with(".jpeg") {
        return "image/jpeg".into();
    }
    if clean.ends_with(".webp") {
        return "image/webp".into();
    }
    if clean.ends_with(".gif") {
        return "image/gif".into();
    }
    if clean.ends_with(".pdf") {
        return "application/pdf".into();
    }
    if clean.ends_with(".mp4") {
        return "video/mp4".into();
    }
    "image/png".into()
}

pub fn collect_file_parts(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        let c = m.get("content");
        if !c.map(|v| v.is_array()).unwrap_or(false) {
            continue;
        }
        for p in c.unwrap().as_array().unwrap() {
            if !p.is_object() {
                continue;
            }
            let url = file_part_url(p);
            if let Some(url) = url {
                out.push(json!({
                    "type": "file",
                    "mime": guess_mime(&url),
                    "url": url
                }));
            }
        }
    }
    out
}

fn file_part_url(p: &Value) -> Option<String> {
    let t = p.get("type").and_then(|v| v.as_str());
    match t {
        Some("image_url") => p
            .get("image_url")
            .and_then(|iu| iu.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some("image") => p
            .get("url")
            .or_else(|| p.get("image_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some("file") => p
            .get("url")
            .or_else(|| p.get("file").and_then(|f| f.get("url")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some("input_image") => p
            .get("image_url")
            .or_else(|| p.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn image_placeholder_count(m: &Value) -> usize {
    let c = m.get("content").and_then(|v| v.as_array());
    if c.is_none() {
        return 0;
    }
    c.unwrap()
        .iter()
        .filter(|p| {
            p.is_object()
                && matches!(
                    p.get("type").and_then(|t| t.as_str()),
                    Some("image_url") | Some("image") | Some("input_image") | Some("file")
                )
        })
        .count()
}

pub fn render_history(messages: &[Value]) -> String {
    let mut out = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "system" || role == "developer" {
            continue;
        }
        let imgs = image_placeholder_count(m);
        let suffix = if imgs > 0 {
            format!(
                "\n[attached {} image/file part(s) — see message attachments]",
                imgs
            )
        } else {
            String::new()
        };
        if role == "tool" {
            let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let id = m
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            out.push(format!(
                "TOOL RESULT (name={} id={}):\n{}{}",
                name,
                id,
                msg_text(m),
                suffix
            ));
        } else if role == "assistant" && m.get("tool_calls").is_some() {
            let text = msg_text(m);
            if !text.is_empty() {
                out.push(format!("ASSISTANT: {}{}", text, suffix));
            } else if !suffix.is_empty() {
                out.push(format!("ASSISTANT: {}", suffix.trim()));
            }
            let empty: Vec<Value> = Vec::new();
            let calls = m.get("tool_calls").unwrap();
            let mapped: Vec<Value> = calls
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .map(|t| {
                    json!({
                        "id": t.get("id"),
                        "name": t.get("function").and_then(|f| f.get("name")),
                        "arguments": t.get("function").and_then(|f| f.get("arguments"))
                    })
                })
                .collect();
            out.push(format!(
                "ASSISTANT TOOL CALLS: {}",
                serde_json::to_string(&mapped).unwrap_or_default()
            ));
        } else {
            let r = role.to_uppercase();
            let r = if r.is_empty() { "USER".into() } else { r };
            out.push(format!("{}: {}{}", r, msg_text(m), suffix));
        }
    }
    out.join("\n\n")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatOptions {
    pub max_tokens: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub response_format: Option<String>,
}

pub fn parse_chat_options(body: &Value) -> Result<ChatOptions, ValidationError> {
    let messages = body.get("messages");
    if !messages.map(|m| m.is_array()).unwrap_or(false) {
        return Err(ValidationError::new(
            "messages must be a non-empty array",
            400,
            "invalid_request_error",
        ));
    }
    if messages.unwrap().as_array().unwrap().is_empty() {
        return Err(ValidationError::new(
            "messages must be a non-empty array",
            400,
            "invalid_request_error",
        ));
    }
    if let Some(n) = body.get("n") {
        if !n.is_null() && n.as_u64() != Some(1) {
            return Err(ValidationError::new(
                "only n=1 is supported",
                400,
                "invalid_request_error",
            ));
        }
    }
    check_num(body.get("temperature"), "temperature", 0.0, 2.0)?;
    check_num(body.get("top_p"), "top_p", 0.0, 1.0)?;
    check_num(body.get("presence_penalty"), "presence_penalty", -2.0, 2.0)?;
    check_num(
        body.get("frequency_penalty"),
        "frequency_penalty",
        -2.0,
        2.0,
    )?;

    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"));
    let max_tokens = match max_tokens {
        None | Some(Value::Null) => None,
        Some(v) => {
            let n = v.as_u64();
            if n.is_none() || n == Some(0) {
                return Err(ValidationError::new(
                    "max_tokens must be a positive integer",
                    400,
                    "invalid_request_error",
                ));
            }
            n
        }
    };

    let stop = match body.get("stop").cloned() {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.is_empty() {
                None
            } else {
                Some(vec![s])
            }
        }
        Some(Value::Array(arr)) => {
            if arr.iter().any(|s| !s.is_string()) {
                return Err(ValidationError::new(
                    "stop must be a string or array of strings",
                    400,
                    "invalid_request_error",
                ));
            }
            let v: Vec<String> = arr
                .iter()
                .filter_map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .take(4)
                .map(|s| s.to_string())
                .collect();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        _ => {
            return Err(ValidationError::new(
                "stop must be a string or array of strings",
                400,
                "invalid_request_error",
            ));
        }
    };

    let response_format = match body.get("response_format") {
        None | Some(Value::Null) => None,
        Some(rf) => {
            let t = rf.get("type").and_then(|v| v.as_str());
            if t.is_none() {
                return Err(ValidationError::new(
                    "response_format.type must be a string",
                    400,
                    "invalid_request_error",
                ));
            }
            match t.unwrap() {
                "text" => None,
                "json_object" | "json_schema" => Some(t.unwrap().to_string()),
                other => {
                    return Err(ValidationError::new(
                        format!("unsupported response_format.type \"{}\"", other),
                        400,
                        "invalid_request_error",
                    ));
                }
            }
        }
    };

    if let Some(tc) = body.get("tool_choice") {
        if !tc.is_null() {
            validate_tool_choice(tc)?;
        }
    }

    Ok(ChatOptions {
        max_tokens,
        stop,
        response_format,
    })
}

fn check_num(v: Option<&Value>, name: &str, min: f64, max: f64) -> Result<(), ValidationError> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(val) => {
            let n = val.as_f64();
            if n.is_none() || !n.unwrap().is_finite() || n.unwrap() < min || n.unwrap() > max {
                return Err(ValidationError::new(
                    format!("{} must be a number {}..{}", name, min, max),
                    400,
                    "invalid_request_error",
                ));
            }
            Ok(())
        }
    }
}

fn validate_tool_choice(tc: &Value) -> Result<(), ValidationError> {
    if let Some(s) = tc.as_str() {
        if ["none", "auto", "required"].contains(&s) {
            return Ok(());
        }
    }
    if tc.is_object() {
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        if name.is_some() {
            return Ok(());
        }
    }
    Err(ValidationError::new(
        "tool_choice must be \"none\", \"auto\", \"required\", or {function:{name}}",
        400,
        "invalid_request_error",
    ))
}

#[derive(Debug, PartialEq)]
pub struct TruncateResult {
    pub text: String,
    pub truncated_by: Option<String>,
}

pub fn apply_stop_and_max_tokens(text: &str, opts: &ChatOptions) -> TruncateResult {
    let mut out = text.to_string();
    let mut truncated_by = None;
    if let Some(stop) = &opts.stop {
        let mut cut: Option<usize> = None;
        for s in stop {
            if let Some(i) = out.find(s) {
                cut = Some(cut.map(|c| c.min(i)).unwrap_or(i));
            }
        }
        if let Some(i) = cut {
            out = out[..i].to_string();
            truncated_by = Some("stop".into());
        }
    }
    if let Some(max) = opts.max_tokens {
        if max > 0 {
            let approx = (max * 4) as usize;
            if out.len() > approx {
                out = out[..approx].to_string();
                truncated_by = Some("length".into());
            }
        }
    }
    TruncateResult {
        text: out,
        truncated_by,
    }
}

pub fn tool_instruction(tools: &[Value], tool_choice: Option<&Value>) -> String {
    if tools.is_empty() {
        return String::new();
    }
    if tool_choice.and_then(|v| v.as_str()) == Some("none") {
        return String::new();
    }
    let defs: Vec<Value> = tools
        .iter()
        .map(|t| {
            if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                let f = t.get("function").unwrap_or(t);
                json!({
                    "name": f.get("name"),
                    "description": f.get("description").unwrap_or(&Value::String("".into())),
                    "parameters": f.get("parameters").cloned().unwrap_or(json!({}))
                })
            } else {
                t.clone()
            }
        })
        .collect();
    if let Some(obj) = tool_choice {
        if obj.is_object() {
            let name = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?");
            return format!(
                "You have access to these tools (OpenAI function format):\n{}\nRULES: You MUST call the tool named \"{}\" — reply with its tool call block. Do not answer in plain text.\nEach call is exactly one fenced block, nothing else inside the block:\n```tool_call\n{{\"name\":\"<tool name>\",\"arguments\":{{...}}}}\n```\nUse only the listed tool names. arguments must be a JSON object matching the tool's parameters schema. Put any explanation OUTSIDE the blocks.",
                serde_json::to_string(&defs).unwrap_or_default(),
                name
            );
        }
    }
    let rule = match tool_choice {
        Some(Value::String(s)) if s == "required" => {
            "You MUST call at least one tool — reply with tool call block(s). Do not answer in plain text."
        }
        _ => {
            "If the request needs a tool, reply with one or more tool call blocks. If no tool is needed, answer normally with no blocks."
        }
    };
    vec![
        "You have access to these tools (OpenAI function format):".to_string(),
        serde_json::to_string(&defs).unwrap_or_default(),
        format!("RULES: {}", rule),
        "Each call is exactly one fenced block, nothing else inside the block:".to_string(),
        "```tool_call".to_string(),
        "{\"name\":\"<tool name>\",\"arguments\":{...}}".to_string(),
        "```".to_string(),
        "Use only the listed tool names. arguments must be a JSON object matching the tool's parameters schema. Put any explanation OUTSIDE the blocks.".to_string(),
    ]
    .join("\n")
}

pub struct ParsedToolCall {
    pub name: String,
    pub args: Value,
}

pub fn parse_tool_calls(
    text: &str,
    known_names: Option<&std::collections::HashSet<String>>,
) -> (String, Vec<ParsedToolCall>) {
    let re = Regex::new(r"```tool_call\s*([\s\S]*?)```").unwrap();
    let mut calls = Vec::new();
    let mut rest = text.to_string();
    for m in re.find_iter(text) {
        let full = m.as_str();
        let inner = re
            .captures(full)
            .and_then(|c| c.get(1))
            .map(|c| c.as_str().trim())
            .unwrap_or("");
        if let Ok(obj) = serde_json::from_str::<Value>(inner) {
            let name = obj.get("name").and_then(|n| n.as_str());
            if let Some(name) = name {
                if known_names.map(|k| k.contains(name)).unwrap_or(true) {
                    let args = obj
                        .get("arguments")
                        .filter(|a| a.is_object())
                        .cloned()
                        .unwrap_or(json!({}));
                    calls.push(ParsedToolCall {
                        name: name.to_string(),
                        args,
                    });
                    rest = rest.replace(full, "");
                }
            }
        }
    }
    (rest.trim().to_string(), calls)
}

pub fn parse_event_block(block: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in block.lines() {
        let t = line.trim();
        if !t.starts_with("data:") {
            continue;
        }
        let payload = t[5..].trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str(payload) {
            out.push(v);
        }
    }
    out
}

pub fn rid(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let r: u32 = rand::random();
    format!("{}_{:x}{:x}", prefix, ts, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn msg_text_extracts_text_parts() {
        let m = json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "hello " },
                { "type": "image_url", "image_url": { "url": "https://x/y.png" } },
                { "type": "input_text", "text": "world" },
                "!"
            ]
        });
        assert_eq!(msg_text(&m), "hello world!");
    }

    #[test]
    fn collect_file_parts_maps_urls() {
        let msgs = [json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "look" },
                { "type": "image_url", "image_url": { "url": "https://example.com/a.jpg" } },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } }
            ]
        })];
        let files = collect_file_parts(&msgs);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["type"], "file");
        assert_eq!(files[0]["mime"], "image/jpeg");
        assert_eq!(files[1]["mime"], "image/png");
    }

    #[test]
    fn render_history_skips_system_developer() {
        let out = render_history(&[
            json!({ "role": "system", "content": "sys" }),
            json!({ "role": "developer", "content": "dev" }),
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "hi" },
                    { "type": "image_url", "image_url": { "url": "https://x/i.png" } }
                ]
            }),
        ]);
        assert!(!out.contains("sys"));
        assert!(!out.contains("dev"));
        assert!(out.contains("USER: hi"));
        assert!(out.contains("attached 1 image"));
    }

    #[test]
    fn parse_chat_options_validates() {
        let base = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let o = parse_chat_options(&base).unwrap();
        assert_eq!(o.max_tokens, None);
        let o2 = parse_chat_options(&json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": "END",
            "max_tokens": 50,
            "response_format": { "type": "json_object" }
        }))
        .unwrap();
        assert_eq!(o2.max_tokens, Some(50));
        assert_eq!(o2.stop, Some(vec!["END".into()]));
        assert_eq!(o2.response_format, Some("json_object".into()));
    }

    #[test]
    fn apply_stop_and_max_tokens_test() {
        let opts = ChatOptions {
            max_tokens: None,
            stop: Some(vec!["END".into()]),
            response_format: None,
        };
        let r = apply_stop_and_max_tokens("hello END world", &opts);
        assert_eq!(r.text, "hello ");
        assert_eq!(r.truncated_by, Some("stop".into()));
        let opts_len = ChatOptions {
            max_tokens: Some(1),
            stop: None,
            response_format: None,
        };
        let r2 = apply_stop_and_max_tokens("abcdef", &opts_len);
        assert_eq!(r2.text, "abcd");
        assert_eq!(r2.truncated_by, Some("length".into()));
    }

    #[test]
    fn resolve_model_id_alias() {
        assert_eq!(
            resolve_model_id("muse-spark-1.3-contributor"),
            "muse-spark-1.3-contributor-free"
        );
    }

    #[test]
    fn parse_tool_calls_fence() {
        let text = "hello\n```tool_call\n{\"name\":\"get_weather\",\"arguments\":{\"city\":\"Paris\"}}\n```";
        let names: std::collections::HashSet<String> = ["get_weather".into()].into_iter().collect();
        let (content, calls) = parse_tool_calls(text, Some(&names));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(content, "hello");
    }

    #[test]
    fn parse_event_block_parses_sse() {
        let block = ": heartbeat\ndata: {\"type\":\"message.part.delta\"}\n\ndata: [DONE]\n\ndata: not-json\n\ndata: {\"type\":\"text\"}";
        let evs = parse_event_block(block);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["type"], "message.part.delta");
    }
}
