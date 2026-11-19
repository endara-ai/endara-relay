// End-to-end run of the built Worker (build/index.js) in Miniflare
// against two mock MCP upstreams. Writes e2e/out/report.json.
// Usage: npm run build && npm run test:e2e
import workerd from "workerd";
import { mkdirSync, writeFileSync } from "node:fs";
import { startMockUpstream, text } from "./mock-upstream.mjs";

// worker-build 0.9 output needs a newer runtime than Miniflare bundles; use
// the pinned `workerd` package (the same one Alchemy dev uses).
process.env.MINIFLARE_WORKERD_PATH ??= workerd.default;
const { Miniflare } = await import("miniflare");

const AUTH = "e2e-relay-token";
const ALPHA_TOKEN = "alpha-secret";

const alpha = await startMockUpstream({
  name: "alpha-server",
  mode: "json",
  token: ALPHA_TOKEN,
  tools: [
    { def: { name: "add", description: "Add two numbers", inputSchema: { type: "object" } }, run: ({ a, b }) => text(a + b) },
    { def: { name: "echo", description: "Echo text back", inputSchema: { type: "object" } }, run: ({ text: t }) => text(`alpha:${t}`) },
    {
      def: { name: "show_app", description: "Open the dashboard app", inputSchema: { type: "object" }, _meta: { ui: { resourceUri: "ui://app/main" } } },
      run: () => ({ content: [{ type: "resource_link", uri: "ui://app/main", name: "app" }] }),
    },
  ],
  resources: [{ def: { uri: "ui://app/main", name: "Dashboard" }, text: "<html>app</html>" }],
  prompts: [
    { def: { name: "greet", description: "Greet someone" }, get: ({ who }) => ({ messages: [{ role: "user", content: { type: "text", text: `Hello ${who}` } }] }) },
  ],
});

const beta = await startMockUpstream({
  name: "beta-server",
  mode: "sse",
  tools: [{ def: { name: "echo", description: "Echo over SSE", inputSchema: { type: "object" } }, run: ({ text: t }) => text(`beta:${t}`) }],
});

const relayToml = `
[relay]
machine_name = "e2e"
request_timeout_secs = 5

[[endpoints]]
name = "alpha"
description = "Alpha"
transport = "http"
url = "${alpha.url}"
headers = { Authorization = "Bearer $ALPHA_TOKEN" }

[[endpoints]]
name = "beta"
transport = "http"
url = "${beta.url}"

[[endpoints]]
name = "down"
transport = "http"
url = "http://127.0.0.1:9/mcp"

[[endpoints]]
name = "local-files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[profiles]]
name = "Solo"
path = "solo"
endpoints = ["beta"]
js_execution = false
toon_output = false
`;

const mf = new Miniflare({
  modules: true,
  scriptPath: "build/index.js",
  modulesRules: [{ type: "CompiledWasm", include: ["**/*.wasm"] }],
  compatibilityDate: "2026-10-01",
  kvNamespaces: ["RELAY_CACHE"],
  bindings: { RELAY_CONFIG: relayToml, RELAY_AUTH_TOKEN: AUTH, ALPHA_TOKEN },
});
const base = (await mf.ready).toString().replace(/\/$/, "");

let nextId = 1;
const transcript = [];
async function rpc(method, params, { path = "/mcp", token = AUTH, raw } = {}) {
  const body = raw ?? { jsonrpc: "2.0", id: nextId++, method, ...(params ? { params } : {}) };
  const res = await fetch(base + path, {
    method: "POST",
    headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify(body),
  });
  const textBody = await res.text();
  const json = textBody ? JSON.parse(textBody) : null;
  transcript.push({ path, request: body, status: res.status, response: json });
  return { status: res.status, json };
}
const toolText = (r) => r.json?.result?.content?.[0]?.text;

