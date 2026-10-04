use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

use kerux_core::agent::AgentEvent;
use kerux_core::config::AppConfig;
use kerux_core::gateway::GatewayConfig;
use kerux_core::webui::{
    agent_event_to_unsequenced_frames, build_router, ActiveStream, AgentRunner, AuthStatusResponse,
    ChatCancelRequest, ChatStartRequest, ChatStartResponse, ChatSteerRequest, DeleteSessionRequest,
    DirectoryListResponse, FileContentResponse, LoginRequest, LoginResponse, MockAgentRunner,
    ModelsResponse, NewSessionRequest, NewSessionResponse, RenameSessionRequest,
    SessionDetailResponse, SessionsListResponse, WebUiState, MAX_CONCURRENT_RUNNING_STREAMS,
    MAX_EVENTS_PER_STREAM, MAX_WIRE_FRAME_BYTES,
};

fn create_test_state(
    auth_password: Option<String>,
    runner: Arc<dyn AgentRunner>,
    workspace_dir: std::path::PathBuf,
    session_dir: std::path::PathBuf,
) -> WebUiState {
    let mut config = AppConfig::default();
    config.agent.model = "gpt-4o".to_string();
    config.client.provider = "openai".to_string();

    WebUiState::with_runner_and_dir(
        Arc::new(tokio::sync::RwLock::new(config)),
        auth_password,
        workspace_dir,
        session_dir,
        runner,
    )
}

#[tokio::test]
async fn test_health_and_auth_status_without_password() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    // GET /health
    let req = Request::builder()
        .uri("/health")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(val["status"], "ok");

    // GET /api/auth/status
    let req = Request::builder()
        .uri("/api/auth/status")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let auth_status: AuthStatusResponse = serde_json::from_slice(&body).unwrap();
    assert!(auth_status.authenticated);
    assert_eq!(auth_status.profile, "default");
    assert_eq!(auth_status.version, "0.4.0");
}

#[tokio::test]
async fn test_auth_status_and_login_with_password() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(
        Some("secret123".into()),
        runner,
        ws_temp.path().into(),
        sess_temp.path().into(),
    );
    let app = build_router(state);

    // GET /api/auth/status without token -> authenticated: false
    let req = Request::builder()
        .uri("/api/auth/status")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let auth_status: AuthStatusResponse = serde_json::from_slice(&body).unwrap();
    assert!(!auth_status.authenticated);

    // Invalid login -> 401
    let login_req = LoginRequest {
        password: Some("wrong".into()),
        username: None,
    };
    let req = Request::builder()
        .uri("/api/auth/login")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&login_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Valid login -> 200 + token + cookie
    let login_req = LoginRequest {
        password: Some("secret123".into()),
        username: None,
    };
    let req = Request::builder()
        .uri("/api/auth/login")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&login_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let cookie_hdr = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie_hdr.contains("hermes_auth="));
    assert!(cookie_hdr.contains("HttpOnly"));

    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let login_resp: LoginResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(login_resp.status, "ok");
    assert!(!login_resp.token.is_empty());

    // GET /api/auth/status with Bearer token -> authenticated: true
    let req = Request::builder()
        .uri("/api/auth/status")
        .method(Method::GET)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", login_resp.token),
        )
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let auth_status: AuthStatusResponse = serde_json::from_slice(&body).unwrap();
    assert!(auth_status.authenticated);
}

#[tokio::test]
async fn test_entropy_failure_fails_closed() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let mut state = create_test_state(
        Some("secret123".into()),
        runner,
        ws_temp.path().into(),
        sess_temp.path().into(),
    );
    state.token_generator = Some(Arc::new(|| {
        Err(kerux_core::error::Error::Agent(
            "Entropy failure injection".to_string(),
        ))
    }));
    let app = build_router(state);

    let login_req = LoginRequest {
        password: Some("secret123".into()),
        username: None,
    };
    let req = Request::builder()
        .uri("/api/auth/login")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&login_req).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_protected_routes_require_auth() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(
        Some("pwd".into()),
        runner,
        ws_temp.path().into(),
        sess_temp.path().into(),
    );
    let app = build_router(state);

    let protected_uris = vec![
        ("/api/sessions", Method::GET),
        ("/api/session?session_id=sess_1", Method::GET),
        ("/api/session/new", Method::POST),
        ("/api/chat/start", Method::POST),
        ("/api/workspaces", Method::GET),
        ("/api/list", Method::GET),
        ("/api/file?path=foo", Method::GET),
        ("/api/models", Method::GET),
        ("/api/models/default", Method::GET),
        ("/api/settings", Method::GET),
    ];

    for (uri, method) in protected_uris {
        let req = Request::builder()
            .uri(uri)
            .method(method)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "URI {uri} should be protected"
        );
    }
}

