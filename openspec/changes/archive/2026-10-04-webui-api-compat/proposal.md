# Proposal: Hermes WebUI / Hermex iOS Compatibility API Layer

## Status
Proposed (Round 10 revisions addressing Reviewer A & B findings)

## Summary
Add a lightweight, Axum-based HTTP REST and Server-Sent Events (SSE) gateway adapter to Kerux (`kerux serve`), implementing the standard Hermes WebUI API protocol. This allows native iOS clients like [Hermex](https://github.com/uzairansaruzi/hermex) as well as web-based UI clients to connect directly to a self-hosted Kerux instance.

## Motivation
Kerux is a fast, self-contained AI agent runtime written in Rust. While it currently supports interactive terminal use (TUI) and messaging bridges (Telegram, Discord, Slack, WhatsApp), mobile and rich web interfaces cannot connect directly to it.
Hermex is an open-source native SwiftUI iOS app built specifically for the Hermes ecosystem. By adding a compatible REST + SSE server layer into `kerux serve`, users can use their iPhones to monitor, chat with, and control their self-hosted Kerux agent with zero additional Python dependencies.

## Architecture & State Management
- **Axum WebUI Subsystem (`crates/kerux-core/src/webui/`)**:
  - `WebUiState`: Contains `session_dir: PathBuf` (defaults to `crate::platform::kerux_home().join("sessions")`), `session_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>` (per-session mutex serialization for race-free persistence), `active_streams: Arc<RwLock<HashMap<String, ActiveStream>>>`, `auth_password: Option<String>`, `auth_token: Arc<RwLock<Option<String>>>`, `runtime_config: Arc<RwLock<Config>>`, `workspace_root: PathBuf`, and `agent_runner: Arc<dyn AgentRunner + Send + Sync>`.
  - **Pluggable AgentRunner Trait**:
    ```rust
    #[async_trait::async_trait]
    pub trait AgentRunner: Send + Sync {
        async fn run_turn(
            &self,
            session_id: &str,
            user_message: &str,
            model: Option<&str>,
            history: Vec<Message>,
            event_tx: mpsc::Sender<AgentEvent>,
            cancel: Arc<AtomicBool>,
            steer_rx: mpsc::Receiver<String>,
        ) -> Result<String>;
    }
    ```
    - `LiveAgentRunner` creates turn-isolated `KeruxAgent` instances with model override support.
    - `MockAgentRunner` provides deterministic, offline scripted events for tests.
  - **Single Owner Startup**: `Gateway::run()` in `kerux-core` is the single owner that parses `config.webui_addr` to `SocketAddr` and spawns the WebUI server task when `config.webui_enabled` is true. `kerux serve` invokes `gateway.run()`.
  - **Loopback & Transport Security Policy**:
    - `addr.ip().is_loopback()` is checked at startup. If non-loopback (e.g. `0.0.0.0`, `::`) and `webui_password` is empty, startup fails with `Error::Config("WebUI remote non-loopback binding requires a non-empty password")`.
    - Following Hermex guidelines, remote internet deployments are expected to terminate TLS via reverse proxy (Tailscale HTTPS, Cloudflare Tunnel, Caddy, nginx).
  - **Session Persistence & Serialized Locking**:
    - File format: `{session_dir}/{session_id}.json`.
    - Top-level schema: `{"id": String, "title": String, "created_at": u64, "updated_at": u64, "messages": [{"role": String, "content": String, "timestamp": u64}]}`.
    - Every read/write/append to a session acquires its keyed mutex in `session_locks`, ensuring atomic, race-free persistence across concurrent turns.
  - **Stream Concurrency & Replay Cache**:
    - `ActiveStream` owns: `cancel_flag: Arc<AtomicBool>`, `steer_tx: mpsc::Sender<String>`, event ring buffer (up to 100 events, max 64KiB per event, larger text chunks split sequentially), and a 5-minute post-completion TTL.
    - Active running streams are bounded at 32. Concurrency slot is released immediately on turn completion, error, or cancellation.
    - `streamEnd` is emitted exactly once upon closing the SSE stream connection (not duplicated in buffer).
    - Unknown or expired stream IDs return HTTP 404 (`{"error": "Stream not found or expired"}`).
    - `/api/chat/steer` sends messages via `steer_tx`. If the stream has concluded, returns HTTP 400 (`{"error": "Stream is not active"}`).
  - **Path Confinement & Safe Access**: All file/workspace operations strictly canonicalize paths against `workspace_root`, reject `..` traversal (returning HTTP 403), and enforce a 10MB maximum file size limit (returning HTTP 413 if exceeded).
  - **Dynamic Model & Settings Introspection**:
    - `/api/models` returns active model and configured fallbacks from `runtime_config`.
    - `/api/default-model` returns `config.client.model`.
    - `/api/settings` dynamically reports `auth_enabled: state.auth_password.is_some()`.
  - **Authentication & Route Access**:
    - Public endpoints: `/health`, `/api/auth/status`, `/api/auth/login`.
    - Protected endpoints (require valid Bearer token or cookie when password is set): all other `/api/*` routes.
    - `OPTIONS` requests are handled by CORS middleware before auth checks, returning 200/204 to ensure browser preflight is never rejected with 401.
    - Tokens are 32-byte cryptographically secure random hex strings, verified in constant time.
    - Cookie `hermes_auth=<token>` is set on login (`HttpOnly; SameSite=Lax; Path=/`).
    - CORS: `CorsLayer` permits `[GET, POST, OPTIONS]` with headers `[Authorization, Content-Type, Accept, Cache-Control]` and allowed origin `Any` (credentialed cross-origin disabled). Request body size is capped at 2MB.

## Configuration Schema
In `crates/kerux-core/src/config.rs` (`GatewaySettings`):
```toml
[gateway]
webui_enabled = false
webui_addr = "127.0.0.1:8787"
webui_password = "" # Optional on loopback; mandatory on non-loopback binds
```
Default function `default_webui_addr() -> String { "127.0.0.1:8787".into() }` is used via `#[serde(default = "default_webui_addr")]`. Environment variable `KERUX_WEBUI_PASSWORD` takes highest precedence (if non-empty), followed by `HERMES_WEBUI_PASSWORD`.

## Goals
1. Provide core HTTP endpoints with exact JSON schemas:
   - Health and authentication (`/health`, `/api/auth/status`, `/api/auth/login`).
   - Session discovery and management (`/api/sessions`, `/api/session`, `/api/session/new`, `/api/session/rename`, `/api/session/delete`).
   - Real-time chat streaming over Server-Sent Events (`/api/chat/start`, `/api/chat/stream`).
   - Run interruption (`/api/chat/cancel`) and steering (`/api/chat/steer`).
   - Workspace file browsing (`/api/workspaces`, `/api/list`, `/api/file`).
   - Model and settings introspection (`/api/models`, `/api/settings`, `/api/default-model`).
2. Map Kerux's native `AgentEvent` lifecycle events directly to standard WebUI SSE frames (`token`, `reasoning`, `toolStarted`, `toolCompleted`, `error`, `done`, `metering`, `heartbeat`, `cancelled`, `streamEnd`).
3. Support optional password authentication via `Authorization: Bearer <token>` or Cookie `hermes_auth=<token>`.
4. Ensure full test coverage with isolated unit/integration tests using `tempfile`, `MockAgentRunner`, and in-memory `tower::oneshot`.

## Non-Goals
- Full replication of legacy Python WebUI plugin repository or heavy frontend asset bundling.
- Rewriting the Hermex iOS client itself (Kerux conforms to the existing Hermex contract).
