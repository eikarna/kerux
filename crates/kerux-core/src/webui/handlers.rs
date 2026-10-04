//! HTTP route handlers for Hermes WebUI / Hermex API compatibility.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::error;

use super::models::*;
use super::stream::{
    agent_event_to_unsequenced_frames, evict_expired_completed_streams, ActiveStream, AgentRunner,
    MAX_CONCURRENT_RUNNING_STREAMS, STATE_CANCELLED, STATE_COMPLETED, STATE_ERRORED,
};
use crate::client::Message;

lazy_static::lazy_static! {
    static ref SESSION_ID_REGEX: Regex = Regex::new(r"^[a-zA-Z0-9_-]{1,64}$").unwrap();
}

pub const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10MB

pub type TokenGeneratorFn = Arc<dyn Fn() -> crate::error::Result<String> + Send + Sync>;
pub type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
pub struct WebUiState {
    pub session_dir: PathBuf,
    pub session_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
    pub active_streams: Arc<RwLock<HashMap<String, Arc<ActiveStream>>>>,
    pub auth_password: Option<String>,
    pub auth_token: Arc<RwLock<Option<String>>>,
    pub runtime_config: Arc<RwLock<crate::config::AppConfig>>,
    pub workspace_root: PathBuf,
    pub agent_runner: Arc<dyn AgentRunner>,
    pub token_generator: Option<TokenGeneratorFn>,
    pub clock_override: Option<ClockFn>,
    pub persistence_failure_override: Arc<AtomicBool>,
}

impl WebUiState {
    pub fn new(
        runtime_config: Arc<RwLock<crate::config::AppConfig>>,
        auth_password_raw: String,
        workspace_root: PathBuf,
    ) -> Self {
        let auth_password = if auth_password_raw.trim().is_empty() {
            None
        } else {
            Some(auth_password_raw.trim().to_string())
        };

        let session_dir = crate::platform::kerux_home().join("sessions");
        let runner = Arc::new(super::stream::LiveAgentRunner::new(runtime_config.clone()));

        Self {
            session_dir,
            session_locks: Arc::new(RwLock::new(HashMap::new())),
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            auth_password,
            auth_token: Arc::new(RwLock::new(None)),
            runtime_config,
            workspace_root,
            agent_runner: runner,
            token_generator: None,
            clock_override: None,
            persistence_failure_override: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_runner_and_dir(
        runtime_config: Arc<RwLock<crate::config::AppConfig>>,
        auth_password: Option<String>,
        workspace_root: PathBuf,
        session_dir: PathBuf,
        runner: Arc<dyn AgentRunner>,
    ) -> Self {
        Self {
            session_dir,
            session_locks: Arc::new(RwLock::new(HashMap::new())),
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            auth_password,
            auth_token: Arc::new(RwLock::new(None)),
            runtime_config,
            workspace_root,
            agent_runner: runner,
            token_generator: None,
            clock_override: None,
            persistence_failure_override: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn get_session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.write().await;
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn generate_token(&self) -> crate::error::Result<String> {
        if let Some(ref gen) = self.token_generator {
            gen()
        } else {
            generate_random_token()
        }
    }
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn generate_random_token() -> crate::error::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| {
        crate::error::Error::Agent(format!("Cryptographic entropy generation failed: {e}"))
    })?;
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    Ok(s)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// === Route Handlers ===

pub async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

pub async fn auth_status_handler(
    State(state): State<WebUiState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let is_auth = if state.auth_password.is_none() {
        true
    } else {
        verify_auth_headers(&state, &headers).await
    };

    Json(AuthStatusResponse {
        authenticated: is_auth,
        profile: "default".to_string(),
        version: "0.4.0".to_string(),
    })
}

pub async fn login_handler(
    State(state): State<WebUiState>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let expected = match &state.auth_password {
        Some(p) => p,
        None => {
            let token = match state.generate_token() {
                Ok(t) => {
                    let mut tok = state.auth_token.write().await;
                    if tok.is_none() {
                        *tok = Some(t.clone());
                    }
                    tok.clone().unwrap_or(t)
                }
                Err(e) => {
                    error!("Token generation error: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorResponse {
                            error: "Cryptographic token generation failed".to_string(),
                        }),
                    )
                        .into_response();
                }
            };
            let mut res = (
                StatusCode::OK,
                Json(LoginResponse {
                    status: "ok".to_string(),
                    token: token.clone(),
                }),
            )
                .into_response();
            let cookie = format!("hermes_auth={token}; HttpOnly; SameSite=Lax; Path=/");
            if let Ok(val) = cookie.parse() {
                res.headers_mut().insert(header::SET_COOKIE, val);
            }
            return res;
        }
    };

    let provided = req.password.unwrap_or_default();
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        let token = match state.generate_token() {
            Ok(t) => {
                let mut tok = state.auth_token.write().await;
                *tok = Some(t.clone());
                t
            }
            Err(e) => {
                error!("Token generation error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "Cryptographic token generation failed".to_string(),
                    }),
                )
                    .into_response();
            }
        };