#[tokio::test]
async fn test_sessions_crud_and_atomic_persistence() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    // 1. Create new session
    let new_req = NewSessionRequest {
        title: Some("My Test Session".into()),
    };
    let req = Request::builder()
        .uri("/api/session/new")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&new_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let new_res: NewSessionResponse = serde_json::from_slice(&body).unwrap();
    let sess_id = new_res.id;
    assert_eq!(new_res.title, "My Test Session");

    // 2. Get session
    let req = Request::builder()
        .uri(format!("/api/session?session_id={sess_id}"))
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let detail: SessionDetailResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(detail.id, sess_id);
    assert_eq!(detail.title, "My Test Session");

    // 3. Rename session
    let rename_req = RenameSessionRequest {
        session_id: sess_id.clone(),
        title: "Renamed Title".into(),
    };
    let req = Request::builder()
        .uri("/api/session/rename")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&rename_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // 4. List sessions
    let req = Request::builder()
        .uri("/api/sessions")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let list_res: SessionsListResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(list_res.sessions.len(), 1);
    assert_eq!(list_res.sessions[0].title, "Renamed Title");

    // 5. Delete session
    let del_req = DeleteSessionRequest {
        session_id: sess_id.clone(),
    };
    let req = Request::builder()
        .uri("/api/session/delete")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&del_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // 6. Delete again -> 404
    let req = Request::builder()
        .uri("/api/session/delete")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&del_req).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_chat_start_and_concurrency_limits() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();

    let runner = Arc::new(MockAgentRunner::with_delay(
        vec![AgentEvent::Content {
            text: "Hello from agent".into(),
        }],
        "Hello from agent".into(),
        std::time::Duration::from_millis(500),
    ));

    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    // Fill up to 32 running streams
    for i in 0..MAX_CONCURRENT_RUNNING_STREAMS {
        let chat_req = ChatStartRequest {
            session_id: format!("sess_{i}"),
            message: "Hello".into(),
            model: None,
        };
        let req = Request::builder()
            .uri("/api/chat/start")
            .method(Method::POST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&chat_req).unwrap()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "Stream {i} should be accepted"
        );
    }

    // 33rd stream should be rejected with 429 Too Many Requests
    let chat_req = ChatStartRequest {
        session_id: "sess_overflow".into(),
        message: "Hello".into(),
        model: None,
    };
    let req = Request::builder()
        .uri("/api/chat/start")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&chat_req).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_chat_cancel_and_terminal_events() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();

    let runner = Arc::new(MockAgentRunner::with_delay(
        vec![],
        "completed".into(),
        std::time::Duration::from_secs(2),
    ));

    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    let chat_req = ChatStartRequest {
        session_id: "sess_cancel".into(),
        message: "Run a long task".into(),
        model: None,
    };
    let req = Request::builder()
        .uri("/api/chat/start")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&chat_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let start_res: ChatStartResponse = serde_json::from_slice(&body).unwrap();
    let stream_id = start_res.stream_id;

    // Cancel the stream
    let cancel_req = ChatCancelRequest {
        stream_id: stream_id.clone(),
    };
    let req = Request::builder()
        .uri("/api/chat/cancel")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&cancel_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Read SSE stream and verify cancelled event and streamEnd
    let req = Request::builder()
        .uri(format!("/api/chat/stream?stream_id={stream_id}"))
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let sse_str = String::from_utf8_lossy(&body);

    assert!(sse_str.contains("event: cancelled"));
    assert!(sse_str.contains("event: streamEnd"));
}

#[tokio::test]
async fn test_chat_steer_flow() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();

    let runner = Arc::new(MockAgentRunner::with_delay(
        vec![],
        "steered".into(),
        std::time::Duration::from_millis(500),
    ));

    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    let chat_req = ChatStartRequest {
        session_id: "sess_steer".into(),
        message: "Initial task".into(),
        model: None,
    };
    let req = Request::builder()
        .uri("/api/chat/start")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&chat_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let start_res: ChatStartResponse = serde_json::from_slice(&body).unwrap();
    let stream_id = start_res.stream_id;

    // Send steer message while running
    let steer_req = ChatSteerRequest {
        stream_id: stream_id.clone(),
        message: "Change direction!".into(),
    };
    let req = Request::builder()
        .uri("/api/chat/steer")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&steer_req).unwrap()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Wait for stream to finish
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    // Steer after stream finished -> 400 Bad Request
    let req = Request::builder()
        .uri("/api/chat/steer")
        .method(Method::POST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&steer_req).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_sse_wire_frame_bounding_and_chunking() {
    // 1. Test streaming content chunking
    let huge_content = "A".repeat(150_000);
    let event = AgentEvent::Content {
        text: huge_content.clone(),
    };
    let frames = agent_event_to_unsequenced_frames(&event);

    assert!(frames.len() >= 3);
    let mut reconstructed = String::new();
    for (evt, data) in &frames {
        assert_eq!(evt, "token");
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        let chunk = parsed["content"].as_str().unwrap();
        reconstructed.push_str(chunk);

        let wire_frame = format!("event: {evt}\ndata: {data}\n\n");
        assert!(
            wire_frame.len() <= MAX_WIRE_FRAME_BYTES,
            "Wire frame exceeds 64 KiB: {}",
            wire_frame.len()
        );
    }
    assert_eq!(reconstructed, huge_content);

    // 2. Test discrete tool payload bounding
    let huge_tool_result = "T".repeat(100_000);
    let event = AgentEvent::ToolComplete {
        result: kerux_core::tools::ToolResult::success(
            "call_1",
            serde_json::json!({ "output": huge_tool_result }),
        ),
    };
    let frames = agent_event_to_unsequenced_frames(&event);
    assert_eq!(frames.len(), 1);
    let (evt, data) = &frames[0];
    assert_eq!(evt, "toolCompleted");
    let wire_frame = format!("event: {evt}\ndata: {data}\n\n");
    assert!(
        wire_frame.len() <= MAX_WIRE_FRAME_BYTES,
        "Tool wire frame exceeds 64 KiB: {}",
        wire_frame.len()
    );
    assert!(data.contains("[TRUNCATED]"));
}

