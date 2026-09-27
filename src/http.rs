use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt as FuturesStreamExt;
use serde_json::{json, Value as JsonValue};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
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

pub fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
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

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = Router::new()
            .route("/health", get(health))
            .route("/v1/health", get(health))
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat_completions))
            .layer(RequestBodyLimitLayer::new(config.wrap_max_body_bytes))
            .layer(cors)
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
    })
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
    state.in_flight.fetch_add(1, Ordering::SeqCst);
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

    let res = if stream {
        match stream_chat_completions(&state.client, &body).await {
            Ok(rx) => {
                let stream = ReceiverStream::new(rx).map(|s| Ok::<_, std::convert::Infallible>(s));
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream")
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "keep-alive")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
            Err(e) => map_error(e),
        }
    } else {
        match handle_chat_completions(&state.client, &body).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => map_error(e),
        }
    };
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    res
}

fn map_error(e: WrapError) -> Response {
    if let WrapError::Validation(v) = &e {
        return error_json(
            StatusCode::from_u16(v.http_status).unwrap_or(StatusCode::BAD_REQUEST),
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
    error_json(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &message,
        type_,
        code,
    )
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
