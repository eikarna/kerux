# webui-api Specification Deltas

## ADDED Requirements

### Requirement: Gateway WebUI Service Lifecycle
When `webui_enabled` is true in Gateway configuration, `Gateway::run()` MUST synchronously bind a `tokio::net::TcpListener` before reporting readiness, pass the pre-bound listener to the WebUI server task, add the running WebUI server task to the gateway task handles, propagate runtime serve errors upon task exit, and gracefully terminate the listener when a shutdown signal is received.

#### Scenario: WebUI-only gateway stays running until shutdown signal
- **Given** Gateway configuration with `webui_enabled = true` and no messaging adapters or webhooks
- **When** `Gateway::run()` is invoked
- **Then** the WebUI server starts listening, `run()` remains running indefinitely, and exits cleanly upon receiving shutdown signal

#### Scenario: Synchronous failure propagation on bind error
- **Given** Gateway configuration with `webui_enabled = true` and an invalid or already-bound address
- **When** `Gateway::run()` is invoked
- **Then** `Gateway::run()` returns an error immediately without hanging or silently ignoring the failure

## MODIFIED Requirements

### Requirement: Health and Authentication Probing
The server MUST provide `/health` and `/api/auth/status` endpoints to allow clients to verify server connectivity and authentication state. Token generation MUST be cryptographically secure and fail-closed. When password protection is enabled in configuration (`webui_password` is non-empty), unauthorized requests to all protected endpoints (`/api/sessions`, `/api/session`, `/api/session/*`, `/api/chat/*`, `/api/workspaces`, `/api/list`, `/api/file`, `/api/models`, `/api/models/default`, `/api/default-model`, `/api/settings`) MUST return HTTP 401 Unauthorized (`{"error": "Unauthorized"}`) until authenticated via `/api/auth/login`. CORS middleware MUST apply standard CORS headers (`Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Authorization, Content-Type, hermes-auth`, and `Access-Control-Max-Age: 86400`) across all responses (200, 204, 401, 403, 404, 413, 429, 500). Same-origin browser sessions MAY use the `hermes_auth` cookie, while cross-origin API clients use Bearer authentication.

#### Scenario: Health check returns status ok
- **Given** the Kerux WebUI server is running
- **When** a client sends a `GET /health` request
- **Then** the server responds with HTTP 200 OK and body `{"status": "ok"}`

#### Scenario: Auth status when password protection is disabled
- **Given** the Kerux WebUI server is configured with `webui_password = ""` (empty/disabled)
- **When** a client sends a `GET /api/auth/status` request
- **Then** the server responds with HTTP 200 OK and body `{"authenticated": true, "profile": "default", "version": "0.4.0"}`

#### Scenario: Auth status when password protection is enabled without credentials
- **Given** the Kerux WebUI server is configured with `webui_password = "secret123"`
- **When** a client sends a `GET /api/auth/status` request without credentials
- **Then** the server responds with HTTP 200 OK and body `{"authenticated": false, "profile": "default", "version": "0.4.0"}`

#### Scenario: Valid login grants bearer token and session cookie
- **Given** the Kerux WebUI server is configured with `webui_password = "secret123"`
- **When** a client sends a `POST /api/auth/login` request with `{"password": "secret123"}`
- **Then** the server responds with HTTP 200 OK and body `{"status": "ok", "token": "<token>"}` where token is 64 hex characters, and sets a `Set-Cookie: hermes_auth=<token>; HttpOnly; SameSite=Lax; Path=/` header

#### Scenario: Invalid login is rejected
- **Given** the Kerux WebUI server is configured with `webui_password = "secret123"`
- **When** a client sends a `POST /api/auth/login` request with `{"password": "wrongpassword"}`
- **Then** the server responds with HTTP 401 Unauthorized and body `{"error": "Invalid password"}`

#### Scenario: Protected endpoints reject unauthenticated requests
- **Given** the Kerux WebUI server is configured with `webui_password = "secret123"`
- **When** a client sends a `GET /api/sessions` request without a valid token or cookie
- **Then** the server responds with HTTP 401 Unauthorized

#### Scenario: CORS preflight request is unauthenticated
- **Given** the Kerux WebUI server is configured with `webui_password = "secret123"`
- **When** a client sends an `OPTIONS /api/sessions` preflight request with `Origin: http://localhost:3000`
- **Then** the server responds with HTTP 204 No Content and CORS headers (`Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Authorization, Content-Type, hermes-auth`, `Access-Control-Max-Age: 86400`) without requiring credentials

#### Scenario: Cryptographically secure token generation on login
- **Given** the Kerux WebUI server is running with password protection
- **When** a client sends a valid `POST /api/auth/login` request
- **Then** the server issues a cryptographically secure 64-hex-character token and does not default to zeros on entropy exhaustion

