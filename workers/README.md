# Endara Relay on Cloudflare Workers

The relay's MCP aggregation running at the edge: one authenticated `/mcp`
endpoint in front of your remote MCP servers. It scales to zero, and has no
servers or containers to run. It is deployed with [Alchemy](https://alchemy.run).

It is a Rust crate (`workers-rs`) compiled to WebAssembly. It shares the
naming, URI-wrapping and TOON code with the local relay (`../src/prefix.rs`,
`resource_uri.rs`, `tool_call_rewrite.rs`, `toon_convert.rs`, `protocol.rs`),
so tool names and `mcp-relay://` URIs are the same as on the local relay.

## What runs on the edge

| Feature | Edge relay |
|---|---|
| Streamable-HTTP upstreams (JSON or SSE replies) | Yes |
| Tool / resource / prompt aggregation, `<prefix>__` names, single-endpoint passthrough | Yes |
| `list_tools` / `search_tools` meta-tools, TOON output, profiles (`/mcp/{path}`) | Yes |
| Upstream auth with static headers (`$SECRET` references) | Yes |
| STDIO servers, Docker isolation | No: Workers cannot start processes |
| Legacy SSE upstreams | No: use streamable HTTP |
| OAuth / EMA-managed upstreams | Not yet |
| `execute_tools` JS sandbox, `write_file`, management API, hot reload | No |

Skipped endpoints are listed with a reason in `GET /healthz`.

## Speed

- Upstream sessions (`Mcp-Session-Id`) are cached, so a warm call costs one
  upstream round trip. A `404` on a cached session makes the relay start a new
  session and retry once.
- Tool/resource/prompt lists are cached for `cache_ttl_secs` (default 300) in
  isolate memory and in the `RELAY_CACHE` KV namespace. A cold isolate
  therefore does not have to start upstream sessions or fetch lists again.
- `tools/call` is routed by name prefix. It does not wait for a catalog fetch.
- A failing upstream is cached as failed for 30 s, so it does not slow down
  every list request.

## Deploy

Requires Rust with `wasm32-unknown-unknown`, `cargo install worker-build`, Node 22+.

```sh
cd workers
npm install
cp relay.example.toml relay.toml          # edit endpoints
cat > .env <<'EOF'
CLOUDFLARE_ACCOUNT_ID=...
CLOUDFLARE_API_TOKEN=...
RELAY_AUTH_TOKEN=<long random string>     # clients send: Authorization: Bearer <token>
GITHUB_TOKEN=...                          # every $VAR used in relay.toml
EOF
npm run deploy                            # worker-build --release && alchemy deploy
```

`alchemy.run.ts` creates the `RelayCache` KV namespace and the `Relay` Worker.
It binds `relay.toml` (or `$RELAY_CONFIG_FILE`) and every `$VAR` that the
TOML references as Worker secrets. `npm run destroy` removes them.

Point MCP clients at `https://<worker-url>/mcp` with the bearer token.

## Test

```sh
npm run build && npm run test:e2e
```

This runs the built Worker in Miniflare against two mock upstreams (a JSON
upstream with sessions and an SSE upstream) and a dead upstream. It writes
`e2e/out/report.json` with every check and the full request/response
transcript.