        let mut res = (
            StatusCode::OK,
            Json(LoginResponse {
                status: "ok".to_string(),
                token: token.clone(),
            }),
        )
            .into_response();
        let cookie = format!("hermes_auth={token}; HttpOnly; SameSite=Lax; Path=/");
        if let Ok(val) = cookie.parse() {
            res.headers_mut().insert(header::SET_COOKIE, val);
        }
        res
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid password".to_string(),
            }),
        )
            .into_response()
    }
}

pub async fn list_sessions_handler(State(state): State<WebUiState>) -> Response {
    let session_dir = state.session_dir.clone();
    let summaries_res = tokio::task::spawn_blocking(move || {
        if !session_dir.exists() {
            let _ = std::fs::create_dir_all(&session_dir);
        }

        let mut summaries = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(data) = std::fs::read_to_string(&path) {
                        if let Ok(detail) = serde_json::from_str::<SessionDetailResponse>(&data) {
                            summaries.push(SessionSummary {
                                id: detail.id,
                                title: detail.title,
                                created_at: detail.created_at,
                                updated_at: detail.updated_at,
                                message_count: detail.messages.len(),
                            });
                        }
                    }
                }
            }
        }
        summaries.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        summaries
    })
    .await;

    match summaries_res {
        Ok(summaries) => Json(SessionsListResponse {
            sessions: summaries,
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to read sessions: {e}"),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SessionQuery {
    pub session_id: String,
}

pub async fn get_session_handler(
    State(state): State<WebUiState>,
    Query(query): Query<SessionQuery>,
) -> Response {
    if !SESSION_ID_REGEX.is_match(&query.session_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid session_id format".to_string(),
            }),
        )
            .into_response();
    }

    let lock = state.get_session_lock(&query.session_id).await;
    let _guard = lock.lock().await;

    let path = state.session_dir.join(format!("{}.json", query.session_id));
    let read_res = tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Err((StatusCode::NOT_FOUND, "Session not found".to_string()));
        }
        match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<SessionDetailResponse>(&data) {
                Ok(detail) => Ok(detail),
                Err(e) => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to parse session file: {e}"),
                )),
            },
            Err(e) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read session file: {e}"),
            )),
        }
    })
    .await;

    match read_res {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err((status, error))) => (status, Json(ErrorResponse { error })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Join error: {e}"),
            }),
        )
            .into_response(),
    }
}