#[tokio::test]
async fn test_stream_replay_buffer_and_ttl_eviction() {
    let (steer_tx, _) = tokio::sync::mpsc::channel(1);
    let stream = Arc::new(ActiveStream::new(
        "stream_replay".into(),
        "sess_replay".into(),
        steer_tx,
    ));

    // Buffer 150 frames -> verify FIFO drops oldest so len <= 100
    for i in 1..=150 {
        stream
            .buffer_raw_frame("token".into(), format!(r#"{{"token":"chunk_{i}"}}"#))
            .await;
    }

    {
        let buf = stream.buffered_events.read().await;
        assert_eq!(buf.len(), MAX_EVENTS_PER_STREAM);
        assert_eq!(buf.first().unwrap().seq, 51);
        assert_eq!(buf.last().unwrap().seq, 150);
    }

    // Finalize stream and verify TTL check
    stream.finalize_stream().await;
    assert!(!stream.is_expired().await);
}

#[tokio::test]
async fn test_workspace_and_file_traversal_protection() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();

    let secret_file = ws_temp.path().join("hello.txt");
    std::fs::write(&secret_file, "hello kerux").unwrap();

    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    // 1. GET /api/workspaces
    let req = Request::builder()
        .uri("/api/workspaces")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // 2. GET /api/list (valid root)
    let req = Request::builder()
        .uri("/api/list")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let list_res: DirectoryListResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(list_res.items.len(), 1);
    assert_eq!(list_res.items[0].name, "hello.txt");

    // 3. GET /api/list with path traversal `..` -> 403 Forbidden
    let req = Request::builder()
        .uri("/api/list?path=../../etc")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // 4. GET /api/file (valid)
    let req = Request::builder()
        .uri("/api/file?path=hello.txt")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let file_res: FileContentResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(file_res.content, "hello kerux");

    // 5. GET /api/file with traversal `..` -> 403 Forbidden
    let req = Request::builder()
        .uri("/api/file?path=../../etc/passwd")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_cors_preflight_and_headers() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(
        Some("secret".into()),
        runner,
        ws_temp.path().into(),
        sess_temp.path().into(),
    );
    let app = build_router(state);

    // Preflight OPTIONS on protected route -> 204 No Content with CORS headers (no auth required)
    let req = Request::builder()
        .uri("/api/chat/start")
        .method(Method::OPTIONS)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
    assert!(res
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_METHODS)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("POST"));

    // 401 Unauthorized response also carries CORS headers
    let req = Request::builder()
        .uri("/api/models")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
}

#[tokio::test]
async fn test_model_routes_and_aliases() {
    let ws_temp = tempdir().unwrap();
    let sess_temp = tempdir().unwrap();
    let runner = Arc::new(MockAgentRunner::new(vec![], "ok".into()));
    let state = create_test_state(None, runner, ws_temp.path().into(), sess_temp.path().into());
    let app = build_router(state);

    // GET /api/models
    let req = Request::builder()
        .uri("/api/models")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let models_res: ModelsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(models_res.default, "gpt-4o");

    // GET /api/models/default
    let req = Request::builder()
        .uri("/api/models/default")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // GET /api/default-model alias
    let req = Request::builder()
        .uri("/api/default-model")
        .method(Method::GET)
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_gateway_refuses_remote_bind_without_password() {
    let config = GatewayConfig {
        webui_enabled: true,
        webui_addr: "0.0.0.0:8787".to_string(),
        webui_password: "".to_string(),
        ..Default::default()
    };

    let gateway = kerux_core::gateway::Gateway::new(config);

    let res = gateway.run().await;
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("WebUI remote non-loopback binding requires a non-empty password"),
        "Unexpected error: {err_msg}"
    );
}
