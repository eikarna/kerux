# Model Context Protocol (MCP)

Kerux provides native client support for the [Model Context Protocol (MCP)](https://modelcontextprotocol.io), allowing the agent to dynamically discover, load, and execute tools provided by external MCP servers.

Both local subprocess (`stdio`) and remote HTTP/SSE (`http` / `sse`) transports are supported out of the box with zero external dependencies.

## Transports

Kerux supports two primary MCP transport types:

1. **`stdio` (Local Subprocess)**:
   - Spawns a dedicated child process on demand.
   - Communicates via newline-delimited JSON-RPC over stdin/stdout.
   - Non-blocking stderr background draining prevents pipe deadlocks.
   - Ideal for local tool packages (e.g. `@modelcontextprotocol/server-*`, Python scripts, custom CLI binaries).

2. **`http` / `sse` (Remote Streamable HTTP / Server-Sent Events)**:
   - Connects to an existing MCP server endpoint over HTTP.
   - Handles standard MCP JSON-RPC 2.0 handshake (`initialize`, `tools/list`, `tools/call`).
   - Supports optional Bearer token authentication via `auth_token`.
   - Ideal for microservices, containerized agents, or local daemon bridges (e.g. Minecraft Effector MCP).

## Configuration

MCP servers are configured in `kerux.toml` under the `[mcp]` section:

```toml
[mcp]
# Automatically discover and connect to servers at agent startup
autoload = true

# Example 1: Local stdio transport (Node.js npx package)
[[mcp.servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true

[mcp.servers.env]
# Optional environment variables passed to the child process
DEBUG = "false"

# Example 2: Local stdio transport (Python script)
[[mcp.servers]]
name = "python-data"
transport = "stdio"
command = "uv"
args = ["run", "python", "./tools/mcp_server.py"]
enabled = true

# Example 3: Remote HTTP / SSE transport
[[mcp.servers]]
name = "local-bridge"
transport = "http" # or "sse"
url = "http://127.0.0.1:8080/mcp"
auth_token = "optional-bearer-token"
enabled = true
```

### Server Fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `name` | string | Unique identifier for the MCP server. |
| `transport` | string | Either `"stdio"` or `"http"` / `"sse"` (defaults to `"stdio"` if command is set). |
| `command` | string | Executable path or binary name for `stdio` transport. |
| `args` | array | Command-line arguments passed to the executable. |
| `env` | table | Environment variables injected into the subprocess. |
| `url` | string | Target HTTP URL for `http`/`sse` transport. |
| `auth_token` | string | Optional authorization token sent in the `Authorization: Bearer <token>` header. |
| `enabled` | boolean | Set to `false` to disable a configured server without removing it from config. |

## Automatic Discovery

When `autoload = true`, Kerux scans the following locations at startup and connects to all active servers:

1. **`~/.kerux/mcp.json`**: Kerux-native MCP registry file.
2. **Claude Desktop Configuration**:
   - **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`
   - **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
   - **Linux**: `~/.config/Claude/claude_desktop_config.json`

Existing servers configured in Claude Desktop work seamlessly in Kerux without duplicating configuration entries.

## Terminal UI (TUI) Management

In the Kerux TUI workspace view:
- Navigate to the **MCP** management panel (`m` or via tab navigation).
- View all active MCP servers, connection status, and advertised tool schemas.
- Press `a` to open the interactive **Add MCP Server** modal form.
- Press `d` to disconnect and remove the selected MCP server.
