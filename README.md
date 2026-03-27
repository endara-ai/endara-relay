# Endara Relay

**A local MCP tool aggregator — aggregate multiple MCP servers behind a single endpoint.**

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/endara-relay.svg)](https://crates.io/crates/endara-relay)
[![CI](https://img.shields.io/github/actions/workflow/status/endara-ai/endara-relay/ci.yml?branch=main&label=CI)](https://github.com/endara-ai/endara-relay/actions)
[![GitHub Release](https://img.shields.io/github/v/release/endara-ai/endara-relay)](https://github.com/endara-ai/endara-relay/releases)

---

## What is this?

Endara Relay is a single Rust binary that sits between your AI assistant (Claude Desktop, Cursor, or any MCP client) and all the MCP servers you use. Instead of configuring each server individually in your client, you point your client at one local endpoint — `localhost:9400` — and Relay handles the rest.

It connects to each MCP server using the appropriate transport (STDIO, SSE, or HTTP), merges their tool catalogs into a unified list, and prefixes tool names to avoid collisions. If a server crashes, Relay restarts it automatically. If you edit the config file, Relay picks up the changes without a restart.

No cloud. No accounts. Everything runs on your machine.

```
┌──────────────────────────────────────────────────────┐
│ Endara Relay (single Rust process)                   │
│                                                      │
│  ┌─────────────────┐  ┌─────────────────────────┐   │
│  │ Config Watcher   │  │ Local HTTP Server :9400  │   │
│  │ (notify crate)   │  │                         │   │
│  │ TOML hot-reload  │  │  /mcp   → MCP protocol  │   │
│  └────────┬─────────┘  │  /api   → Management    │   │
│           │            └────────┬────────────────┘   │
│           ▼                     │                    │
│  ┌─────────────────────────┐    │                    │
│  │ Adapter Registry        │◀───┘                    │
│  │                         │                         │
│  │  ┌──────┐ ┌──────┐ ┌──────┐                      │
│  │  │STDIO │ │ SSE  │ │ HTTP │   Adapters            │
│  │  └──┬───┘ └──┬───┘ └──┬───┘                      │
│  └─────┼────────┼────────┼───────────────────────┘   │
│        ▼        ▼        ▼                           │
│     [MCP-A]  [MCP-B]  [MCP-C]                        │
│                                                      │
│  ┌─────────────────────────┐                         │
│  │ JS Sandbox (boa_engine) │                         │
│  │ execute_tools runtime   │                         │
│  └─────────────────────────┘                         │
└──────────────────────────────────────────────────────┘
         ▲
         │ localhost:9400
┌────────┴──────────────┐
│ Claude Desktop /      │
│ Cursor / any MCP      │
│ client                │
└───────────────────────┘
```

## Quick Start

### 1. Install

```bash
# Homebrew (macOS / Linux)
brew install endara-ai/tap/endara-relay

# Cargo
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

---

## Management API

Relay exposes a REST API on `:9400/api` for monitoring and management:

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/status` | Relay status, uptime, endpoint/health counts |
| `GET` | `/api/endpoints` | List all endpoints with health and transport info |
| `GET` | `/api/endpoints/:name/tools` | List tools for a specific endpoint |
| `GET` | `/api/endpoints/:name/logs` | View stderr logs for a STDIO endpoint |
| `POST` | `/api/endpoints/:name/restart` | Restart a specific endpoint |
| `POST` | `/api/endpoints/:name/refresh` | Re-fetch the tool catalog for an endpoint |
| `GET` | `/api/config` | View current config (env values redacted) |
| `POST` | `/api/config/reload` | Trigger a config reload |

**Example:**

```bash
# Check relay status
curl http://localhost:9400/api/status

# List all endpoints
curl http://localhost:9400/api/endpoints

# Restart a misbehaving endpoint
curl -X POST http://localhost:9400/api/endpoints/github/restart
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

## Contributing

Contributions are welcome! Here's how to get started:

1. **Fork** the repository
2. **Create a branch** for your feature or fix (`git checkout -b my-feature`)
3. **Make your changes** and ensure tests pass (`cargo test`)
4. **Run formatting and lints** (`cargo fmt && cargo clippy`)
5. **Submit a pull request** with a clear description of your changes

Please open an issue first for large changes or new features so we can discuss the approach.

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

```
Copyright 2025 Endara AI

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
