# Proposal: WebUI & Hermex API Production Hardening

## Status
Proposed

## Why
A comprehensive 6-agent production readiness review identified critical reliability, security, and lifecycle defects in the initial WebUI compatibility layer:
1. **Gateway Lifecycle**: When started in WebUI-only mode (`webui_enabled = true` without platform adapters/webhooks), `Gateway::run()` detached the Axum server into an untracked background task, causing the gateway process to exit immediately. Startup binding errors were only logged asynchronously rather than returned.
2. **Cryptographic Token Safety**: `generate_random_token()` converted `getrandom` entropy failures into zeroed buffers, risking deterministic authentication tokens on system entropy exhaustion.
3. **Stream Memory & Task Leaks**: Completed streams (>5 min TTL) and steering receiver tasks were never evicted from `active_streams`, leading to memory and task leaks on long-running instances.
4. **Cancellation Flow Synchronization**: `/api/chat/cancel` prematurely marked streams completed before the background agent runner had exited, causing lost terminal frames or racing with final state persistence.
5. **Endpoint Parity & Chunking**: Missing `/api/models/default` route alias, unchunked large reasoning/tool payloads, and unverified SSE wire frames in tests.
6. **I/O Safety & Persistence Errors**: Synchronous `std::fs` operations in async Axum handlers and ignored return values on session persistence calls.

## What Changes
1. **Gateway Integration**:
   - Bind the WebUI listener synchronously before spawning, returning `Result<()>` directly on failure.
   - Integrate WebUI server `JoinHandle` into `Gateway::run()` tracked `handles` vector, pass `shutdown_rx` for graceful shutdown, and propagate runtime serve errors upon exit (triggering shutdown for all sibling handles).
2. **Security, CORS & Cryptography**:
   - Make token generation fail-closed with error propagation (`Result<String, Error>`) and injectable token generator in `WebUiState` for deterministic testing.
   - Enforce standard CORS headers (`Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Authorization, Content-Type, hermes-auth`, `Access-Control-Max-Age: 86400`) across all route and error/fallback responses, with preflight `OPTIONS` returning 204 No Content before auth checks.
   - Add `/api/models/default` route alias alongside `/api/default-model`.
   - Enforce lexical check for `..`, canonical symlink resolution, and `strip_prefix` containment checks on all workspace filesystem operations.
3. **Stream Lifecycle & Garbage Collection**:
   - Implement completed stream eviction (`evict_expired_completed_streams()`) for streams where elapsed time since completion exceeds `STREAM_REPLAY_TTL` (5 minutes), triggered on a 30s cadence (cancelled on shutdown) and synchronously on request paths.
   - Limit retained replay history to `MAX_EVENTS_PER_STREAM = 100` frames per stream with FIFO ring-buffer eviction.
   - Properly close steering channels and release runner resources on turn completion.
   - Manage stream terminal lifecycle via an atomic state machine (`0: Active`, `1: Completed`, `2: Cancelled`, `3: Errored`, `4: PanicCaught`) with atomic CAS linearization (`0 -> target`), guaranteeing exactly one terminal transition and single `streamEnd` emission across normal completion, errors, cancellation, runner panics, and client disconnects.
4. **Payload Handling & Non-Blocking Async I/O**:
   - Apply dynamic wire-frame measurement ensuring every emitted SSE wire frame `event: ...\ndata: ...\n\n` across ALL 11 event variants is strictly <= 64 KiB (65,536 bytes), chunking streaming text content (`token`, `reasoning`) and truncating oversized discrete payloads with a notice (`... [TRUNCATED]`).
   - Persist user message at turn start (rolling back in-memory mutation on persistence failure) and assistant message at turn completion.
   - Replace blocking synchronous filesystem calls with `tokio::task::spawn_blocking` across all WebUI module code paths, enforcing atomic session write-and-rename and logging persistence errors properly.
5. **Wire-Level Test Verification**:
   - Enhance integration tests to consume and assert raw wire-level SSE frames (`event:` and `data:` strings and `\n\n` delimiters) for all 11 event variants (`token`, `reasoning`, `toolStarted`, `toolCompleted (success=true)`, `toolCompleted (success=false)`, `metering`, `error`, `done`, `cancelled`, `heartbeat`, and `streamEnd`).
   - Cover full auth suite (health check, auth status with/without password, login success with cookie/token, login failure, CORS preflight, fail-closed entropy failure).
   - Add tests for `/api/models/default`, gateway WebUI-only startup and shutdown, and completed stream TTL eviction.

## Goals
- Guarantee WebUI-only gateway mode stays running and handles shutdown cleanly.
- Ensure leak-free stream lifecycle with automated completed-stream eviction.
- Verify exact SSE wire frames matching Hermex iOS client expectations.
- Maintain 100% test pass rate across `cargo test --workspace` and Clippy cleanliness.

## Non-Goals
- Changing the underlying CLI or Ratatui TUI behavior.
- Multi-user ACL permissions system (beyond single-user / password-protected WebUI).
