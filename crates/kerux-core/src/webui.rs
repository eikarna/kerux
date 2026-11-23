//! WebUI & Hermex iOS REST & SSE API compatibility layer.
//!
//! Exposes an Axum HTTP listener compatible with the [Hermex](https://github.com/uzairansaruzi/hermex)
//! native iOS SwiftUI client and standard Hermes WebUI dashboards:
//! - Health check: `GET /health`
//! - Authentication gate: `GET /api/auth/status`, `POST /api/auth/login`
//! - Session management: `GET /api/sessions`, `GET /api/session`, `POST /api/session/new`, `POST /api/session/rename`, `POST /api/session/delete`
//! - Chat lifecycle: `POST /api/chat/start`, `GET /api/chat/stream`, `GET/POST /api/chat/cancel`, `POST /api/chat/steer`
//! - Models & settings: `GET /api/models`, `GET /api/settings`

use axum::{
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error};

use crate::error::Result;
use crate::session_store::SessionStore;

/// Server-Sent Events emitted to WebUI / Hermex iOS clients during a chat turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WebUiSseEvent {
    Token {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolStarted {
        name: String,
        call_id: String,
        arguments: String,
    },
    ToolCompleted {
        name: String,
        call_id: String,
        content: String,
        success: bool,
    },
    Metering {
        total_tokens: usize,
        tokens_per_second: Option<f64>,
    },
    Done {
        session_id: String,
    },
    Error {
        message: String,
    },
    StreamEnd,
}