pub async fn new_session_handler(
    State(state): State<WebUiState>,
    Json(req): Json<NewSessionRequest>,
) -> Response {
    let token = match state.generate_token() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Entropy generation error: {e}"),
                }),
            )
                .into_response()
        }
    };
    let id = format!("sess_{}", &token[..12]);
    let title = req.title.unwrap_or_else(|| "New Session".to_string());
    let now = now_epoch_secs();

    let detail = SessionDetailResponse {
        id: id.clone(),
        title: title.clone(),
        created_at: now,
        updated_at: now,
        messages: Vec::new(),
    };

    let lock = state.get_session_lock(&id).await;
    let _guard = lock.lock().await;

    let path = state.session_dir.join(format!("{id}.json"));
    let fail_override = state.persistence_failure_override.load(Ordering::Relaxed);
    let write_res = tokio::task::spawn_blocking(move || {
        if fail_override {
            return Err("Forced persistence failure".to_string());
        }
        crate::persist::write_json(&path, &detail).map_err(|e| e.to_string())
    })
    .await;

    match write_res {
        Ok(Ok(_)) => (StatusCode::OK, Json(NewSessionResponse { id, title })).into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to persist new session".to_string(),
            }),
        )
            .into_response(),
    }
}

pub async fn rename_session_handler(
    State(state): State<WebUiState>,
    Json(req): Json<RenameSessionRequest>,
) -> Response {
    if !SESSION_ID_REGEX.is_match(&req.session_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid session_id format".to_string(),
            }),
        )
            .into_response();
    }

    let lock = state.get_session_lock(&req.session_id).await;
    let _guard = lock.lock().await;

    let path = state.session_dir.join(format!("{}.json", req.session_id));
    let fail_override = state.persistence_failure_override.load(Ordering::Relaxed);
    let res = tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Err((StatusCode::NOT_FOUND, "Session not found".to_string()));
        }
        if fail_override {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update session".to_string(),
            ));
        }
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(mut detail) = serde_json::from_str::<SessionDetailResponse>(&data) {
                detail.title = req.title;
                detail.updated_at = now_epoch_secs();
                if crate::persist::write_json(&path, &detail).is_ok() {
                    return Ok(());
                }
            }
        }
        Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update session".to_string(),
        ))
    })
    .await;

    match res {
        Ok(Ok(_)) => Json(StandardStatusResponse {
            status: "ok".to_string(),
        })
        .into_response(),
        Ok(Err((code, err))) => (code, Json(ErrorResponse { error: err })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Join error: {e}"),
            }),
        )
            .into_response(),
    }
}

pub async fn delete_session_handler(
    State(state): State<WebUiState>,
    Json(req): Json<DeleteSessionRequest>,
) -> Response {
    if !SESSION_ID_REGEX.is_match(&req.session_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid session_id format".to_string(),
            }),
        )
            .into_response();
    }

    let lock = state.get_session_lock(&req.session_id).await;
    let _guard = lock.lock().await;

    let path = state.session_dir.join(format!("{}.json", req.session_id));
    let del_res = tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Err(StatusCode::NOT_FOUND);
        }
        let _ = std::fs::remove_file(path);
        Ok(())
    })
    .await;

    match del_res {
        Ok(Ok(_)) => Json(StandardStatusResponse {
            status: "ok".to_string(),
        })
        .into_response(),
        Ok(Err(code)) => (
            code,
            Json(ErrorResponse {
                error: "Session not found".to_string(),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Join error: {e}"),
            }),
        )
            .into_response(),
    }
}

