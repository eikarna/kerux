## Implementation Tasks

- [x] T1: Gateway Lifecycle & Shutdown Integration (`crates/kerux-core/src/gateway.rs`)
  - Bind the `tokio::net::TcpListener` synchronously in `Gateway::run()` before spawning the server, returning bind/address errors immediately (zero TOCTOU race)
  - Pass the pre-bound `TcpListener` and `shutdown_rx` into Axum serve task
  - Add WebUI server `JoinHandle` to `handles` vector so WebUI-only mode (`webui_enabled = true` with no messaging adapters) remains running until shutdown signal, serves `/health`, and propagates runtime serve errors upon exit (triggering gateway termination if any handle fails)

- [x] T2: Cryptographic Token Hardening, CORS & Route Aliases (`crates/kerux-core/src/webui/handlers.rs`, `crates/kerux-core/src/webui/mod.rs`)
  - Make `generate_random_token()` fail-closed (`Result<String, crate::error::Error>`) with deterministic error injection support in `WebUiState` for testing
  - Return HTTP 500 without issuing auth tokens or cookies on entropy generation failure
  - Enforce CORS middleware applying standard CORS headers (`Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Authorization, Content-Type, hermes-auth`, `Access-Control-Max-Age: 86400`) across all responses (preflight 204, success 200, unauthorized 401, forbidden 403, not found 404, payload too large 413, rate limit 429, error 500)
  - Add `/api/models/default` route alias alongside `/api/default-model`
  - Ensure canonical symlink resolution and `strip_prefix` containment checks on all workspace file and directory operations, opening files and reading directories immediately under the validated canonical path within `spawn_blocking` (mapping missing paths to 404 and escapes to 403)
  - Ensure all protected endpoints (`/api/sessions`, `/api/session`, `/api/session/*`, `/api/chat/*`, `/api/workspaces`, `/api/list`, `/api/file`, `/api/models`, `/api/models/default`, `/api/default-model`, `/api/settings`) enforce authentication

- [x] T3: Stream Garbage Collection, Slot Reuse & Atomic Terminal State Machine (`crates/kerux-core/src/webui/stream.rs`, `crates/kerux-core/src/webui/handlers.rs`)
  - Enforce bounded replay buffer of at most `MAX_EVENTS_PER_STREAM = 100` frames per stream with FIFO ring-buffer eviction
  - Implement automated eviction (`evict_expired_completed_streams()`) with deterministic timestamp support in `WebUiState`, running on a 30s cadence owned by the WebUI server task and synchronously on request paths, atomically removing completed streams where elapsed time since `completed_at` > `STREAM_REPLAY_TTL` (5 min) under write lock, while retaining active streams and unexpired completed streams (< 5 min)
  - Enforce atomic 32-stream concurrency ceiling with immediate HTTP 429 rejection on the 33rd concurrent start, guaranteeing stream slot release on all termination paths so subsequent turns succeed
  - Ensure steering receiver tasks and `steer_tx` channels are closed and dropped on turn completion without leaking channels or background tasks
  - Manage cancellation and termination via an atomic terminal state machine (`0: Active`, `1: Completed`, `2: Cancelled`, `3: Errored`, `4: PanicCaught`) using atomic CAS linearization (`0 -> target`), with a 5-second task join deadline before forced abort, performing post-abort cleanup, stream completion, and single `streamEnd` emission across normal completion, errors, cancellation, runner panic, and client disconnects

- [x] T4: Structured Payload Chunking & Non-Blocking Async I/O (`crates/kerux-core/src/webui/stream.rs`, `crates/kerux-core/src/webui/handlers.rs`)
  - Apply dynamic wire-frame measurement to ensure every emitted SSE frame `event: ...\ndata: ...\n\n` is strictly <= 64 KiB (65,536 bytes) across all 11 event variants, chunking streaming text content (`token`, `reasoning`) and truncating oversized discrete payloads (> 60,000 bytes) with a notice (`... [TRUNCATED]`)
  - Persist user message at turn start (rolling back in-memory mutation on persistence failure) and assistant message at turn completion
  - Replace blocking synchronous filesystem calls with `tokio::task::spawn_blocking` across all WebUI module handlers and helpers (`list_sessions_handler`, `get_session_handler`, `new_session_handler`, `rename_session_handler`, `delete_session_handler`, `chat_start_handler`, `list_directory_handler`, `read_file_handler`), operating strictly on canonical paths, enforcing per-session mutex locking (`get_session_lock`) + atomic session write-and-rename, handling and logging `write_json` errors properly (returning 500 on synchronous start persistence failure, and emitting SSE `error` frame on background completion persistence failure)

