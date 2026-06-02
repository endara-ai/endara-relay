# Endara Relay

**One endpoint for all your MCP servers.** [endara.ai](https://endara.ai)

Aggregate local and cloud MCP servers behind a single endpoint.
Add servers, manage OAuth, connect any AI client — all from one place.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://img.shields.io/github/actions/workflow/status/endara-ai/endara-relay/ci.yml?branch=main&label=CI)](https://github.com/endara-ai/endara-relay/actions)
[![GitHub Release](https://img.shields.io/github/v/release/endara-ai/endara-relay)](https://github.com/endara-ai/endara-relay/releases)

<!-- TODO(website): unblock once endara.ai/images/desktop-hero.png exists -->
<!-- ![Endara Desktop — endpoint dashboard](https://endara.ai/images/desktop-hero.png) -->

> Works with Claude Desktop, ChatGPT, Cursor, Windsurf, VS Code, Zed, Continue, and any MCP-compatible client.

---

## Why?

- **One endpoint, not N** — point every AI client at `localhost:9400` instead of pasting the same MCP server config into each app.
- **OAuth managed for you** — Relay handles token storage and refresh for servers that need it, so your clients don't have to.
- **Hot-reload config** — edit your TOML, save, and Relay picks up the change without a restart.
- **Automatic restart on crash** — flaky STDIO servers come back on their own with exponential backoff.
- **Endpoint profiles** — serve named subsets of your endpoints under their own `/mcp/{profile}` URL so different agents can share one relay without sharing one catalog.
- **Live tool-call event stream** — subscribe to every tool call via Server-Sent Events on the management API; powers the [Endara Desktop](https://github.com/endara-ai/endara-desktop) overlay.
- **Fully local** — no cloud, no accounts, no telemetry. Everything runs on your machine.

---

## What is this?

Endara Relay is a single Rust binary that sits between your AI assistant (Claude Desktop, Cursor, or any MCP client) and all the MCP servers you use. Instead of configuring each server individually in your client, you point your client at one local endpoint — `localhost:9400` — and Relay handles the rest.

It connects to each MCP server using the appropriate transport (STDIO, SSE, or HTTP), merges their tool catalogs into a unified list, and prefixes tool names to avoid collisions. If a server crashes, Relay restarts it automatically. If you edit the config file, Relay picks up the changes without a restart.

No cloud. No accounts. Everything runs on your machine.

```
┌──────────────────────────────────────────────────────┐
│ Endara Relay (single Rust process)                   │
│                                                      │
│  ┌────────────────────┐  ┌──────────────────────────┐│
│  │ TCP loopback :9400 │  │ Unix socket / Named pipe ││
│  │  /mcp  /healthz    │  │  /api/*  (per-user, 0600)││
│  │  /oauth/callback   │  │                          ││
│  └────────────────────┘  └──────────────────────────┘│
└──────────────────────────────────────────────────────┘
```

The TCP loopback listener serves MCP traffic, the health probe, and the OAuth callback. The management API (`/api/*`) is bound exclusively to a per-user OS-local Unix-domain socket (Linux/macOS) or Named Pipe (Windows) with 0600 permissions; it is not reachable over TCP. See [Management API](#management-api) for the full endpoint list and the platform-specific socket paths.

## Quick Start

### 1. Install

```bash
# Homebrew — recommended (macOS / Linux)
brew install endara-ai/tap/endara-relay

# Or, with cargo:
cargo install endara-relay

# Or download a pre-built binary from GitHub Releases:
# https://github.com/endara-ai/endara-relay/releases
```

### 2. Create a config file

```bash
mkdir -p ~/.endara
cat > ~/.endara/config.toml << 'EOF'
[relay]
machine_name = "my-laptop"

[[endpoints]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/projects"]

[[endpoints]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
EOF
```

### 3. Run

```bash
endara-relay --config ~/.endara/config.toml
```

### 4. Connect your MCP client

Point Claude Desktop (or any MCP client) to `http://localhost:9400/mcp`. You'll see tools from all configured endpoints in a single list, prefixed with the endpoint name:

- `filesystem__read_file`
- `filesystem__write_file`
- `github__list_repos`
- `github__create_issue`

---

## Configuration

The config file is TOML. Here's a complete reference:

```toml
[relay]
machine_name = "my-laptop"        # Required — identifies this machine
local_js_execution = true         # Optional — enable JS execution mode (default: false)
toon_output = true                # Optional — convert JSON tool responses to TOON (default: true)
startup_init_timeout_secs = 60    # Optional — cap on how long the MCP listener waits for
                                  # adapter init before binding 9400 anyway (default: 60)

# STDIO endpoint — spawns a child process
[[endpoints]]
name = "github"                   # Required — unique name, used as tool prefix
transport = "stdio"               # Required — "stdio", "sse", or "http"
command = "npx"                   # Required for stdio — command to run
args = ["-y", "@modelcontextprotocol/server-github"]  # Optional — command arguments
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }              # Optional — environment variables

# SSE endpoint — connects to a Server-Sent Events MCP server
[[endpoints]]
name = "remote-server"
transport = "sse"
url = "http://localhost:3001/sse"  # Required for sse/http — server URL

# HTTP endpoint — connects via JSON-RPC over HTTP
[[endpoints]]
name = "http-server"
transport = "http"
url = "http://localhost:4000/mcp"  # Required for sse/http — server URL

# Optional — override the advertised server_type
# By default the relay derives the server_type from the upstream
# `serverInfo.name`, then strips one of the suffixes
# `-mcp-server`, `_mcp_server`, `-mcp`, `_mcp` (so `linear-mcp-server`
# becomes `linear`). When that produces an awkward name, set
# `server_type_override` to take control of what is advertised to the
# model. The override is sanitized to a valid identifier but is **never**
# auto-stripped — what you write is what gets used.
[[endpoints]]
name = "drive"
transport = "oauth"
url = "https://drivemcp.googleapis.com/mcp/v1"
oauth_server_url = "https://accounts.google.com"
client_id = "$GOOGLE_CLIENT_ID"
server_type_override = "google-drive"  # Optional — overrides upstream-derived server_type

# Endpoint profiles — serve named subsets of the endpoints above
# at /mcp/{path}. Clients pointed at the prefixed URL see only the
# tools from the listed endpoints. See "Endpoint profiles" below.
[[profiles]]
name = "Work"
path = "work"
endpoints = ["github", "drive"]
js_execution = true
toon_output = true

[[profiles]]
name = "Personal"
path = "personal"
endpoints = ["filesystem"]
js_execution = false
toon_output = false
```

### Environment variable resolution

Environment variables in `env` maps are resolved at startup:

| Syntax | Behavior |
|--------|----------|
| `$VAR` | Replaced with the value of `VAR` from the process environment |
| `$$VAR` | Literal string `$VAR` (escape with double `$`) |
| `plain` | Kept as-is |

### Validation rules

- At least one endpoint must be configured
- Endpoint names must be unique and non-empty
- `stdio` transport requires a `command` field
- `sse` and `http` transports require a `url` field

---

## Features

### Multi-transport adapters

Connect to any MCP server regardless of how it communicates:

- **STDIO** — Spawns a child process and communicates over stdin/stdout. Ideal for local CLI-based MCP servers like the official `@modelcontextprotocol/server-*` packages.
- **SSE** — Connects to a remote server using HTTP + Server-Sent Events. Good for servers that push updates.
- **HTTP** — Standard JSON-RPC 2.0 over HTTP POST. The simplest remote transport.

### Tool prefixing

Every tool is automatically prefixed with its endpoint name to prevent collisions. If endpoint `github` exposes a tool called `list_repos`, it becomes `github__list_repos` in the merged catalog. This means you can connect multiple servers that expose identically-named tools without conflicts.

### Config hot-reload

Relay watches your config file for changes using the [notify](https://crates.io/crates/notify) crate. When you save the file, Relay automatically:

- Starts adapters for newly added endpoints
- Stops adapters for removed endpoints
- Restarts adapters for changed endpoints
- Leaves unchanged endpoints running

No restart required.

### Crash recovery

If a STDIO server process crashes, Relay automatically restarts it with exponential backoff. After repeated failures, the endpoint is marked unhealthy. This keeps your tool catalog available even when individual servers are flaky.

### Endpoint profiles

Profiles are named subsets of your registered endpoints served under their own MCP URL. Pointing a client at `http://localhost:9400/mcp/{profile}` (or `http://localhost:9400/mcp/sse/{profile}` for legacy SSE clients) exposes only the tools from the endpoints in that profile's allow-list, so different agents or clients can share one relay without sharing one catalog. The unprefixed `/mcp` URL continues to serve the union of every enabled endpoint.

Each profile owns its own `local_js_execution` and `toon_output` values independent of the global `[relay]` defaults — one profile can serve raw JSON while another keeps TOON encoding on, from the same relay process. Profiles can be edited in TOML or managed through Endara Desktop's **Profiles** tab and the management API.

### Tool-call event stream

The management API exposes a Server-Sent Events stream at `GET /api/events/tool-calls` that publishes every MCP tool call routed through the relay, with lifecycle (in-flight, success, failure), duration, upstream endpoint, and tool name. This is what powers the [Endara Desktop](https://github.com/endara-ai/endara-desktop) tool-call overlay; you can subscribe directly for custom dashboards or telemetry pipelines.

### JS execution mode

When `local_js_execution = true`, Relay replaces the full tool catalog with three meta-tools:

| Meta-tool | Description |
|-----------|-------------|
| `list_tools` | List all available tools across all endpoints |
| `search_tools` | Search tools by name or description |
| `execute_tools` | Run a JavaScript script that can call any tool |

This dramatically reduces context window pollution. Instead of exposing hundreds of tools to the AI, it sees only three. The AI writes short JS scripts to discover and call the tools it needs.

**Example: the AI calls `execute_tools` with:**

```javascript
const repos = await call("github__list_repos", { org: "endara-ai" });
const issues = await call("github__list_issues", { repo: repos[0].name });
return { repos: repos.length, firstRepoIssues: issues };
```

The JS sandbox is powered by [boa_engine](https://crates.io/crates/boa_engine) and runs entirely in-process — no external runtime needed.

### TOON tool output

By default, Relay converts JSON tool responses to [TOON](https://crates.io/crates/toon-format) (Token-Oriented Object Notation) before they reach your MCP client. TOON is an indentation-driven format with tabular array headers that produces ~40–60% fewer tokens than JSON on the structured shapes most MCP tools return, while remaining losslessly round-trippable.

Scalars, non-JSON text, image/resource content, `structuredContent`, and error responses pass through unchanged. To opt out, set `toon_output = false` under `[relay]` in your config, or pass `--no-toon` on the command line.

---

## Management API

Relay exposes a management REST API for monitoring and control. The API is reachable only through an OS-local Unix-domain socket (Linux/macOS) or Named Pipe (Windows) created per-user with 0600 permissions; it is not bound to TCP.

| Platform | Path |
|----------|------|
| Linux    | `$XDG_RUNTIME_DIR/endara-relay-<suffix>/api.sock` (fallback: `<data-dir>/api.sock`) |
| macOS    | `$TMPDIR/endara-relay-<uid>-<suffix>/api.sock` |
| Windows  | `\\.\pipe\endara-relay-<user-sid>` |

`<suffix>` is a stable hash of the relay's data directory, so two relays running against different data dirs get distinct sockets. The exact path is logged at startup and is also reported by `GET /api/status`.

A curated subset of the API:

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/status` | Relay status, uptime, endpoint/health counts, resolved socket path |
| `GET` | `/api/endpoints` | List all endpoints with health and transport info |
| `GET` | `/api/catalog` | Full merged tool catalog with applied prefixes and current availability |
| `GET` | `/api/endpoints/:name/tools` | List tools for a specific endpoint |
| `GET` | `/api/endpoints/:name/logs` | View stderr logs for a STDIO endpoint |
| `POST` | `/api/endpoints/:name/restart` | Restart a specific endpoint |
| `POST` | `/api/endpoints/:name/refresh` | Re-fetch the tool catalog for an endpoint |
| `POST` | `/api/endpoints/:name/disable` &nbsp;/ `enable` | Hide or restore an endpoint without removing it |
| `GET` | `/api/config` | View current config (env values redacted) |
| `POST` | `/api/config/reload` | Trigger a config reload |
| `GET` | `/api/events/tool-calls` | Server-Sent Events stream of every tool call (in-flight, success, failure, duration) |

OAuth flows (`/api/endpoints/:name/oauth/*`, `/api/oauth/setup/*`) and per-tool enable/disable endpoints are documented in full on the [Endara Relay docs](https://endara.ai/docs/relay#management-api).

**Example (Linux):**

```bash
curl --unix-socket "$XDG_RUNTIME_DIR/endara-relay-<suffix>/api.sock" http://localhost/api/status
```

---

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (stable, 2021 edition)
- macOS, Linux, or Windows

### Build

```bash
git clone https://github.com/endara-ai/endara-relay.git
cd endara-relay
cargo build --release
```

The binary will be at `target/release/endara-relay`.

### Run tests

```bash
# Unit tests
cargo test

# All tests (including integration tests)
cargo test --all-targets
```

---

## Releasing

Releases are automated via GitHub Actions. To create a new release:

1. Tag the commit: `git tag v0.1.8 && git push origin v0.1.8` (release candidates use `vX.Y.Z-rc.N`)
2. The [release workflow](.github/workflows/release.yml) automatically:
   - Builds release binaries for all platforms (Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64)
   - Creates a GitHub Release with the tag
   - Uploads platform binaries as release assets

Binary naming convention: `endara-relay-{target_triple}` (e.g. `endara-relay-aarch64-apple-darwin`, `endara-relay-x86_64-pc-windows-msvc.exe`)

The [Endara Desktop](https://github.com/endara-ai/endara-desktop) release workflow downloads these binaries to bundle as a Tauri sidecar.

### CI

On every push and PR, the CI workflow runs:
- `cargo fmt --check` — formatting
- `cargo clippy -- -D warnings` — linting
- `cargo test` — unit tests
- `cargo test --test '*'` — integration tests
- Cross-platform build matrix (Linux, macOS, Windows)

---

## Desktop App

Prefer a UI to running a binary from a terminal? [Endara Desktop](https://github.com/endara-ai/endara-desktop) bundles Relay as a sidecar and adds an endpoint dashboard, log viewer, and one-click OAuth flows. It installs from the same Homebrew tap (`brew install --cask endara-ai/tap/endara`) and is built on top of this repo. More at [endara.ai](https://endara.ai).

---

## Security

The relay's threat model — including its trust boundaries, the management API's UDS/Named-Pipe isolation, and the OAuth callback's localhost-only CSRF protections — is documented in [`THREAT_MODEL.md`](THREAT_MODEL.md). See [`SECURITY.md`](SECURITY.md) for the responsible-disclosure process and response-time expectations.

---

## Contributing

Contributions are welcome! Here's how to get started:

1. **Fork** the repository
2. **Create a branch** for your feature or fix (`git checkout -b my-feature`)
3. **Make your changes** and ensure tests pass (`cargo test`)
4. **Run formatting and lints** (`cargo fmt && cargo clippy`)
5. **Submit a pull request** with a clear description of your changes

Please open an issue first for large changes or new features so we can discuss the approach.

---

## Links

- Website: [endara.ai](https://endara.ai)
- Desktop app: [endara-ai/endara-desktop](https://github.com/endara-ai/endara-desktop)
- Releases: [github.com/endara-ai/endara-relay/releases](https://github.com/endara-ai/endara-relay/releases)

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

```
Copyright 2025–2026 Endara AI

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```