pub async fn chat_start_handler(
    State(state): State<WebUiState>,
    Json(req): Json<ChatStartRequest>,
) -> Response {
    if !SESSION_ID_REGEX.is_match(&req.session_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid session_id format".to_string(),
            }),
        )
            .into_response();
    }

    let (steer_tx, steer_rx) = mpsc::channel(16);
    let (event_tx, mut event_rx) = mpsc::channel(64);

    // Evict expired streams and atomically reserve stream slot under write lock
    let (stream_id, active_stream) = {
        let now_override = state.clock_override.as_ref().map(|c| c());
        evict_expired_completed_streams(&state.active_streams, now_override).await;

        let mut streams = state.active_streams.write().await;
        let running_count = streams.values().filter(|s| s.is_running()).count();
        if running_count >= MAX_CONCURRENT_RUNNING_STREAMS {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse {
                    error: "Active stream limit reached".to_string(),
                }),
            )
                .into_response();
        }

        let token = match state.generate_token() {
            Ok(t) => t,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "Entropy generation failed".to_string(),
                    }),
                )
                    .into_response();
            }
        };
        let stream_id = format!("stream_{}", &token[..16]);
        let active_stream = Arc::new(ActiveStream::new(
            stream_id.clone(),
            req.session_id.clone(),
            steer_tx,
        ));
        streams.insert(stream_id.clone(), active_stream.clone());
        (stream_id, active_stream)
    };

    // Persist user message immediately under session lock with rollback on failure
    let path = state.session_dir.join(format!("{}.json", req.session_id));
    let fail_override = state.persistence_failure_override.load(Ordering::Relaxed);
    let user_msg_str = req.message.clone();
    let session_id_str = req.session_id.clone();

    let persist_user_res = {
        let lock = state.get_session_lock(&req.session_id).await;
        let _guard = lock.lock().await;

        tokio::task::spawn_blocking(move || {
            if fail_override {
                return Err("Forced persistence failure".to_string());
            }

            let mut detail = if path.exists() {
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|d| serde_json::from_str::<SessionDetailResponse>(&d).ok())
                    .unwrap_or_else(|| SessionDetailResponse {
                        id: session_id_str.clone(),
                        title: "Session".to_string(),
                        created_at: now_epoch_secs(),
                        updated_at: now_epoch_secs(),
                        messages: Vec::new(),
                    })
            } else {
                SessionDetailResponse {
                    id: session_id_str.clone(),
                    title: "Session".to_string(),
                    created_at: now_epoch_secs(),
                    updated_at: now_epoch_secs(),
                    messages: Vec::new(),
                }
            };

            detail.messages.push(SessionMessageDto {
                role: "user".to_string(),
                content: user_msg_str,
                timestamp: now_epoch_secs(),
            });
            detail.updated_at = now_epoch_secs();

            crate::persist::write_json(&path, &detail).map_err(|e| e.to_string())?;

            let history = detail
                .messages
                .iter()
                .map(|m| match m.role.as_str() {
                    "user" => Message::user(&m.content),
                    "system" => Message::system(&m.content),
                    _ => Message::assistant(&m.content),
                })
                .collect::<Vec<_>>();
            Ok(history)
        })
        .await
    };

    let history = match persist_user_res {
        Ok(Ok(h)) => h,
        _ => {
            // Remove stream slot on startup persistence failure
            let mut streams = state.active_streams.write().await;
            streams.remove(&stream_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Failed to persist user message".to_string(),
                }),
            )
                .into_response();
        }
    };

    let runner = state.agent_runner.clone();
    let session_id_clone = req.session_id.clone();
    let user_msg = req.message.clone();
    let model_req = req.model.clone();
    let stream_clone = active_stream.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        let stream_event_clone = stream_clone.clone();
        let event_pump = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                for (evt, data) in agent_event_to_unsequenced_frames(&event) {
                    stream_event_clone.buffer_raw_frame(evt, data).await;
                }
            }
        });

        let outcome = runner
            .run_turn(
                &session_id_clone,
                &user_msg,
                model_req.as_deref(),
                history,
                event_tx,
                stream_clone.cancel_flag.clone(),
                steer_rx,
            )
            .await;

        let _ = event_pump.await;

        match outcome {
            Ok(content) => {
                let fail_save = state_clone
                    .persistence_failure_override
                    .load(Ordering::Relaxed);
                let lock = state_clone.get_session_lock(&session_id_clone).await;
                let _guard = lock.lock().await;
                let sess_path = state_clone
                    .session_dir
                    .join(format!("{session_id_clone}.json"));
                let sess_clone = session_id_clone.clone();
                let save_content = content.clone();

                let save_res = tokio::task::spawn_blocking(move || {
                    if fail_save {
                        return Err("Forced assistant save failure".to_string());
                    }
                    if let Ok(data) = std::fs::read_to_string(&sess_path) {
                        if let Ok(mut detail) = serde_json::from_str::<SessionDetailResponse>(&data)
                        {
                            detail.messages.push(SessionMessageDto {
                                role: "assistant".to_string(),
                                content: save_content,
                                timestamp: now_epoch_secs(),
                            });
                            detail.updated_at = now_epoch_secs();
                            return crate::persist::write_json(&sess_path, &detail)
                                .map_err(|e| e.to_string());
                        }
                    }
                    Err(format!("Failed to read session {sess_clone}"))
                })
                .await;

                if let Ok(Err(e)) = save_res {
                    error!("Failed to persist assistant message: {e}");
                    stream_clone
                        .buffer_raw_frame(
                            "error".to_string(),
                            serde_json::json!({ "error": format!("Persistence failure: {e}") })
                                .to_string(),
                        )
                        .await;
                }

                if stream_clone.try_transition_terminal(STATE_COMPLETED) {
                    stream_clone
                        .buffer_raw_frame(
                            "done".to_string(),
                            serde_json::json!({ "message": content }).to_string(),
                        )
                        .await;
                }
            }
            Err(e) => {
                error!("Agent turn error: {e}");
                if stream_clone.cancel_flag.load(Ordering::Relaxed) {
                    if stream_clone.try_transition_terminal(STATE_CANCELLED) {
                        stream_clone
                            .buffer_raw_frame("cancelled".to_string(), "{}".to_string())
                            .await;
                    }
                } else if stream_clone.try_transition_terminal(STATE_ERRORED) {
                    stream_clone
                        .buffer_raw_frame(
                            "error".to_string(),
                            serde_json::json!({ "error": e.to_string() }).to_string(),
                        )
                        .await;
                }
            }
        }

        stream_clone.finalize_stream().await;
    });

    (
        StatusCode::OK,
        Json(ChatStartResponse {
            stream_id,
            session_id: req.session_id,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct StreamQuery {
    pub stream_id: String,
}

pub async fn chat_stream_handler(
    State(state): State<WebUiState>,
    Query(query): Query<StreamQuery>,
) -> Response {
    let now_override = state.clock_override.as_ref().map(|c| c());
    evict_expired_completed_streams(&state.active_streams, now_override).await;

    let stream_opt = {
        let streams = state.active_streams.read().await;
        streams.get(&query.stream_id).cloned()
    };

    let stream = match stream_opt {
        Some(s) => {
            if s.is_expired().await {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        error: "Stream not found or expired".to_string(),
                    }),
                )
                    .into_response();
            }
            s
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Stream not found or expired".to_string(),
                }),
            )
                .into_response();
        }
    };

    let stream_events = async_stream::stream! {
        let mut last_seen_seq = 0;
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));
        heartbeat_interval.reset();

        loop {
            let frames = {
                let buf = stream.buffered_events.read().await;
                buf.iter()
                    .filter(|f| f.seq > last_seen_seq)
                    .cloned()
                    .collect::<Vec<_>>()
            };

            for frame in frames {
                if frame.seq > last_seen_seq {
                    last_seen_seq = frame.seq;
                }
                yield Ok::<Event, Infallible>(Event::default().event(frame.event).data(frame.data));
            }

            let is_running = stream.is_running();
            if !is_running {
                let final_frames = {
                    let buf = stream.buffered_events.read().await;
                    buf.iter()
                        .filter(|f| f.seq > last_seen_seq)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                for frame in final_frames {
                    if frame.seq > last_seen_seq {
                        last_seen_seq = frame.seq;
                    }
                    yield Ok::<Event, Infallible>(Event::default().event(frame.event).data(frame.data));
                }

                yield Ok::<Event, Infallible>(Event::default().event("streamEnd").data("{}"));
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = heartbeat_interval.tick() => {
                    yield Ok::<Event, Infallible>(Event::default().event("heartbeat").data("{}"));
                }
            }
        }
    };

    Sse::new(stream_events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
        .into_response()
}