/// Abstract runner that executes agent turns on behalf of the WebUI.
#[async_trait::async_trait]
pub trait WebUiAgentRunner: Send + Sync {
    /// Execute one turn for `session_id`, emitting events over `event_sender`.
    async fn run_turn(
        &self,
        session_id: &str,
        user_message: &str,
        event_sender: broadcast::Sender<WebUiSseEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<()>;

    /// Model name configured as default.
    fn default_model(&self) -> String;
}

/// Built-in runner wiring a [`crate::agent::KeruxAgent`] directly to session persistence.
pub struct DirectAgentRunner {
    agent: Arc<crate::agent::KeruxAgent>,
    session_store: SessionStore,
    default_model: String,
    run_lock: Arc<tokio::sync::Mutex<()>>,
}

impl DirectAgentRunner {
    pub fn new(
        agent: Arc<crate::agent::KeruxAgent>,
        session_store: SessionStore,
        default_model: String,
    ) -> Self {
        Self {
            agent,
            session_store,
            default_model,
            run_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait::async_trait]
impl WebUiAgentRunner for DirectAgentRunner {
    async fn run_turn(
        &self,
        session_id: &str,
        user_message: &str,
        event_sender: broadcast::Sender<WebUiSseEvent>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        let _guard = self.run_lock.lock().await;

        // Load prior session messages
        let session_data = self.session_store.load(session_id);
        self.agent.clear_history().await;

        if let Some(summary) = &session_data.summary {
            self.agent
                .add_message(crate::client::Message::system(format!(
                    "{}\n{}",
                    crate::agent::CONTEXT_SUMMARY_MARKER,
                    summary
                )))
                .await;
        }

        for msg in session_data.messages {
            self.agent.add_message(msg).await;
        }

        // Setup event pipe
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<crate::agent::AgentEvent>(64);
        self.agent.set_event_sender(Some(agent_tx));

        let sse_tx = event_sender.clone();
        let session_id_str = session_id.to_string();
        let pump_handle = tokio::spawn(async move {
            while let Some(event) = agent_rx.recv().await {
                let sse_event = match event {
                    crate::agent::AgentEvent::Content { text } => {
                        Some(WebUiSseEvent::Token { text })
                    }
                    crate::agent::AgentEvent::Reasoning { text } => {
                        Some(WebUiSseEvent::Reasoning { text })
                    }
                    crate::agent::AgentEvent::Thinking { content } => {
                        Some(WebUiSseEvent::Reasoning { text: content })
                    }
                    crate::agent::AgentEvent::ToolStart {
                        name,
                        call_id,
                        arguments,
                    } => Some(WebUiSseEvent::ToolStarted {
                        name,
                        call_id,
                        arguments,
                    }),
                    crate::agent::AgentEvent::ToolComplete { result } => {
                        Some(WebUiSseEvent::ToolCompleted {
                            name: result.tool_call_id.clone(),
                            call_id: result.tool_call_id,
                            content: result.content,
                            success: result.success,
                        })
                    }
                    crate::agent::AgentEvent::Telemetry { telemetry } => {
                        Some(WebUiSseEvent::Metering {
                            total_tokens: telemetry.total_tokens,
                            tokens_per_second: telemetry.tokens_per_second,
                        })
                    }
                    crate::agent::AgentEvent::Done { .. } => Some(WebUiSseEvent::Done {
                        session_id: session_id_str.clone(),
                    }),
                    crate::agent::AgentEvent::Error { error } => {
                        Some(WebUiSseEvent::Error { message: error })
                    }
                    _ => None,
                };

                if let Some(ev) = sse_event {
                    let _ = sse_tx.send(ev);
                }
            }
        });

        let outcome = self
            .agent
            .run_with_cancel(user_message.to_string(), cancel_flag.clone())
            .await;

        self.agent.set_event_sender(None);
        let _ = pump_handle.await;

        // Persist updated conversation
        let updated_conv = self.agent.conversation().await;
        let _ = self
            .session_store
            .save(session_id, session_data.summary.as_deref(), &updated_conv);

        match outcome {
            Ok(_) => {
                let _ = event_sender.send(WebUiSseEvent::Done {
                    session_id: session_id.to_string(),
                });
            }
            Err(e) => {
                let err_msg = if cancel_flag.load(Ordering::SeqCst) {
                    "Run cancelled by user".to_string()
                } else {
                    e.to_string()
                };
                let _ = event_sender.send(WebUiSseEvent::Error { message: err_msg });
            }
        }

        let _ = event_sender.send(WebUiSseEvent::StreamEnd);
        Ok(())
    }

    fn default_model(&self) -> String {
        self.default_model.clone()
    }
}

/// Active in-flight stream tracking.
#[derive(Clone)]
pub struct WebUiActiveStream {
    pub stream_id: String,
    pub session_id: String,
    pub event_tx: broadcast::Sender<WebUiSseEvent>,
    pub cancel_flag: Arc<AtomicBool>,
    pub created_at: Instant,
}

/// Shared state for the WebUI Axum router.
#[derive(Clone)]
pub struct WebUiServerState {
    pub session_store: SessionStore,
    pub password: Option<String>,
    pub runner: Option<Arc<dyn WebUiAgentRunner>>,
    pub active_streams: Arc<RwLock<HashMap<String, WebUiActiveStream>>>,
    pub start_time: Instant,
}

impl WebUiServerState {
    pub fn new(
        session_store: SessionStore,
        password: Option<String>,
        runner: Option<Arc<dyn WebUiAgentRunner>>,
    ) -> Self {
        Self {
            session_store,
            password,
            runner,
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
        }
    }
}

/// Build the Axum router for the WebUI API layer.
pub fn create_webui_router(state: WebUiServerState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/api/auth/status", get(auth_status_handler))
        .route("/api/auth/login", post(auth_login_handler))
        .route("/api/sessions", get(sessions_list_handler))
        .route("/api/session", get(session_detail_handler))
        .route("/api/session/new", post(session_new_handler))
        .route("/api/session/rename", post(session_rename_handler))
        .route("/api/session/delete", post(session_delete_handler))
        .route("/api/chat/start", post(chat_start_handler))
        .route("/api/chat/stream", get(chat_stream_handler))
        .route(
            "/api/chat/cancel",
            get(chat_cancel_get_handler).post(chat_cancel_post_handler),
        )
        .route("/api/chat/steer", post(chat_steer_handler))
        .route("/api/models", get(models_list_handler))
        .route("/api/settings", get(settings_handler))
        .layer(middleware::from_fn(cors_middleware))
        .with_state(state)
}

/// Serve the WebUI listener until shutdown signal resolves.
pub async fn serve_webui_listener<F>(
    listener: tokio::net::TcpListener,
    state: WebUiServerState,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, create_webui_router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| crate::error::Error::Agent(format!("WebUI server failed: {error}")))
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

async fn cors_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    if method == axum::http::Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(
            "Access-Control-Allow-Origin",
            "*".parse().unwrap_or_else(|_| "null".parse().unwrap()),
        );
        headers.insert(
            "Access-Control-Allow-Methods",
            "GET, POST, OPTIONS, PUT, DELETE"
                .parse()
                .unwrap_or_else(|_| "null".parse().unwrap()),
        );
        headers.insert(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, Accept"
                .parse()
                .unwrap_or_else(|_| "null".parse().unwrap()),
        );
        return response;
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "Access-Control-Allow-Origin",
        "*".parse().unwrap_or_else(|_| "null".parse().unwrap()),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        "GET, POST, OPTIONS, PUT, DELETE"
            .parse()
            .unwrap_or_else(|_| "null".parse().unwrap()),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization, Accept"
            .parse()
            .unwrap_or_else(|_| "null".parse().unwrap()),
    );
    response
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    sessions: usize,
    #[serde(rename = "activeStreams")]
    active_streams: usize,
    #[serde(rename = "uptimeSeconds")]
    uptime_seconds: f64,
}

async fn health_handler(State(state): State<WebUiServerState>) -> Json<HealthResponse> {
    let sessions = state.session_store.list().len();
    let active_streams = state.active_streams.read().await.len();
    let uptime_seconds = state.start_time.elapsed().as_secs_f64();

    Json(HealthResponse {
        status: "ok",
        sessions,
        active_streams,
        uptime_seconds,
    })
}

#[derive(Serialize)]
struct AuthStatusResponse {
    #[serde(rename = "authEnabled")]
    auth_enabled: bool,
    #[serde(rename = "loggedIn")]
    logged_in: bool,
    #[serde(rename = "passwordAuthEnabled")]
    password_auth_enabled: bool,
}

async fn auth_status_handler(State(state): State<WebUiServerState>) -> Json<AuthStatusResponse> {
    let has_pw = state.password.is_some();
    Json(AuthStatusResponse {
        auth_enabled: has_pw,
        logged_in: !has_pw,
        password_auth_enabled: has_pw,
    })
}

#[derive(Deserialize)]
struct LoginRequest {
    password: Option<String>,
}

#[derive(Serialize)]
struct LoginResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn auth_login_handler(
    State(state): State<WebUiServerState>,
    Json(body): Json<LoginRequest>,
) -> (StatusCode, Json<LoginResponse>) {
    match &state.password {
        None => (
            StatusCode::OK,
            Json(LoginResponse {
                ok: true,
                message: Some("No authentication configured".to_string()),
                error: None,
            }),
        ),
        Some(expected) => {
            if body.password.as_deref() == Some(expected.as_str()) {
                (
                    StatusCode::OK,
                    Json(LoginResponse {
                        ok: true,
                        message: Some("Authenticated".to_string()),
                        error: None,
                    }),
                )
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(LoginResponse {
                        ok: false,
                        message: None,
                        error: Some("Invalid password".to_string()),
                    }),
                )
            }
        }
    }
}