const checks = [];
async function check(name, fn) {
  try {
    await fn();
    checks.push({ name, ok: true });
  } catch (e) {
    checks.push({ name, ok: false, error: String(e?.message ?? e) });
  }
}
function expect(cond, msg) {
  if (!cond) throw new Error(msg);
}
const eq = (a, b, label) => expect(JSON.stringify(a) === JSON.stringify(b), `${label}: expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);

await check("healthz reports served and skipped endpoints", async () => {
  const res = await fetch(`${base}/healthz`);
  const body = await res.json();
  transcript.push({ path: "/healthz", status: res.status, response: body });
  eq(body.endpoints, ["alpha", "beta", "down"], "endpoints");
  eq(body.skipped.map((s) => s.name), ["local-files"], "skipped");
});

await check("missing bearer token is rejected", async () => {
  const r = await rpc("ping", null, { token: null });
  eq(r.status, 401, "status");
});

await check("wrong bearer token is rejected", async () => {
  const r = await rpc("ping", null, { token: "nope" });
  eq(r.status, 401, "status");
});

await check("GET /mcp returns 405", async () => {
  const res = await fetch(`${base}/mcp`, { headers: { authorization: `Bearer ${AUTH}` } });
  eq(res.status, 405, "status");
});

await check("initialize returns relay identity", async () => {
  const r = await rpc("initialize", { protocolVersion: "2025-03-26", capabilities: {}, clientInfo: { name: "e2e", version: "1" } });
  eq(r.json.result.serverInfo.name, "Endara Relay (Cloudflare)", "serverInfo.name");
  eq(r.json.result.protocolVersion, "2025-03-26", "protocolVersion");
});

await check("notification gets 202", async () => {
  const r = await rpc(null, null, { raw: { jsonrpc: "2.0", method: "notifications/initialized" } });
  eq(r.status, 202, "status");
});

let toolNames = [];
await check("tools/list merges prefixed tools across pages and skips the dead upstream", async () => {
  const r = await rpc("tools/list");
  toolNames = r.json.result.tools.map((t) => t.name);
  eq(toolNames, ["alpha__add", "alpha__echo", "alpha__show_app", "beta__echo", "list_tools", "search_tools"], "names");
  const add = r.json.result.tools.find((t) => t.name === "alpha__add");
  eq(add.description, "[Alpha] Add two numbers", "label");
  const app = r.json.result.tools.find((t) => t.name === "alpha__show_app");
  expect(app._meta.ui.resourceUri.startsWith("mcp-relay://alpha/"), `ui pointer not wrapped: ${app._meta.ui.resourceUri}`);
});

await check("second tools/list is served from cache", async () => {
  const before = alpha.stats["tools/list"];
  await rpc("tools/list");
  eq(alpha.stats["tools/list"], before, "upstream tools/list count");
});

await check("tools/call routes by prefix (JSON upstream with auth header)", async () => {
  const r = await rpc("tools/call", { name: "alpha__add", arguments: { a: 2, b: 3 } });
  eq(toolText(r), "5", "result");
});

await check("tools/call over an SSE upstream skips interleaved notifications", async () => {
  const r = await rpc("tools/call", { name: "beta__echo", arguments: { text: "hi" } });
  eq(toolText(r), "beta:hi", "result");
});

await check("tool result resource links are wrapped", async () => {
  const r = await rpc("tools/call", { name: "alpha__show_app", arguments: {} });
  const uri = r.json.result.content[0].uri;
  expect(uri.startsWith("mcp-relay://alpha/"), `not wrapped: ${uri}`);
});

await check("expired upstream session is re-initialized transparently", async () => {
  const before = alpha.stats.initialize;
  await fetch(`${alpha.base}/admin/expire`, { method: "POST" });
  const r = await rpc("tools/call", { name: "alpha__echo", arguments: { text: "again" } });
  eq(toolText(r), "alpha:again", "result");
  eq(alpha.stats.initialize, before + 1, "initialize count");
});

await check("unknown tool name returns a JSON-RPC error", async () => {
  const r = await rpc("tools/call", { name: "nope__tool", arguments: {} });
  expect(r.json.error?.message.includes("no tool found"), JSON.stringify(r.json));
});

await check("call to a dead upstream returns a JSON-RPC error", async () => {
  const r = await rpc("tools/call", { name: "down__anything", arguments: {} });
  expect(r.json.error?.code === -32603, JSON.stringify(r.json));
});

await check("search_tools ranks the name match first", async () => {
  const r = await rpc("tools/call", { name: "search_tools", arguments: { query: "add" } });
  const results = JSON.parse(toolText(r));
  eq(results[0].name, "alpha__add", "top hit");
});

await check("list_tools paginates", async () => {
  const r = await rpc("tools/call", { name: "list_tools", arguments: { limit: 2, offset: 1 } });
  const page = JSON.parse(toolText(r));
  eq(page.total, 4, "total");
  eq(page.tools.map((t) => t.name), ["alpha__echo", "alpha__show_app"], "page");
});

await check("resources/list wraps URIs and resources/read unwraps them", async () => {
  const list = await rpc("resources/list");
  const uri = list.json.result.resources[0].uri;
  expect(uri.startsWith("mcp-relay://alpha/"), `not wrapped: ${uri}`);
  const read = await rpc("resources/read", { uri });
  eq(read.json.result.contents[0].text, "<html>app</html>", "contents");
});

await check("prompts are namespaced and routed", async () => {
  const list = await rpc("prompts/list");
  eq(list.json.result.prompts.map((p) => p.name), ["alpha__greet"], "prompts");
  const get = await rpc("prompts/get", { name: "alpha__greet", arguments: { who: "edge" } });
  eq(get.json.result.messages[0].content.text, "Hello edge", "prompt text");
});

await check("profile with one endpoint passes names through unprefixed", async () => {
  const list = await rpc("tools/list", null, { path: "/mcp/solo" });
  eq(list.json.result.tools.map((t) => t.name), ["echo", "list_tools", "search_tools"], "names");
  const call = await rpc("tools/call", { name: "echo", arguments: { text: "solo" } }, { path: "/mcp/solo" });
  eq(toolText(call), "beta:solo", "result");
});

await check("unknown profile is 404", async () => {
  const r = await rpc("ping", null, { path: "/mcp/missing" });
  eq(r.status, 404, "status");
});

await check("batch returns one reply per request and none for notifications", async () => {
  const r = await rpc(null, null, {
    raw: [
      { jsonrpc: "2.0", id: 900, method: "ping" },
      { jsonrpc: "2.0", method: "notifications/initialized" },
      { jsonrpc: "2.0", id: 901, method: "tools/call", params: { name: "alpha__add", arguments: { a: 40, b: 2 } } },
    ],
  });
  eq(r.json.map((m) => m.id), [900, 901], "ids");
  eq(r.json[1].result.content[0].text, "42", "batched call");
});

await check("parse error returns -32700", async () => {
  const res = await fetch(`${base}/mcp`, { method: "POST", headers: { authorization: `Bearer ${AUTH}` }, body: "{not json" });
  const body = await res.json();
  eq(body.error.code, -32700, "code");
});

await check("healthz probe reports per-endpoint reachability", async () => {
  const res = await fetch(`${base}/healthz?probe=1`, { headers: { authorization: `Bearer ${AUTH}` } });
  const body = await res.json();
  transcript.push({ path: "/healthz?probe=1", status: res.status, response: body });
  eq(body.probe.map((p) => `${p.name}:${p.status}`), ["alpha:ok", "beta:ok", "down:error"], "probe");
});

await mf.dispose();
await alpha.close();
await beta.close();

const passed = checks.filter((c) => c.ok).length;
mkdirSync("e2e/out", { recursive: true });
writeFileSync(
  "e2e/out/report.json",
  JSON.stringify({ passed, total: checks.length, checks, upstreamStats: { alpha: alpha.stats, beta: beta.stats }, transcript }, null, 2),
);
for (const c of checks) console.log(`${c.ok ? "PASS" : "FAIL"}  ${c.name}${c.ok ? "" : `\n      ${c.error}`}`);
console.log(`\n${passed}/${checks.length} checks passed — report: e2e/out/report.json`);
process.exit(passed === checks.length ? 0 : 1);