pub async fn chat_cancel_handler(
    State(state): State<WebUiState>,
    Json(req): Json<ChatCancelRequest>,
) -> Response {
    let stream_opt = {
        let streams = state.active_streams.read().await;
        streams.get(&req.stream_id).cloned()
    };

    if let Some(stream) = stream_opt {
        stream.cancel_flag.store(true, Ordering::SeqCst);
        if stream.try_transition_terminal(STATE_CANCELLED) {
            stream
                .buffer_raw_frame("cancelled".to_string(), "{}".to_string())
                .await;
        }

        // Spawn watchdog to ensure finalization after at most 5s join deadline
        let stream_watchdog = stream.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if stream_watchdog.is_running() {
                let _ = stream_watchdog.try_transition_terminal(STATE_CANCELLED);
            }
            stream_watchdog.finalize_stream().await;
        });

        return Json(StandardStatusResponse {
            status: "cancelled".to_string(),
        })
        .into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: "Stream not found".to_string(),
        }),
    )
        .into_response()
}

pub async fn chat_steer_handler(
    State(state): State<WebUiState>,
    Json(req): Json<ChatSteerRequest>,
) -> Response {
    let stream_opt = {
        let streams = state.active_streams.read().await;
        streams.get(&req.stream_id).cloned()
    };

    if let Some(stream) = stream_opt {
        if stream.is_running() {
            let tx_opt = {
                let guard = stream.steer_tx.lock().await;
                guard.clone()
            };
            if let Some(tx) = tx_opt {
                if tx.send(req.message).await.is_ok() {
                    return Json(StandardStatusResponse {
                        status: "ok".to_string(),
                    })
                    .into_response();
                }
            }
        }
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Stream is not active".to_string(),
            }),
        )
            .into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: "Stream not found".to_string(),
        }),
    )
        .into_response()
}

