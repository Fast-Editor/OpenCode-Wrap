use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, map_response, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value as JsonValue};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;

use crate::bootstrap::{ensure_opencode, require_opencode_installed, SpawnState};
use crate::config::Config;
use crate::error::WrapError;
use crate::oco::{upstream_http_status, OcoClient};
use crate::turn::{handle_chat_completions, list_models, stream_chat_completions};

pub struct AppState {
    pub client: OcoClient,
    pub spawn: Mutex<SpawnState>,
    pub in_flight: AtomicUsize,
    pub shutting_down: AtomicBool,
}

fn build_cors(origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods(Any).allow_headers(Any);
    if origins.is_empty() {
        return layer.allow_origin(Any);
    }
    let list: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| match o.parse::<HeaderValue>() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!("[wrap] ignoring invalid WRAP_CORS_ORIGINS entry {:?}", o);
                None
            }
        })
        .collect();
    layer.allow_origin(AllowOrigin::list(list))
}

pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = Config::from_env().map_err(|e| e.to_string())?;
    require_opencode_installed().map_err(|e| e.to_string())?;
    let client = OcoClient::new(config.clone());
    let mut spawn = SpawnState {
        child: None,
        spawned: Arc::new(AtomicBool::new(false)),
    };
    ensure_opencode(&client, &mut spawn)
        .await
        .map_err(|e| e.to_string())?;

    let state = Arc::new(AppState {
        client,
        spawn: Mutex::new(spawn),
        in_flight: AtomicUsize::new(0),
        shutting_down: AtomicBool::new(false),
    });

    if config.wrap_api_key.is_some() {
        tracing::info!("[wrap] WRAP_API_KEY set: /v1 routes require `Authorization: Bearer`");
    }
    if !config.cors_origins.is_empty() {
        tracing::info!("[wrap] CORS restricted to {:?}", config.cors_origins);
    }

    let v1 = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .merge(v1)
        .layer(DefaultBodyLimit::max(config.wrap_max_body_bytes))
        .layer(RequestBodyLimitLayer::new(config.wrap_max_body_bytes))
        .layer(map_response(rewrite_payload_too_large))
        .layer(build_cors(&config.cors_origins))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.wrap_port)).await?;
    tracing::info!(
        "[wrap] OpenAI-compatible API at http://127.0.0.1:{}/v1",
        config.wrap_port
    );

    let state_shutdown = state.clone();
    let shutdown = async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("[wrap] shutdown signal received");
        }
        state_shutdown.shutting_down.store(true, Ordering::SeqCst);
        let in_flight = state_shutdown.in_flight.load(Ordering::SeqCst);
        if in_flight > 0 {
            tracing::info!("[wrap] draining {} in-flight request(s)...", in_flight);
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    if let Some(mut child) = state.spawn.lock().await.child.take() {
        let _ = child.kill().await;
    }
    Ok(())
}

struct InFlightGuard(Arc<AppState>);