#[derive(Deserialize)]
struct SessionsQuery {
    #[serde(rename = "include_archived")]
    _include_archived: Option<bool>,
}

#[derive(Serialize)]
struct SessionSummaryItem {
    #[serde(rename = "sessionId")]
    session_id: String,
    title: String,
    #[serde(rename = "messageCount")]
    message_count: usize,
    #[serde(rename = "estimatedTokens")]
    estimated_tokens: usize,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
}

#[derive(Serialize)]
struct SessionsListResponse {
    sessions: Vec<SessionSummaryItem>,
    count: usize,
}

async fn sessions_list_handler(
    State(state): State<WebUiServerState>,
    Query(_query): Query<SessionsQuery>,
) -> Json<SessionsListResponse> {
    let list = state.session_store.list();
    let sessions: Vec<SessionSummaryItem> = list
        .into_iter()
        .map(|item| SessionSummaryItem {
            session_id: item.id,
            title: item.title,
            message_count: item.message_count,
            estimated_tokens: item.estimated_tokens,
            updated_at: item.updated_at,
        })
        .collect();
    let count = sessions.len();
    Json(SessionsListResponse { sessions, count })
}

#[derive(Deserialize)]
struct SessionDetailQuery {
    id: Option<String>,
    session_id: Option<String>,
}