pub async fn workspaces_handler(State(state): State<WebUiState>) -> impl IntoResponse {
    Json(WorkspacesResponse {
        workspaces: vec![WorkspaceItem {
            path: state.workspace_root.to_string_lossy().to_string(),
            name: "workspace".to_string(),
            active: true,
        }],
    })
}

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
}

pub async fn list_directory_handler(
    State(state): State<WebUiState>,
    Query(query): Query<PathQuery>,
) -> Response {
    let sub = query.path.unwrap_or_default();
    let ws_root = state.workspace_root.clone();

    let list_res = tokio::task::spawn_blocking(move || {
        if sub.contains("..") {
            return Err((
                StatusCode::FORBIDDEN,
                "Access denied: Path escapes workspace root".to_string(),
            ));
        }

        let target = ws_root.join(&sub);
        let canonical = match dunce_canonicalize(&target) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Err((StatusCode::NOT_FOUND, "Directory not found".to_string()));
                }
                return Err((
                    StatusCode::FORBIDDEN,
                    "Access denied: Path escapes workspace root".to_string(),
                ));
            }
        };

        let ws_canonical = match dunce_canonicalize(&ws_root) {
            Ok(c) => c,
            Err(_) => ws_root.clone(),
        };

        if canonical.strip_prefix(&ws_canonical).is_err() {
            return Err((
                StatusCode::FORBIDDEN,
                "Access denied: Path escapes workspace root".to_string(),
            ));
        }

        let mut items = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&canonical) {
            for entry in entries.flatten() {
                let p = entry.path();
                let is_dir = p.is_dir();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let name = entry.file_name().to_string_lossy().to_string();
                let rel_path = p
                    .strip_prefix(&ws_canonical)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string();
                items.push(DirectoryEntryDto {
                    name,
                    path: rel_path,
                    is_dir,
                    size,
                });
            }
        }

        items.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(items)
    })
    .await;

    match list_res {
        Ok(Ok(items)) => Json(DirectoryListResponse { items }).into_response(),
        Ok(Err((status, error))) => (status, Json(ErrorResponse { error })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Join error: {e}"),
            }),
        )
            .into_response(),
    }
}

