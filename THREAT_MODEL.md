# Endara Relay — Threat Model

This document describes the trust boundaries, in-scope threats, and intentional
non-goals for the Endara Relay process.

## Trust model

- The relay is a **single-user, single-host process**. The only intended client
  of the management API (`/api/*`) is the local Endara Desktop app running as
  the same OS user that started the relay.
- The MCP plane (`/mcp`, `/mcp/*`) is a localhost TCP service consumed by MCP
  clients (Claude Desktop, Claude Code, etc.) on the same machine.
- Upstream MCP servers configured by the user are treated as semi-trusted: the
  relay forwards their tool/resource output to MCP clients but does not execute
  their code in-process. The optional JS sandbox executes scripts that come from
  the user's local TOML config, not from upstream servers.

## In-scope assets

- OAuth tokens at rest under `$HOME/.endara/tokens` (default `0700`).
- The user's TOML configuration (server credentials, JS execution toggles).
- The integrity of upstream MCP requests/responses passing through the relay.
- The integrity of the GitHub Actions release pipeline that produces signed
  binaries / Homebrew tap.

## In-scope adversaries

1. **Malicious / compromised web origin in the user's browser** — visits a page
   that tries to fetch `http://127.0.0.1:<port>/api/...` to read or mutate relay
   state.
2. **Co-resident OS user** on a shared host — separate UID, attempting to talk
   to the relay or read its on-disk state.
3. **Compromised upstream MCP server** — a server the user has configured
   returns malicious tool definitions or response payloads.
4. **Attacker-controlled metadata in PR titles / branch names / GitHub event
   payloads** during release CI.
5. **Compromised third-party GitHub Action** referenced by a moving tag (`@v1`,
   `@main`).

## Out-of-scope adversaries

- Local code-execution attackers running as the same OS user (game over by
  definition).
- Network-adjacent attackers on the LAN — the relay does not bind non-loopback
  addresses.
- Adversaries with physical access to the machine.
- Side-channel and supply-chain attacks against the Rust toolchain or its
  standard library.

## Protections

### Network surface

- `/mcp`, `/mcp/*`, `/healthz`, `/oauth/callback` listen on TCP loopback
  (`127.0.0.1`) only — never `0.0.0.0`. Configurable port. CORS is **not**
  `permissive`; only `http(s)://localhost:*` and `http(s)://127.0.0.1:*` origins
  are accepted by the localhost-origin predicate.
- `/api/*` is **not exposed over TCP**. It binds:
  - **macOS / Linux:** a Unix-domain socket at
    `$XDG_RUNTIME_DIR/endara-relay/api.sock` (or
    `$TMPDIR/endara-relay-<uid>/api.sock` on macOS).
  - **Windows:** a Named Pipe at `\\.\pipe\endara-relay-<sid>`.
- Browser-origin requests cannot reach `/api/*` regardless of CORS rules — the
  transport itself is unreachable from any web origin.
- On Unix, `SO_PEERCRED` (UCred) verifies the peer UID matches the relay's UID;
  mismatched UIDs are rejected.
- On Windows, the Named Pipe ACL is restricted to the current user SID.
- Stale-socket cleanup runs at startup so a previous crashed instance does not
  block the new bind.
- The OAuth callback HTML response is escaped and served with a strict CSP
  (`default-src 'none'; style-src 'unsafe-inline'`) to neutralise XSS in the
  redirect URL.

### Process surface

- Token directory permissions are forced to `0700` and verified on every
  startup.
- Token files are written with `0600`.
- The desktop app is the only intended consumer of `/api/*`. Third-party CLIs
  and other local processes are explicitly out of scope.

### Sandbox surface

- The optional JS sandbox uses `boa_engine` and only runs user-authored scripts
  from the local TOML config — never scripts supplied by upstream servers. The
  sandbox is a correctness boundary, not a security boundary against an
  OS-level attacker.
- When `local_js_execution = false`, `execute_tools` is removed from the catalog
  **and** rejected at invocation time as defense in depth.

### Build & release surface

- All third-party GitHub Actions on the signing path are pinned to commit SHAs
  (no moving `@v1` / `@main` references).
- Release workflow inputs from `github.*`, `matrix.*`, and `steps.*.outputs.*`
  are funnelled through `env:` vars, never interpolated directly into `run:`
  blocks, to neutralise script-injection.
- The Homebrew tap publishing job runs with least-privilege scoping.

## Known residual risks

- A user who reconfigures their token directory onto a cloud-synced path
  (Dropbox, iCloud Drive, etc.) re-introduces token-leak risk; the app surfaces
  an in-product warning but cannot prevent it.
- The MCP TCP listener is reachable from any local process running as the same
  OS user; we deliberately do not require auth on `/mcp` because it is the
  supported integration surface for local MCP clients.
- Upstream MCP servers are semi-trusted; if the user configures a malicious
  upstream, the relay will faithfully forward its responses to MCP clients.
  Sandbox-the-upstream is not a goal.

## Reporting a vulnerability

See `SECURITY.md` (TODO) for the responsible-disclosure process. In the interim,
file a private security advisory via GitHub at
`https://github.com/endara-ai/endara-relay/security/advisories/new`.