- [x] T5: Wire-Level SSE Integration Tests & Complete Test Suite (`crates/kerux-core/tests/webui_compat.rs`)
  - Wire-level SSE assertions parsing raw `event:` and `data:` strings and `\n\n` boundaries for all 11 event variants with exact JSON payloads:
    - `token` -> `{"content": "..."}`
    - `reasoning` -> `{"content": "..."}`
    - `toolStarted` -> `{"id": "call_1", "name": "terminal", "arguments": {"command": "ls"}}`
    - `toolCompleted` (success=true) -> `{"id": "call_1", "success": true, "content": "file1.txt"}`
    - `toolCompleted` (success=false) -> `{"id": "call_1", "success": false, "error": "command not found"}`
    - `metering` -> `{"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}`
    - `error` -> `{"error": "..."}`
    - `done` -> `{"message": "..."}`
    - `cancelled` -> `{}`
    - `heartbeat` -> `{}`
    - `streamEnd` -> `{}`
  - UTF-8 safe chunking dynamically bounded to <= 64 KiB (65,536 bytes) across oversized streaming text payloads (`token`, `reasoning`) and bounded tool/done outputs verifying every emitted wire frame is strictly <= 64 KiB (65,536 bytes) and chunk concatenation matches original text
  - Ordered replay of retained buffered events (up to 100) upon reconnect, ending with exactly one terminal `streamEnd`, and exact 404 `{"error": "Stream not found or expired"}` for unknown/expired stream
  - Exactly-once `streamEnd` emission verified independently across normal (`done`), error (`error`), runner panic, and cancel (`cancelled`) completion paths, including competing cancel/done/error races
  - Cancellation ordering verifying `cancelled` event -> runner task exit within 5s -> single `streamEnd`, plus forced abort test when runner ignores cancellation
  - Client disconnect handling verifying runner completion to buffer without duplicate terminal frames or task leaks
  - Concurrent burst test using mock runner barriers: launch 40 concurrent start requests, verifying exactly 32 acquire stream slots (HTTP 200) and 8 receive HTTP 429
  - Stream slot release test verifying that completing 32 streams allows subsequent stream starts without 429 rejection
  - Steering channel closure verification: steering a completed stream returns HTTP 400 `{"error": "Stream is not active"}` and steering receiver task exits
  - Heartbeat delivery cadence (5s during idle) verified with deterministic paused Tokio time
  - Complete auth and CORS suite verifying exact response bodies: `GET /health` (200 `{"status":"ok"}`), `GET /api/auth/status` (with and without password), valid login (200 `{"status":"ok","token":"..."}` with 64-hex token + `hermes_auth` cookie), invalid login (401 `{"error":"Invalid password"}`), unauthenticated protected route (401 `{"error":"Unauthorized"}`), and CORS headers (`Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods`, `Access-Control-Allow-Headers`, `Access-Control-Max-Age`) verified across preflight (204), 200, 401, 403, 404, 413, 429, and 500 responses
  - Auth credential matrix: invalid Bearer token (401), invalid cookie (401), valid Bearer token (200), and valid cookie (200)
  - Fail-closed token generation on entropy failure (HTTP 500, no token, no auth cookie)
  - Authentication enforcement across all protected endpoints including `/api/models/default` and `/api/default-model`
  - Workspace and file safety: directory listing, file reading directly on canonical path, path traversal rejection (403 `{"error":"Access denied: Path escapes workspace root"}`), symlink file escape rejection (403), symlink directory escape rejection (403), 404 for missing file/dir, and 10 MB oversized file rejection (413 `{"error":"File size exceeds 10MB limit"}`)
  - Dynamic model and settings introspection: `/api/models` (`{"models":[...],"default":"gpt-4o"}`), `/api/models/default` (`{"model":"gpt-4o"}`), `/api/default-model` (`{"model":"gpt-4o"}`), `/api/settings` (`{"version":"0.4.0","webui":{"auth_enabled":true}}`)
  - Persistence failure handling with deterministic test hooks in `WebUiState`: forced start write failure returns 500 (rolling back in-memory mutation) without spawning runner, forced completion write failure emits SSE `error` frame before `streamEnd`
  - Concurrent session message updates verifying no lost updates under concurrent turns
  - WebUI-only gateway lifecycle startup, `/health` serving, clean shutdown on signal, task join, synchronous bind failure propagation, and runtime serve error propagation
  - Background stream GC automated eviction on 30s cadence and on-demand request path (HTTP 404 after 5 min) verifying active streams and recent completed streams (< 5 min) are retained while expired completed streams are removed, and GC task cancels on shutdown

## Verification Checklist
- [x] `openspec validate --changes` passes
- [x] `cargo check --workspace` passes
- [x] `cargo fmt --all --check` passes
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [x] `cargo test --workspace` passes (all unit + integration tests green)
