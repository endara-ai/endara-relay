# Endara Relay on Cloudflare Workers

The relay's MCP aggregation running at the edge: one authenticated `/mcp`
endpoint in front of your remote MCP servers. It includes a web app for
configuration, so there is no config file. It scales to zero, and has no
servers or containers to run. It is deployed with [Alchemy](https://alchemy.run).

It is a Rust crate (`workers-rs`) compiled to WebAssembly. It shares the
naming, URI-wrapping and TOON code with the local relay (`../src/prefix.rs`,
`resource_uri.rs`, `tool_call_rewrite.rs`, `toon_convert.rs`, `protocol.rs`),
so tool names and `mcp-relay://` URIs are the same as on the local relay.

## What runs on the edge

| Feature | Edge relay |
|---|---|
| Web app to configure servers, tools, profiles and settings | Yes |
| Streamable-HTTP upstreams (JSON or SSE replies) | Yes |
| Upstream auth with a bearer token or custom headers (encrypted at rest) | Yes |
| Tool / resource / prompt aggregation, `<prefix>__` names, single-endpoint passthrough | Yes |
| `list_tools` / `search_tools` meta-tools, TOON output, profiles (`/mcp/{path}`) | Yes |
| STDIO servers, Docker isolation | No: Workers cannot start processes |
| Legacy SSE upstreams | No: use streamable HTTP |
| OAuth sign-in with dynamic client registration (RFC 9728 / 8414 / 7591, PKCE, RFC 8707) | Yes |
| Enterprise SSO (EMA) upstreams | Not yet |
| `execute_tools` JS sandbox, `write_file` | No |

## Speed

- Upstream sessions (`Mcp-Session-Id`) are cached, so a warm call costs one
  upstream round trip. A `404` on a cached session makes the relay start a new
  session and retry once.
- Tool/resource/prompt lists are cached (default 300 s) in isolate memory and
  in the `RELAY_CACHE` KV namespace. A cold isolate therefore does not have to
  start upstream sessions or fetch lists again.
- `tools/call` is routed by name prefix. It does not wait for a catalog fetch.
- A failing upstream is cached as failed for 30 s, so it does not slow down
  every list request.

## Deploy

Requires Rust with `wasm32-unknown-unknown`, `cargo install worker-build`, Node 22+.

Create `workers/.env`:

```
CLOUDFLARE_ACCOUNT_ID=...
CLOUDFLARE_API_TOKEN=...
RELAY_AUTH_TOKEN=<long random string>       # sign-in for the web app and MCP clients
RELAY_ENCRYPTION_KEY=<long random string>   # encrypts upstream tokens; never change it
```

Then:

```sh
cd workers
npm install
npm run deploy      # worker-build --release && alchemy deploy --stage prod
```

`alchemy.run.ts` creates the `RelayCache` KV namespace and the `Relay` Worker,
and binds the two secrets. `npm run destroy` removes them.

## Configure

Open the Worker URL in a browser and paste `RELAY_AUTH_TOKEN` to sign in. Add
servers (URL plus an optional bearer token or headers), test the connection,
turn single tools off, make profiles and change settings.

- The config is one JSON document in KV. Upstream tokens are encrypted with
  AES-256-GCM. The web app only ever sees a masked preview.
- A save applies at once in the same isolate and reaches every Cloudflare
  region within about a minute.
- The web app API (`/api/config`, `/api/status`, `/api/test`) uses the same
  bearer token, so it can be scripted.

### OAuth servers

Choose **OAuth** as the authentication for a server and click **Sign in**.
The relay finds the server's authorization server, registers itself as a
client (dynamic client registration), and opens the sign-in page in a popup.
The authorization server redirects back to `https://<worker-url>/oauth/callback`.

- Sign-in uses PKCE S256, the `resource` parameter and the `iss` check.
  Sign-in links are single-use and expire after ten minutes.
- Access and refresh tokens are encrypted in KV. The relay refreshes the
  access token before it expires, and once more if the server answers 401.
- If the refresh token is rejected, the server shows **Sign in needed**.
- The server must support dynamic client registration. Servers that need a
  pre-registered client are not supported yet.

Point MCP clients at `https://<worker-url>/mcp` (or `/mcp/<profile>`) with
`Authorization: Bearer <RELAY_AUTH_TOKEN>`.

## Test

```sh
npm run build && npm run test:e2e
```

This runs the built Worker in Miniflare against two mock upstreams (a JSON
upstream with sessions and an SSE upstream) and a dead upstream. The run sets
up the relay through the web app API. It writes `e2e/out/report.json` with
every check and the full request/response transcript.

`E2E_SERVE=1 npm run test:e2e` keeps the relay running afterwards, so you can
open the web app locally (token `e2e-relay-token`).
