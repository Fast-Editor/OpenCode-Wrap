use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::sleep;

use crate::config::Config;
use crate::error::{ClientAbortError, OcoTimeoutError, UpstreamError, WrapError};

#[derive(Clone)]
pub struct OcoClient {
    pub config: Config,
    pub http: Client,
    zen_cache: Arc<RwLock<ZenCache>>,
}

struct ZenCache {
    at: Instant,
    ids: HashSet<String>,
}

impl OcoClient {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            http: Client::new(),
            zen_cache: Arc::new(RwLock::new(ZenCache {
                at: Instant::now() - Duration::from_secs(7200),
                ids: HashSet::new(),
            })),
        }
    }

    pub async fn zen_model_ids(&self) -> HashSet<String> {
        {
            let cache = self.zen_cache.read().await;
            if cache.at.elapsed() < Duration::from_secs(3600) && !cache.ids.is_empty() {
                return cache.ids.clone();
            }
        }
        let url = self.config.zen_models_url.clone();
        let res = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                if let Ok(json) = r.json::<Value>().await {
                    let mut ids = HashSet::new();
                    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                        for m in data {
                            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                                ids.insert(id.to_string());
                            }
                        }
                    }
                    if !ids.is_empty() {
                        let mut cache = self.zen_cache.write().await;
                        *cache = ZenCache {
                            at: Instant::now(),
                            ids: ids.clone(),
                        };
                        return ids;
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    "[wrap] zen models fetch failed ({}), skipping validation",
                    e
                );
            }
            Ok(r) => {
                tracing::error!(
                    "[wrap] zen models fetch failed (status {}), skipping validation",
                    r.status()
                );
            }
        }
        self.zen_cache.read().await.ids.clone()
    }
}

pub async fn oco(
    client: &OcoClient,
    path: &str,
    method: &str,
    body: Option<&Value>,
    timeout_ms: Option<u64>,
) -> Result<Value, WrapError> {
    let timeout_ms = timeout_ms.unwrap_or(client.config.oco_timeout_ms);
    let url = format!("{}{}", client.config.opencode_base, path);
    let mut req = client
        .http
        .request(method.parse().unwrap_or(reqwest::Method::GET), &url);
    req = req.header("Content-Type", "application/json");
    if let Some(b) = body {
        req = req.json(b);
    }
    req = req.timeout(Duration::from_millis(timeout_ms));
    let res = req.send().await;
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return Err(WrapError::Timeout(OcoTimeoutError::new(
                    method, path, timeout_ms,
                )));
            }
            if e.is_connect() || e.is_request() {
                return Err(WrapError::Other(e.to_string()));
            }
            return Err(WrapError::Other(e.to_string()));
        }
    };
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let json: Option<Value> = if text.is_empty() {
        None
    } else {
        serde_json::from_str(&text).ok()
    };
    if status < 200 || status >= 300 {
        return Err(WrapError::Upstream(UpstreamError {
            upstream_status: status,
            upstream_body: text.chars().take(2000).collect(),
        }));
    }
    Ok(json.unwrap_or(Value::Null))
}

pub fn is_timeout_abort(err: &WrapError) -> bool {
    matches!(err, WrapError::Timeout(_))
}

pub fn is_transient_connection_error(err: &WrapError) -> bool {
    if let WrapError::Other(e) = err {
        let msg = e.to_string();
        return msg.contains("ECONNREFUSED")
            || msg.contains("ECONNRESET")
            || msg.contains("connection")
            || msg.contains("fetch failed");
    }
    false
}

pub fn is_retryable_upstream(err: &WrapError) -> bool {
    if matches!(err, WrapError::ClientAbort(_)) {
        return false;
    }
    if is_timeout_abort(err) || is_transient_connection_error(err) {
        return true;
    }
    if let WrapError::Upstream(u) = err {
        if u.upstream_status >= 400 && u.upstream_status < 500 {
            return false;
        }
        return true;
    }
    false
}

pub fn is_rate_limited_upstream(err: &WrapError) -> bool {
    if let WrapError::Upstream(u) = err {
        let body = u.upstream_body.to_lowercase();
        return body.contains("rate")
            || body.contains("429")
            || body.contains("freeusagelimit")
            || body.contains("overloaded")
            || body.contains("capacity")
            || body.contains("too many requests");
    }
    false
}

pub fn upstream_http_status(err: &WrapError) -> u16 {
    match err {
        WrapError::ClientAbort(_) => 499,
        WrapError::Timeout(_) => 502,
        e if is_rate_limited_upstream(e) => 429,
        e if is_transient_connection_error(e) => 502,
        WrapError::Upstream(u) => {
            if u.upstream_status >= 500 {
                502
            } else {
                u.upstream_status
            }
        }
        _ => 500,
    }
}

pub async fn prompt_with_retry<F, Fut, T>(mut f: F, max_attempts: u32) -> Result<T, WrapError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, WrapError>>,
{
    let mut last_err: Option<WrapError> = None;
    for attempt in 1..=max_attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                last_err = Some(err.clone());
                let detail = match &err {
                    WrapError::Upstream(u) => format!(
                        "upstream {}: {}",
                        u.upstream_status,
                        u.upstream_body.chars().take(300).collect::<String>()
                    ),
                    _ => format!("{}", err),
                };
                tracing::error!(
                    "[wrap] attempt {}/{} failed: {}",
                    attempt,
                    max_attempts,
                    detail
                );
                if !is_retryable_upstream(&err) || attempt == max_attempts {
                    return Err(err);
                }
                let delay = if is_rate_limited_upstream(&err) {
                    4000
                } else {
                    1500 * attempt as u64
                };
                sleep(Duration::from_millis(delay)).await;
            }
        }
    }
    Err(last_err.unwrap_or(WrapError::ClientAbort(ClientAbortError::default())))
}

pub async fn zen_model_ids(client: &OcoClient) -> HashSet<String> {
    client.zen_model_ids().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ClientAbortError, OcoTimeoutError, UpstreamError, WrapError};

    #[test]
    fn timeout_maps_to_502_and_retryable() {
        let t = WrapError::Timeout(OcoTimeoutError::new("POST", "/session", 10));
        assert_eq!(upstream_http_status(&t), 502);
        assert!(is_retryable_upstream(&t));
    }

    #[test]
    fn client_abort_not_retryable() {
        let a = WrapError::ClientAbort(ClientAbortError::default());
        assert!(!is_retryable_upstream(&a));
        assert_eq!(upstream_http_status(&a), 499);
    }

    #[test]
    fn upstream_4xx_not_retryable() {
        let u = WrapError::Upstream(UpstreamError {
            upstream_status: 404,
            upstream_body: "nope".into(),
        });
        assert!(!is_retryable_upstream(&u));
    }
}
