# WebUI and Hermex API Compatibility Specification

## ADDED Requirements

### Requirement: Health and Authentication Probing
The server MUST provide `/health` and `/api/auth/status` endpoints to allow clients to verify server connectivity and authentication state. When password protection is enabled in configuration (`webui_password` is non-empty), unauthorized requests to protected endpoints MUST return HTTP 401 Unauthorized (`{"error": "Unauthorized"}`) until authenticated via `/api/auth/login`. CORS preflight `OPTIONS` requests MUST be answered with 200/204 before auth checks.

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
- **Then** the server responds with HTTP 200 OK and body `{"status": "ok", "token": "<token>"}` and sets a `Set-Cookie: hermes_auth=<token>; HttpOnly; SameSite=Lax; Path=/` header

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
- **Then** the server responds with HTTP 200/204 OK and CORS headers without requiring credentials

### Requirement: Session Management Endpoints
The server MUST provide endpoints to list, retrieve, create, rename, and delete conversation sessions stored as JSON files under `{session_dir}/{session_id}.json`. Session IDs MUST be validated to match `^[a-zA-Z0-9_-]{1,64}$`. Mutations to a session MUST be serialized with a per-session mutex.

#### Scenario: Listing sessions
- **Given** session files exist in the sessions directory
- **When** an authenticated client sends a `GET /api/sessions` request
- **Then** the server responds with HTTP 200 OK and body `{"sessions": [{"id": "sess-1", "title": "Session 1", "created_at": 1720000000, "updated_at": 1720000100, "message_count": 4}]}`

#### Scenario: Fetching single session history
- **Given** a session with ID `sess-123` exists
- **When** an authenticated client sends a `GET /api/session?session_id=sess-123` request
- **Then** the server responds with HTTP 200 OK and body `{"id": "sess-123", "title": "My Session", "created_at": 1720000000, "updated_at": 1720000100, "messages": [{"role": "user", "content": "hello", "timestamp": 1720000050}]}`

#### Scenario: Fetching nonexistent session returns 404
- **Given** no session exists with ID `sess-none`
- **When** an authenticated client sends a `GET /api/session?session_id=sess-none` request
- **Then** the server responds with HTTP 404 Not Found and body `{"error": "Session not found"}`

#### Scenario: Creating a new session
- **Given** the server is running
- **When** an authenticated client sends a `POST /api/session/new` request with `{"title": "My New Session"}`
- **Then** the server creates a new session file and responds with HTTP 200 OK with `{"id": "<session_id>", "title": "My New Session"}`

#### Scenario: Renaming an existing session
- **Given** a session with ID `sess-123` exists
- **When** an authenticated client sends a `POST /api/session/rename` request with `{"session_id": "sess-123", "title": "Renamed Title"}`
- **Then** the server updates the title on disk and responds with HTTP 200 OK with `{"status": "ok"}`

#### Scenario: Deleting an existing session
- **Given** a session with ID `sess-123` exists
- **When** an authenticated client sends a `POST /api/session/delete` request with `{"session_id": "sess-123"}`
- **Then** the server removes the session file from disk and responds with HTTP 200 OK with `{"status": "ok"}`

### Requirement: Chat Start and SSE Stream Protocol
The server MUST accept chat turn initiation via `POST /api/chat/start` and stream real-time events via Server-Sent Events on `GET /api/chat/stream?stream_id=...`. The server MUST enforce a concurrency ceiling of 32 running streams and serialize session message writes.

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
  - `event: heartbeat` with data `{}` every 5 seconds
  - `event: streamEnd` with data `{}` at the conclusion of the stream

#### Scenario: Turn completion appends assistant response to session
- **Given** an active chat stream for session `sess-123`
- **When** the agent emits `AgentEvent::Done` with `message: "final answer"`
- **Then** the server acquires the session lock, appends the assistant message to session `sess-123`, and saves it to disk atomically

#### Scenario: SSE Reconnect and Replay
- **Given** a stream `stream-abc` that has completed within the last 5 minutes
- **When** a client connects to `GET /api/chat/stream?stream_id=stream-abc`
- **Then** the server replays all buffered events in order before closing with `event: streamEnd`

#### Scenario: Requesting unknown or expired stream
- **Given** no active or buffered stream exists with ID `stream-unknown`
- **When** a client sends `GET /api/chat/stream?stream_id=stream-unknown`
- **Then** the server responds with HTTP 404 Not Found and body `{"error": "Stream not found or expired"}`

### Requirement: Stream Cancellation and Steering
The server MUST allow clients to cancel an in-flight stream or steer the running agent.

#### Scenario: Cancelling active chat stream
- **Given** a running agent stream `stream-abc`
- **When** an authenticated client sends a `POST /api/chat/cancel` request with `{"stream_id": "stream-abc"}`
- **Then** the server signals cooperative cancellation to the agent, emits `event: cancelled`, emits `event: streamEnd`, releases the running stream slot, and returns HTTP 200 OK with `{"status": "cancelled"}`

#### Scenario: Steering active chat stream
- **Given** a running agent stream `stream-abc` with an active `steer_tx` channel
- **When** an authenticated client sends a `POST /api/chat/steer` request with `{"stream_id": "stream-abc", "message": "Please stop searching and answer now"}`
- **Then** the server delivers the steering message to the active agent execution channel and returns HTTP 200 OK with `{"status": "ok"}`

#### Scenario: Steering an inactive stream returns Bad Request
- **Given** a stream `stream-completed` that is not currently running
- **When** an authenticated client sends a `POST /api/chat/steer` request with `{"stream_id": "stream-completed", "message": "hi"}`
- **Then** the server responds with HTTP 400 Bad Request and body `{"error": "Stream is not active"}`

### Requirement: Workspace and Model Introspection
The server MUST provide endpoints for workspace browsing and model inspection, strictly confined to the workspace root directory.

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
