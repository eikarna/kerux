//! Hermes WebUI / Hermex API compatibility subsystem for Kerux.

pub mod handlers;
pub mod models;
pub mod stream;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;
use tracing::info;

pub use handlers::WebUiState;
pub use models::*;
pub use stream::{
    agent_event_to_unsequenced_frames, evict_expired_completed_streams, split_utf8_safe,
    ActiveStream, AgentRunner, LiveAgentRunner, MockAgentRunner, MAX_CHUNK_SIZE_BYTES,
    MAX_CONCURRENT_RUNNING_STREAMS, MAX_EVENTS_PER_STREAM, MAX_WIRE_FRAME_BYTES, STATE_ACTIVE,
    STATE_CANCELLED, STATE_COMPLETED, STATE_ERRORED, STATE_PANIC_CAUGHT, STREAM_REPLAY_TTL,
};

/// Build the Axum router for the WebUI API.
pub fn build_router(state: WebUiState) -> Router {
    Router::new()
        .route("/health", get(handlers::health_handler))
        .route("/api/auth/status", get(handlers::auth_status_handler))
        .route("/api/auth/login", post(handlers::login_handler))
        .route("/api/sessions", get(handlers::list_sessions_handler))
        .route("/api/session", get(handlers::get_session_handler))
        .route("/api/session/new", post(handlers::new_session_handler))
        .route(
            "/api/session/rename",
            post(handlers::rename_session_handler),
        )
        .route(
            "/api/session/delete",
            post(handlers::delete_session_handler),
        )
        .route("/api/chat/start", post(handlers::chat_start_handler))
        .route("/api/chat/stream", get(handlers::chat_stream_handler))
        .route("/api/chat/cancel", post(handlers::chat_cancel_handler))
        .route("/api/chat/steer", post(handlers::chat_steer_handler))
        .route("/api/workspaces", get(handlers::workspaces_handler))
        .route("/api/list", get(handlers::list_directory_handler))
        .route("/api/file", get(handlers::read_file_handler))
        .route("/api/models", get(handlers::models_handler))
        .route("/api/models/default", get(handlers::default_model_handler))
        .route("/api/default-model", get(handlers::default_model_handler))
        .route("/api/settings", get(handlers::settings_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_and_cors_middleware,
        ))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

/// Middleware handling CORS and authentication protection.
async fn auth_and_cors_middleware(
    State(state): State<WebUiState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Handle CORS preflight unconditionally before any authentication checks
    if method == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        apply_cors_headers(&mut res);
        return res;
    }

    // Check if path is protected
    let is_public = path == "/health" || path == "/api/auth/status" || path == "/api/auth/login";

    if !is_public && state.auth_password.is_some() {
        let is_authed = handlers::verify_auth_headers(&state, req.headers()).await;
        if !is_authed {
            let mut res = (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Unauthorized" })),
            )
                .into_response();
            apply_cors_headers(&mut res);
            return res;
        }
    }

    let mut res = next.run(req).await;
    apply_cors_headers(&mut res);
    res
}

fn apply_cors_headers(res: &mut Response) {
    let headers = res.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization, Content-Type, hermes-auth, Accept, Cache-Control"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
}

/// Serve the WebUI HTTP service on a pre-bound TcpListener with graceful shutdown.
pub async fn serve_webui_listener(
    listener: tokio::net::TcpListener,
    state: WebUiState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> crate::error::Result<()> {
    let addr = listener.local_addr().map_err(|e| {
        crate::error::Error::Agent(format!("Failed to inspect WebUI listener address: {e}"))
    })?;
    let app = build_router(state.clone());

    // Spawn 30-second GC loop
    let gc_streams = state.active_streams.clone();
    let (gc_shutdown_tx, mut gc_shutdown_rx) = tokio::sync::watch::channel(false);
    let gc_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    evict_expired_completed_streams(&gc_streams, None).await;
                }
                _ = gc_shutdown_rx.changed() => {
                    break;
                }
            }
        }
    });

    info!("Hermes WebUI / Hermex server listening at http://{addr}");
    let serve_res = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = gc_shutdown_tx.send(true);
        })
        .await;

    let _ = gc_task.await;

    serve_res.map_err(|e| crate::error::Error::Agent(format!("WebUI server error: {e}")))?;
    Ok(())
}

/// Serve the WebUI HTTP service on the given SocketAddr.
pub async fn serve_webui(state: WebUiState, addr: SocketAddr) -> crate::error::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        crate::error::Error::Agent(format!("Failed to bind WebUI server to {addr}: {e}"))
    })?;
    serve_webui_listener(listener, state, std::future::pending()).await
}