#### Scenario: Fail-closed behavior on entropy generation failure
- **Given** the Kerux WebUI server is running with password protection and OS entropy generation fails
- **When** a client attempts `POST /api/auth/login`
- **Then** the server responds with HTTP 500 Internal Server Error, does not issue an auth token, and sets no authentication cookie

### Requirement: Chat Start and SSE Stream Protocol
The server MUST accept chat turn initiation via `POST /api/chat/start` and stream real-time events via Server-Sent Events on `GET /api/chat/stream?stream_id=...` using standard SSE syntax (`event: <name>\n` and `data: <json>\n\n` framing). Every emitted SSE wire frame across all 11 event variants MUST be dynamically measured and strictly <= 64 KiB (65,536 bytes). The server MUST enforce a concurrency ceiling of 32 running streams, safely chunk oversized payload text across streaming events (`token`, `reasoning`), truncate oversized tool payload strings (> 60,000 bytes) with a notice to keep atomic frames <= 64 KiB, serialize session message writes (saving user message at turn start and assistant message at turn completion), and automatically evict completed streams that have finished over 5 minutes ago via a background stream GC task owned by the WebUI server and on request paths. All stream completions (normal, error, cancellation, panic) MUST transition through an atomic state machine (`0: Active`, `1: Completed`, `2: Cancelled`, `3: Errored`, `4: PanicCaught`) using atomic CAS linearization (`0 -> target`), emitting exactly one terminal `streamEnd` frame.

#### Scenario: Chat turn initiation and session message persistence
- **Given** an existing session `sess-123` and active running streams < 32
- **When** an authenticated client sends a `POST /api/chat/start` request with `{"session_id": "sess-123", "message": "List files", "model": null}`
- **Then** the server appends the user message to session `sess-123`, spawns an agent execution turn via `AgentRunner`, allocates an active stream slot, and returns HTTP 200 OK with `{"stream_id": "<stream_id>", "session_id": "sess-123"}`

#### Scenario: Chat turn rejection when concurrency limit reached
- **Given** 32 chat streams are currently running
- **When** an authenticated client sends a `POST /api/chat/start` request
- **Then** the server responds with HTTP 429 Too Many Requests and body `{"error": "Active stream limit reached"}`

#### Scenario: Real-time event streaming via SSE
- **Given** an active chat stream `stream-abc`
- **When** a client opens a `GET /api/chat/stream?stream_id=stream-abc` SSE connection
- **Then** the server delivers SSE frames matching Hermex contracts:
  - `event: token` with data `{"content": "text chunk"}` for `AgentEvent::Content`
  - `event: reasoning` with data `{"content": "thinking chunk"}` for `AgentEvent::Reasoning` / `Thinking`
  - `event: toolStarted` with data `{"id": "call_1", "name": "terminal", "arguments": {"command": "ls"}}` for `AgentEvent::ToolStart`
  - `event: toolCompleted` with data `{"id": "call_1", "success": true, "content": "file1.txt"}` for `AgentEvent::ToolComplete`
  - `event: toolCompleted` with data `{"id": "call_1", "success": false, "error": "command not found"}` for `AgentEvent::ToolError`
  - `event: metering` with data `{"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}` for `AgentEvent::Telemetry`
  - `event: error` with data `{"error": "Agent execution failed"}` for `AgentEvent::Error`
  - `event: done` with data `{"message": "final answer"}` for `AgentEvent::Done`
  - `event: cancelled` with data `{}` upon cancellation
  - `event: heartbeat` with data `{}` every 5 seconds during stream idle
  - `event: streamEnd` with data `{}` at the conclusion of the stream

#### Scenario: Turn completion appends assistant response to session
- **Given** an active chat stream for session `sess-123`
- **When** the agent emits `AgentEvent::Done` with `message: "final answer"`
- **Then** the server acquires the session lock, appends the assistant message to session `sess-123`, and saves it to disk atomically

#### Scenario: SSE Reconnect and Replay
- **Given** a stream `stream-abc` that has completed within the last 5 minutes
- **When** a client connects to `GET /api/chat/stream?stream_id=stream-abc`
- **Then** the server replays up to 100 retained buffered events in sequential order without duplicate terminal frames, closing with exactly one `event: streamEnd`

#### Scenario: Requesting unknown or expired stream
- **Given** no active or buffered stream exists with ID `stream-unknown`
- **When** a client sends `GET /api/chat/stream?stream_id=stream-unknown`
- **Then** the server responds with HTTP 404 Not Found and body `{"error": "Stream not found or expired"}`