#[derive(Serialize)]
struct ChatMessageItem {
    role: String,
    content: String,
    #[serde(rename = "messageId")]
    message_id: String,
    timestamp: f64,
}

#[derive(Serialize)]
struct SessionDetailItem {
    #[serde(rename = "sessionId")]
    session_id: String,
    title: String,
    #[serde(rename = "messageCount")]
    message_count: usize,
    messages: Vec<ChatMessageItem>,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
}

#[derive(Serialize)]
struct SessionDetailResponse {
    session: SessionDetailItem,
}

async fn session_detail_handler(
    State(state): State<WebUiServerState>,
    Query(query): Query<SessionDetailQuery>,
) -> Response {
    let session_id = match query.id.or(query.session_id) {
        Some(id) => id,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let data = state.session_store.load(&session_id);
    let mut title = "(empty session)".to_string();
    let mut messages = Vec::new();

    for (idx, msg) in data.messages.iter().enumerate() {
        let role = match msg.role {
            crate::client::Role::User => "user",
            crate::client::Role::Assistant => "assistant",
            crate::client::Role::System => "system",
            crate::client::Role::Tool => "tool",
        };
        if title == "(empty session)" && msg.role == crate::client::Role::User {
            let first = msg.content.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                title = first.chars().take(60).collect();
            }
        }
        messages.push(ChatMessageItem {
            role: role.to_string(),
            content: msg.content.clone(),
            message_id: format!("{role}-{idx}"),
            timestamp: 0.0,
        });
    }

    if let Some(summary) = data.summary {
        if title == "(empty session)" {
            let first = summary.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                title = first.chars().take(60).collect();
            }
        }
    }

    let message_count = messages.len();
    Json(SessionDetailResponse {
        session: SessionDetailItem {
            session_id,
            title,
            message_count,
            messages,
            updated_at: current_timestamp_secs(),
        },
    })
    .into_response()
}

#[derive(Deserialize)]
struct SessionNewRequest {
    #[serde(rename = "sessionId")]
    session_id_camel: Option<String>,
    session_id: Option<String>,
    title: Option<String>,
}

#[derive(Serialize)]
struct SessionNewResponse {
    session: SessionDetailItem,
}

async fn session_new_handler(
    State(_state): State<WebUiServerState>,
    Json(body): Json<SessionNewRequest>,
) -> Json<SessionNewResponse> {
    let id = body
        .session_id_camel
        .or(body.session_id)
        .unwrap_or_else(|| format!("sess_{:x}", current_timestamp_millis()));
    let title = body.title.unwrap_or_else(|| "New Session".to_string());

    Json(SessionNewResponse {
        session: SessionDetailItem {
            session_id: id,
            title,
            message_count: 0,
            messages: Vec::new(),
            updated_at: current_timestamp_secs(),
        },
    })
}

#[derive(Deserialize)]
struct SessionRenameRequest {
    #[serde(rename = "sessionId")]
    session_id_camel: Option<String>,
    session_id: Option<String>,
    _title: Option<String>,
}

