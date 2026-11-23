# WebUI & Hermex iOS Compatibility Layer

Kerux includes a native REST and Server-Sent Events (SSE) API layer designed for web dashboards and native mobile clients, including the [Hermex](https://github.com/uzairansaruzi/hermex) native iOS SwiftUI client.

## Quickstart

Start the gateway with WebUI enabled via CLI:

```bash
kerux serve --webui --webui-port 8787
```

Or configure it in your `kerux.toml`:

```toml
[gateway]
webui_enabled = true
webui_addr = "0.0.0.0:8787"
# webui_password = "optional-secret"
```

## Hermex iOS Setup

1. Open **Hermex** on your iPhone or iPad.
2. In Settings, set the **Server URL** to `http://<your-machine-ip>:8787`.
3. If `webui_password` is set, enter the password in the **Password** field.
4. Tap **Connect**. You can now chat with your Kerux agent, view live reasoning traces, inspect tool execution cards, and browse prior sessions.

## API Reference

### Health & Auth

- `GET /health`
  Returns server health, uptime, and active stream metrics.
  ```json
  {
    "status": "ok",
    "sessions": 4,
    "activeStreams": 0,
    "uptimeSeconds": 128.4
  }
  ```

- `GET /api/auth/status`
  Returns whether password protection is enabled and if the client is currently authorized.

- `POST /api/auth/login`
  Authenticates a client with a password body (`{"password": "..."}`).

### Sessions

- `GET /api/sessions`
  Lists all persistent chat sessions recorded by Kerux.
  ```json
  {
    "sessions": [
      {
        "sessionId": "cli:default",
        "title": "Cli Default",
        "messageCount": 12,
        "estimatedTokens": 1420,
        "updatedAt": 1795412345
      }
    ],
    "count": 1
  }
  ```

- `GET /api/session?id=<session_id>`
  Fetches full session details including message history.

- `POST /api/session/new`
  Creates a new session ID with an optional title.

- `POST /api/session/rename`
  Renames a session: `{"sessionId": "...", "title": "New Title"}`.

- `POST /api/session/delete`
  Deletes session history: `{"sessionId": "..."}`.

### Chat & Streaming (SSE)

- `POST /api/chat/start`
  Starts an agent turn for a given session.
  ```json
  {
    "sessionId": "cli:default",
    "message": "List recent commits in this repository."
  }
  ```
  Returns:
  ```json
  {
    "streamId": "stream_1732360000_1a2b3c4d",
    "sessionId": "cli:default"
  }
  ```

- `GET /api/chat/stream?stream_id=<stream_id>`
  Opens a Server-Sent Events (SSE) stream (`text/event-stream`). Emits the following event types:
  - `token`: Streaming assistant text tokens (`{"text": "..."}`).
  - `reasoning`: Chain-of-thought or thinking text (`{"text": "..."}`).
  - `toolStarted`: Emitted when the agent calls a tool (`{"name": "...", "callId": "...", "arguments": "..."}`).
  - `toolCompleted`: Emitted when the tool finishes (`{"name": "...", "callId": "...", "content": "...", "success": true}`).
  - `metering`: Token metrics and generation speed.
  - `done`: Emitted when the agent finishes the turn (`{"sessionId": "..."}`).
  - `error`: Emitted on runtime failure (`{"message": "..."}`).
  - `streamEnd`: Terminal event indicating the stream is closing (`{}`).

- `GET /api/chat/cancel?stream_id=<stream_id>` or `POST /api/chat/cancel`
  Cancels an in-flight agent run.

- `POST /api/chat/steer`
  Appends mid-turn steering instructions to the active agent run.

### Settings & Models

- `GET /api/models`
  Returns available configured models.

- `GET /api/settings`
  Returns current gateway and runtime settings.