#### Scenario: Eviction of expired replay streams
- **Given** a completed stream where elapsed time since `completed_at` is older than 5 minutes
- **When** the stream garbage collector runs on its 30s cadence or a client requests `GET /api/chat/stream?stream_id=<expired_id>`
- **Then** the expired stream entry is evicted from memory and returns HTTP 404 Not Found

#### Scenario: Cancellation lifecycle ordering
- **Given** an in-flight chat stream
- **When** a client sends `POST /api/chat/cancel` with `{"stream_id": "<id>"}`
- **Then** the agent runner receives the cancellation signal, emits `cancelled`, cleanly completes its task, and emits a single terminal `streamEnd` frame without duplication

#### Scenario: Payload chunking across oversized streaming content
- **Given** streaming text content in `Content` or `Reasoning` exceeding 64 KiB
- **When** the event converter processes the payload
- **Then** the text payload is safely partitioned into multiple consecutive sequential SSE frames at valid UTF-8 code point boundaries such that every complete wire frame is strictly <= 64 KiB

#### Scenario: Oversized discrete tool payload bounding
- **Given** a tool event (`ToolStart`, `ToolComplete`, `ToolError`, `Done`) containing payload text exceeding 60,000 bytes
- **When** the event converter processes the payload
- **Then** the payload text is safely bounded with a truncation notice so the emitted JSON wire frame is strictly <= 64 KiB

### Requirement: Workspace and Model Introspection
The server MUST provide endpoints for workspace browsing, file inspection, and model inspection, supporting both `/api/models/default` and `/api/default-model`, with strict path canonicalization and symlink escape rejection. All filesystem operations MUST be non-blocking.

#### Scenario: Listing workspaces
- **Given** the configured workspace directory `/home/alxy/workspace`
- **When** an authenticated client sends a `GET /api/workspaces` request
- **Then** the server responds with HTTP 200 OK and body `{"workspaces": [{"path": "/home/alxy/workspace", "name": "workspace", "active": true}]}`

#### Scenario: Directory listing within workspace root
- **Given** workspace root contains a subdirectory `src`
- **When** an authenticated client sends a `GET /api/list?path=src` request
- **Then** the server responds with HTTP 200 OK and body `{"items": [{"name": "main.rs", "path": "src/main.rs", "is_dir": false, "size": 1024}]}`

#### Scenario: Rejection of path traversal attempts
- **Given** workspace root is `/home/alxy/workspace`
- **When** a client sends a `GET /api/list?path=../../etc` or `GET /api/file?path=/etc/passwd` request
- **Then** the server responds with HTTP 403 Forbidden and body `{"error": "Access denied: Path escapes workspace root"}`

#### Scenario: Rejection of symlink escapes outside workspace root
- **Given** a symlink inside workspace root pointing to a file or directory outside the workspace root
- **When** an authenticated client sends a `GET /api/file?path=symlink_escape` or `GET /api/list?path=symlink_dir_escape` request
- **Then** the server canonicalizes the target path and responds with HTTP 403 Forbidden and body `{"error": "Access denied: Path escapes workspace root"}`

#### Scenario: Reading file content within size limit
- **Given** workspace root contains `Cargo.toml` (< 10MB)
- **When** an authenticated client sends a `GET /api/file?path=Cargo.toml` request
- **Then** the server responds with HTTP 200 OK and body `{"path": "Cargo.toml", "content": "..."}`

#### Scenario: Rejection of oversized file reads
- **Given** workspace root contains a file `large.bin` larger than 10MB
- **When** an authenticated client sends a `GET /api/file?path=large.bin` request
- **Then** the server responds with HTTP 413 Payload Too Large and body `{"error": "File size exceeds 10MB limit"}`

#### Scenario: Querying configured models and settings dynamically
- **Given** the server configuration with model `gpt-4o` from provider `openai` and `webui_password = "secret"`
- **When** an authenticated client sends `GET /api/models`, `GET /api/default-model`, or `GET /api/settings`
- **Then** the server responds with HTTP 200 OK with dynamic metadata:
  - `/api/models` -> `{"models": [{"id": "gpt-4o", "name": "gpt-4o", "provider": "openai"}], "default": "gpt-4o"}`
  - `/api/default-model` -> `{"model": "gpt-4o"}`
  - `/api/settings` -> `{"version": "0.4.0", "webui": {"auth_enabled": true}}`

#### Scenario: Querying default model via models default endpoint
- **Given** configured model `gpt-4o`
- **When** an authenticated client sends `GET /api/models/default` or `GET /api/default-model`
- **Then** the server responds with HTTP 200 OK and body `{"model": "gpt-4o"}`
