# Tasks: Hermes WebUI / Hermex iOS Compatibility API Layer

## Implementation Tasks

- [x] T1: Data models and DTOs (`crates/kerux-core/src/webui/models.rs`)
  - Define serde models for:
    - Auth: `AuthStatusResponse`, `LoginRequest`, `LoginResponse`
    - Sessions: `SessionSummary`, `SessionsListResponse`, `SessionDetailResponse`, `SessionMessageDto`, `NewSessionRequest`, `NewSessionResponse`, `RenameSessionRequest`, `DeleteSessionRequest`, `StandardStatusResponse`, `ErrorResponse`
    - Chat: `ChatStartRequest`, `ChatStartResponse`, `ChatCancelRequest`, `ChatSteerRequest`, `MeteringPayload`
    - Workspaces & Files: `WorkspacesResponse`, `WorkspaceItem`, `DirectoryListResponse`, `DirectoryEntryDto`, `FileContentResponse`
    - Models & Settings: `ModelsResponse`, `ModelItem`, `DefaultModelResponse`, `SettingsResponse`, `WebUiSettingsDto`
  - Implement exact Hermex wire-level serialization tags.
- [x] T2: SSE Event Streaming Converter, Replay Buffer & AgentRunner Trait (`crates/kerux-core/src/webui/stream.rs`)
  - Define `AgentRunner` trait:
    ```rust
    #[async_trait::async_trait]
    pub trait AgentRunner: Send + Sync {
        async fn run_turn(
            &self,
            session_id: &str,
            user_message: &str,
            model: Option<&str>,
            history: Vec<crate::client::Message>,
            event_tx: tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
            cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
            steer_rx: tokio::sync::mpsc::Receiver<String>,
        ) -> crate::error::Result<String>;
    }
    ```
  - Implement `LiveAgentRunner` and `MockAgentRunner`.
  - Define `ActiveStream` struct with:
    - `cancel_flag: Arc<AtomicBool>`
    - `steer_tx: mpsc::Sender<String>`
    - Bounded event history (up to 100 events, max 64KiB per event chunk, 5 min TTL)
  - Implement active stream concurrency ceiling (32 slots) returning HTTP 429 when exhausted.
  - Bridge `AgentEvent` variants:
    - `Content` -> `token`
    - `Reasoning`/`Thinking` -> `reasoning`
    - `ToolStart` -> `toolStarted` (arguments as JSON object)
    - `ToolComplete` -> `toolCompleted` (success: true, content)
    - `ToolError` -> `toolCompleted` (success: false, error)
    - `Telemetry` -> `metering` (`prompt_tokens`, `completion_tokens`, `total_tokens`)
    - `Error` -> `error`
    - `Done` -> `done`
    - Periodic 5-second `heartbeat`
    - Stream termination `streamEnd` (exactly once on stream close)
- [x] T3: REST API Handlers, Path Confinement & Serialized Session Persistence (`crates/kerux-core/src/webui/handlers.rs`)
  - Implement `/health`, `/api/auth/status`, `/api/auth/login`.
  - Implement `/api/sessions`, `/api/session`, `/api/session/new`, `/api/session/rename`, `/api/session/delete` with `^[a-zA-Z0-9_-]{1,64}$` validation, per-session mutex locking in `session_locks`, and atomic disk persistence under `{session_dir}/{session_id}.json` (returning 404 for missing sessions).
  - Implement `/api/chat/start`, `/api/chat/stream`, `/api/chat/cancel`, `/api/chat/steer`:
    - Append user message on chat start, append assistant response on turn completion (under per-session lock).
    - Pass optional requested `model` to `AgentRunner::run_turn`.
    - Return 404 for unknown/expired stream IDs on `/api/chat/stream`.
    - Handle steering delivery or return 400 if stream is not active.
  - Implement `/api/workspaces`, `/api/list` (`DirectoryListResponse`), `/api/file` with strict canonicalization against `workspace_root` (10MB limit with HTTP 413, traversal with 403).
  - Implement dynamic `/api/models`, `/api/default-model`, `/api/settings`.
- [x] T4: Router, CORS Ordering and WebUI Server Integration (`crates/kerux-core/src/webui/mod.rs`)
  - Construct `axum::Router` with:
    - Request body limit: 2MB (`DefaultBodyLimit::max(2 * 1024 * 1024)`).
    - CORS: methods `[GET, POST, OPTIONS]`, headers `[Authorization, Content-Type, Accept, Cache-Control]`, allowed origins `Any` (credentialed cross-origin disabled).
    - Ensure CORS middleware handles `OPTIONS` requests before the auth middleware to prevent 401 on preflight.
    - Constant-time auth middleware with 32-byte secure random tokens protecting non-public routes.
  - Export `serve_webui(state: WebUiState, addr: SocketAddr)` function.
- [x] T5: Configuration and Gateway Integration (`crates/kerux-core/src/config.rs`, `crates/kerux-core/src/gateway.rs`, `kerux.example.toml`)
  - Add `#[serde(default)]` `webui_enabled: bool`, `#[serde(default = "default_webui_addr")]` `webui_addr: String`, `#[serde(default)]` `webui_password: String` to `GatewaySettings` in `config.rs`.
  - Add `webui_enabled: bool`, `webui_addr: String`, `webui_password: String` to `GatewayConfig` in `gateway.rs`.
  - Update `Gateway::run()` to parse `webui_addr` to `SocketAddr`, check `addr.ip().is_loopback()`, refuse remote non-loopback binding without a password (`Error::Config`), and spawn the WebUI server task when `config.webui_enabled` is true.
  - Add env overrides: `KERUX_WEBUI_PASSWORD` (primary, if non-empty) and `HERMES_WEBUI_PASSWORD` (fallback).
- [x] T6: Isolated Integration Tests (`crates/kerux-core/tests/webui_compat.rs`)
  - Unit/integration tests using `tempfile::tempdir()`, `MockAgentRunner`, and `tower::ServiceExt::oneshot`:
    - Health & Auth status (with and without password, and with bearer/cookie)
    - CORS preflight `OPTIONS` request succeeds without auth
    - Login with valid and invalid password
    - Route protection enforcement (401 on unauthorized access)
    - Session CRUD (new, list, get, rename, delete, 404 on missing) with ID sanitization
    - Concurrent session update test asserting no lost messages (serialized lock check)
    - Dynamic `/api/models`, `/api/default-model`, and `/api/settings` response verification
    - Path traversal rejection (HTTP 403) on `/api/list` and `/api/file`
    - 10MB file limit enforcement returning 413
    - Chat start and session user message persistence with custom model parameter passed to runner
    - SSE stream event delivery (`token`, `reasoning`, `toolStarted`, `toolCompleted` success/failure, `metering`, `error`, `done`, `heartbeat`, `streamEnd`), replay, and 404 for unknown stream
    - Stream completion assistant message persistence into session file
    - Steering message delivery to active stream, and 400 error on completed stream
    - Concurrency slot release upon turn completion, turn error, and cancellation
    - 32-stream concurrency limit enforcement (HTTP 429)
    - 64KiB per-event text chunk splitting
    - Password environment variable precedence (`KERUX_WEBUI_PASSWORD` over `HERMES_WEBUI_PASSWORD`)
    - Remote non-loopback binding refusal when password is empty

## Verification
- Run `openspec validate --changes`
- Run `cargo fmt --all`
- Run `cargo check --workspace`
- Run `cargo test --test webui_compat`
- Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`