async fn session_rename_handler(
    State(_state): State<WebUiServerState>,
    Json(body): Json<SessionRenameRequest>,
) -> Response {
    if body.session_id_camel.or(body.session_id).is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct SessionDeleteRequest {
    #[serde(rename = "sessionId")]
    session_id_camel: Option<String>,
    session_id: Option<String>,
}

async fn session_delete_handler(
    State(state): State<WebUiServerState>,
    Json(body): Json<SessionDeleteRequest>,
) -> Response {
    let id = match body.session_id_camel.or(body.session_id) {
        Some(id) => id,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    state.session_store.clear(&id);
    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct ChatStartRequest {
    #[serde(rename = "sessionId")]
    session_id_camel: Option<String>,
    session_id: Option<String>,
    message: String,
}

#[derive(Serialize)]
struct ChatStartResponse {
    #[serde(rename = "streamId")]
    stream_id: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

async fn chat_start_handler(
    State(state): State<WebUiServerState>,
    Json(body): Json<ChatStartRequest>,
) -> Response {
    let session_id = body
        .session_id_camel
        .or(body.session_id)
        .unwrap_or_else(|| "cli:default".to_string());

    let runner = match state.runner.clone() {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Agent runner is not configured on this server".to_string(),
            )
                .into_response();
        }
    };

    let stream_id = format!("stream_{:x}", current_timestamp_millis());
    let (event_tx, _) = broadcast::channel::<WebUiSseEvent>(128);
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let active_stream = WebUiActiveStream {
        stream_id: stream_id.clone(),
        session_id: session_id.clone(),
        event_tx: event_tx.clone(),
        cancel_flag: cancel_flag.clone(),
        created_at: Instant::now(),
    };

    state
        .active_streams
        .write()
        .await
        .insert(stream_id.clone(), active_stream);

    // Spawn agent runner task
    let active_streams = state.active_streams.clone();
    let stream_id_clone = stream_id.clone();
    let session_id_clone = session_id.clone();
    let message_clone = body.message;

    tokio::spawn(async move {
        let res = runner
            .run_turn(
                &session_id_clone,
                &message_clone,
                event_tx.clone(),
                cancel_flag,
            )
            .await;

        if let Err(e) = res {
            error!(error = %e, "WebUI agent turn execution failed");
            let _ = event_tx.send(WebUiSseEvent::Error {
                message: e.to_string(),
            });
            let _ = event_tx.send(WebUiSseEvent::StreamEnd);
        }

        // Clean up active stream
        active_streams.write().await.remove(&stream_id_clone);
    });

    Json(ChatStartResponse {
        stream_id,
        session_id,
    })
    .into_response()
}

#[derive(Deserialize)]
struct ChatStreamQuery {
    #[serde(rename = "stream_id")]
    stream_id_snake: Option<String>,
    #[serde(rename = "streamId")]
    stream_id_camel: Option<String>,
}

async fn chat_stream_handler(
    State(state): State<WebUiServerState>,
    Query(query): Query<ChatStreamQuery>,
) -> Response {
    let stream_id = match query.stream_id_camel.or(query.stream_id_snake) {
        Some(id) => id,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let rx = {
        let streams = state.active_streams.read().await;
        match streams.get(&stream_id) {
            Some(s) => s.event_tx.subscribe(),
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    };

    let stream = stream_to_sse(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn stream_to_sse(
    rx: broadcast::Receiver<WebUiSseEvent>,
) -> impl Stream<Item = std::result::Result<Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(event) => {
                let sse_event = match event {
                    WebUiSseEvent::Token { text } => Event::default()
                        .event("token")
                        .data(serde_json::json!({ "text": text }).to_string()),
                    WebUiSseEvent::Reasoning { text } => Event::default()
                        .event("reasoning")
                        .data(serde_json::json!({ "text": text }).to_string()),
                    WebUiSseEvent::ToolStarted {
                        name,
                        call_id,
                        arguments,
                    } => Event::default().event("toolStarted").data(
                        serde_json::json!({ "name": name, "callId": call_id, "arguments": arguments })
                            .to_string(),
                    ),
                    WebUiSseEvent::ToolCompleted {
                        name,
                        call_id,
                        content,
                        success,
                    } => Event::default().event("toolCompleted").data(
                        serde_json::json!({ "name": name, "callId": call_id, "content": content, "success": success })
                            .to_string(),
                    ),
                    WebUiSseEvent::Metering {
                        total_tokens,
                        tokens_per_second,
                    } => Event::default().event("metering").data(
                        serde_json::json!({ "totalTokens": total_tokens, "displayableTokensPerSecond": tokens_per_second })
                            .to_string(),
                    ),
                    WebUiSseEvent::Done { session_id } => Event::default()
                        .event("done")
                        .data(serde_json::json!({ "sessionId": session_id }).to_string()),
                    WebUiSseEvent::Error { message } => Event::default()
                        .event("error")
                        .data(serde_json::json!({ "message": message }).to_string()),
                    WebUiSseEvent::StreamEnd => Event::default().event("streamEnd").data("{}"),
                };
                Some((Ok(sse_event), rx))
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                debug!(skipped, "SSE stream skipped lagged messages");
                Some((Ok(Event::default().event("heartbeat").data("{}")), rx))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
}

#[derive(Deserialize)]
struct ChatCancelQuery {
    #[serde(rename = "stream_id")]
    stream_id_snake: Option<String>,
    #[serde(rename = "streamId")]
    stream_id_camel: Option<String>,
}

#[derive(Deserialize)]
struct ChatCancelBody {
    #[serde(rename = "stream_id")]
    stream_id_snake: Option<String>,
    #[serde(rename = "streamId")]
    stream_id_camel: Option<String>,
}

async fn chat_cancel_get_handler(
    State(state): State<WebUiServerState>,
    Query(query): Query<ChatCancelQuery>,
) -> Json<serde_json::Value> {
    if let Some(id) = query.stream_id_camel.or(query.stream_id_snake) {
        let streams = state.active_streams.read().await;
        if let Some(stream) = streams.get(&id) {
            stream.cancel_flag.store(true, Ordering::SeqCst);
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

async fn chat_cancel_post_handler(
    State(state): State<WebUiServerState>,
    Json(body): Json<ChatCancelBody>,
) -> Json<serde_json::Value> {
    if let Some(id) = body.stream_id_camel.or(body.stream_id_snake) {
        let streams = state.active_streams.read().await;
        if let Some(stream) = streams.get(&id) {
            stream.cancel_flag.store(true, Ordering::SeqCst);
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ChatSteerRequest {
    #[serde(rename = "sessionId")]
    _session_id_camel: Option<String>,
    _session_id: Option<String>,
    _text: Option<String>,
}

async fn chat_steer_handler(
    State(_state): State<WebUiServerState>,
    Json(_body): Json<ChatSteerRequest>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Serialize)]
struct ModelItem {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct ModelsResponse {
    models: Vec<ModelItem>,
}

async fn models_list_handler(State(state): State<WebUiServerState>) -> Json<ModelsResponse> {
    let model_name = state
        .runner
        .as_ref()
        .map(|r| r.default_model())
        .unwrap_or_else(|| "default".to_string());

    Json(ModelsResponse {
        models: vec![ModelItem {
            id: model_name.clone(),
            name: model_name,
        }],
    })
}

#[derive(Serialize)]
struct SettingsResponse {
    #[serde(rename = "defaultModel")]
    default_model: String,
    #[serde(rename = "authConfigured")]
    auth_configured: bool,
}

async fn settings_handler(State(state): State<WebUiServerState>) -> Json<SettingsResponse> {
    let default_model = state
        .runner
        .as_ref()
        .map(|r| r.default_model())
        .unwrap_or_else(|| "default".to_string());

    Json(SettingsResponse {
        default_model,
        auth_configured: state.password.is_some(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn current_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct MockRunner;

    #[async_trait::async_trait]
    impl WebUiAgentRunner for MockRunner {
        async fn run_turn(
            &self,
            session_id: &str,
            _user_message: &str,
            event_sender: broadcast::Sender<WebUiSseEvent>,
            _cancel_flag: Arc<AtomicBool>,
        ) -> Result<()> {
            let _ = event_sender.send(WebUiSseEvent::Token {
                text: "Hello from mock agent!".to_string(),
            });
            let _ = event_sender.send(WebUiSseEvent::Done {
                session_id: session_id.to_string(),
            });
            let _ = event_sender.send(WebUiSseEvent::StreamEnd);
            Ok(())
        }

        fn default_model(&self) -> String {
            "mock-model".to_string()
        }
    }

    fn test_server_state(temp_dir: &std::path::Path) -> WebUiServerState {
        let store = SessionStore::new(temp_dir.to_path_buf());
        WebUiServerState::new(
            store,
            Some("secret123".to_string()),
            Some(Arc::new(MockRunner)),
        )
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_server_state(dir.path());
        let router = create_webui_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn test_auth_status_and_login() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_server_state(dir.path());
        let router = create_webui_router(state);

        // Status
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Invalid login
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Valid login
        let res = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"password":"secret123"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sessions_crud() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_server_state(dir.path());
        let router = create_webui_router(state);

        // New session
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session/new")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"sessionId":"test-123","title":"Test Session"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // List sessions
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Delete session
        let res = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session/delete")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"sessionId":"test-123"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_models_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_server_state(dir.path());
        let router = create_webui_router(state);

        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = router
            .oneshot(
                Request::builder()
                    .uri("/api/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