impl InFlightGuard {
    /// Reserve a slot, or `None` when `WRAP_MAX_IN_FLIGHT` is already reached.
    fn try_enter(state: &Arc<AppState>) -> Option<Self> {
        let max = state.client.config.max_in_flight;
        let prev = state.in_flight.fetch_add(1, Ordering::SeqCst);
        if prev >= max {
            state.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(state)))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Byte comparison that does not short-circuit on the first mismatch.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn bearer_token(header: Option<&str>) -> Option<&str> {
    let raw = header?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

async fn require_api_key(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(expected) = state.client.config.wrap_api_key.as_deref() else {
        return next.run(req).await;
    };
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let ok = bearer_token(provided)
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if ok {
        return next.run(req).await;
    }
    error_json(
        StatusCode::UNAUTHORIZED,
        "invalid or missing API key; send `Authorization: Bearer <WRAP_API_KEY>`",
        "invalid_request_error",
        "invalid_api_key",
    )
}

async fn rewrite_payload_too_large(res: Response) -> Response {
    if res.status() != StatusCode::PAYLOAD_TOO_LARGE {
        return res;
    }
    error_json(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request body too large",
        "invalid_request_error",
        "request_too_large",
    )
}

async fn health(State(state): State<Arc<AppState>>) -> Json<JsonValue> {
    let spawned = state.spawn.lock().await.spawned.load(Ordering::SeqCst);
    Json(json!({
        "status": "ok",
        "upstream": state.client.config.opencode_base,
        "spawned": spawned
    }))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<JsonValue> {
    Json(list_models(&state.client).await)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(body): Json<JsonValue>,
) -> Response {
    if state.shutting_down.load(Ordering::SeqCst) {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "server shutting down",
            "server_error",
            "shutting_down",
        );
    }
    let Some(guard) = InFlightGuard::try_enter(&state) else {
        return error_json(
            StatusCode::TOO_MANY_REQUESTS,
            "too many concurrent requests; retry shortly (WRAP_MAX_IN_FLIGHT)",
            "rate_limit_error",
            "concurrency_limit_exceeded",
        );
    };
    let stream = body.get("stream").and_then(|v| v.as_bool()) == Some(true);
    tracing::info!(
        "[wrap] chat: model={} msgs={} tools={} stream={} temp={} max_tokens={} stop={} format={}",
        body.get("model").unwrap_or(&JsonValue::Null),
        body.get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        body.get("tools")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        stream,
        body.get("temperature").unwrap_or(&json!("-")),
        body.get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .unwrap_or(&json!("-")),
        body.get("stop").unwrap_or(&JsonValue::Null),
        body.get("response_format")
            .and_then(|rf| rf.get("type"))
            .unwrap_or(&json!("-")),
    );

    if stream {
        match stream_chat_completions(&state.client, &body).await {
            Ok(rx) => {
                let inner = ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
                let guarded = futures::stream::unfold(
                    (inner, Some(guard)),
                    |(mut inner, guard)| async move {
                        let next = inner.next().await;
                        next.map(|item| (item, (inner, guard)))
                    },
                );
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream")
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "keep-alive")
                    .body(Body::from_stream(guarded))
                    .unwrap();
            }
            Err(e) => {
                drop(guard);
                return map_error(e);
            }
        }
    }
    let res = match handle_chat_completions(&state.client, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_error(e),
    };
    drop(guard);
    res
}

fn status_from_u16(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn map_error(e: WrapError) -> Response {
    if let WrapError::ClientAbort(_) = &e {
        return error_json(
            status_from_u16(499),
            "client disconnected",
            "client_error",
            "client_closed_request",
        );
    }
    if let WrapError::Validation(v) = &e {
        return error_json(
            status_from_u16(v.http_status),
            &v.message,
            "invalid_request_error",
            &v.code,
        );
    }
    let status = upstream_http_status(&e);
    let (type_, code, message): (&str, &str, String) = if status == 429 {
        (
            "rate_limit_error",
            "rate_limit_exceeded",
            "Upstream model backend is rate-limited, retry with backoff.".into(),
        )
    } else if status == 502 {
        (
            "server_error",
            "bad_gateway",
            format!(
                "Upstream model backend timed out / unavailable after retries ({}).",
                e
            ),
        )
    } else if let WrapError::Upstream(u) = &e {
        (
            "server_error",
            "upstream_error",
            format!(
                "Upstream model backend failed after retries: {}",
                u.upstream_body.chars().take(200).collect::<String>()
            ),
        )
    } else {
        ("server_error", "internal_error", e.to_string())
    };
    error_json(status_from_u16(status), &message, type_, code)
}

fn error_json(status: StatusCode, message: &str, type_: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": type_,
                "code": code
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_closed_status_is_499() {
        let status = StatusCode::from_u16(499).expect("499 is an allowed status code");
        assert_eq!(status.as_u16(), 499);
        assert_eq!(status_from_u16(499), status);
    }

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn constant_time_eq_compares_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