pub async fn read_file_handler(
    State(state): State<WebUiState>,
    Query(query): Query<PathQuery>,
) -> Response {
    let sub = match query.path {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Missing path parameter".to_string(),
                }),
            )
                .into_response()
        }
    };

    let ws_root = state.workspace_root.clone();
    let sub_clone = sub.clone();

    let file_res = tokio::task::spawn_blocking(move || {
        if sub_clone.contains("..") {
            return Err((
                StatusCode::FORBIDDEN,
                "Access denied: Path escapes workspace root".to_string(),
            ));
        }

        let target = ws_root.join(&sub_clone);
        let canonical = match dunce_canonicalize(&target) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Err((StatusCode::NOT_FOUND, "File not found".to_string()));
                }
                return Err((
                    StatusCode::FORBIDDEN,
                    "Access denied: Path escapes workspace root".to_string(),
                ));
            }
        };

        let ws_canonical = match dunce_canonicalize(&ws_root) {
            Ok(c) => c,
            Err(_) => ws_root.clone(),
        };

        if canonical.strip_prefix(&ws_canonical).is_err() {
            return Err((
                StatusCode::FORBIDDEN,
                "Access denied: Path escapes workspace root".to_string(),
            ));
        }

        if let Ok(meta) = std::fs::metadata(&canonical) {
            if meta.len() > MAX_FILE_SIZE_BYTES {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "File size exceeds 10MB limit".to_string(),
                ));
            }
        }

        match std::fs::read_to_string(&canonical) {
            Ok(content) => Ok(content),
            Err(e) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )),
        }
    })
    .await;

    match file_res {
        Ok(Ok(content)) => Json(FileContentResponse { path: sub, content }).into_response(),
        Ok(Err((status, error))) => (status, Json(ErrorResponse { error })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Join error: {e}"),
            }),
        )
            .into_response(),
    }
}

pub async fn models_handler(State(state): State<WebUiState>) -> impl IntoResponse {
    let cfg = state.runtime_config.read().await;
    let main_model = cfg.agent.model.clone();
    let provider = cfg.client.provider.clone();

    let mut models = vec![ModelItem {
        id: main_model.clone(),
        name: main_model.clone(),
        provider,
    }];

    for fb in &cfg.client.fallback {
        if let Some(ref m) = fb.model {
            models.push(ModelItem {
                id: m.clone(),
                name: m.clone(),
                provider: fb.provider.clone(),
            });
        }
    }

    Json(ModelsResponse {
        models,
        default: main_model,
    })
}

pub async fn default_model_handler(State(state): State<WebUiState>) -> impl IntoResponse {
    let cfg = state.runtime_config.read().await;
    Json(DefaultModelResponse {
        model: cfg.agent.model.clone(),
    })
}

pub async fn settings_handler(State(state): State<WebUiState>) -> impl IntoResponse {
    Json(SettingsResponse {
        version: "0.4.0".to_string(),
        webui: WebUiSettingsDto {
            auth_enabled: state.auth_password.is_some(),
        },
    })
}

// Helpers
fn dunce_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

pub async fn verify_auth_headers(state: &WebUiState, headers: &HeaderMap) -> bool {
    let current_token = state.auth_token.read().await.clone();
    let Some(valid_token) = current_token else {
        return false;
    };

    if let Some(auth_header) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
    {
        if let Some(bearer) = auth_header.strip_prefix("Bearer ") {
            if constant_time_eq(bearer.trim().as_bytes(), valid_token.as_bytes()) {
                return true;
            }
        }
    }

    if let Some(cookie_header) = headers.get(header::COOKIE).and_then(|h| h.to_str().ok()) {
        for cookie in cookie_header.split(';') {
            let cookie = cookie.trim();
            if let Some(val) = cookie.strip_prefix("hermes_auth=") {
                if constant_time_eq(val.trim().as_bytes(), valid_token.as_bytes()) {
                    return true;
                }
            }
        }
    }

    false
}
