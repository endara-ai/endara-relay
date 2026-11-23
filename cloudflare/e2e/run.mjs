// End-to-end run of the built Worker (build/index.js) in Miniflare
// against two mock MCP upstreams. Writes e2e/out/report.json.
// Usage: npm run build && npm run test:e2e
import workerd from "workerd";
import { mkdirSync, writeFileSync } from "node:fs";
import { startMockOAuthUpstream, startMockUpstream, text } from "./mock-upstream.mjs";

// worker-build 0.9 output needs a newer runtime than Miniflare bundles; use
// the pinned `workerd` package (the same one Alchemy dev uses).
process.env.MINIFLARE_WORKERD_PATH ??= workerd.default;
const { Miniflare } = await import("miniflare");

const AUTH = "e2e-relay-token";
// Keep the relay and mocks running after the checks (for screenshots).
const SERVE = process.env.E2E_SERVE === "1";
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

const gamma = await startMockOAuthUpstream({ name: "gamma-server" });
const noPkce = await startMockOAuthUpstream({ name: "no-pkce-server", advertisePkce: false });

const mf = new Miniflare({
  modules: true,
  scriptPath: "build/index.js",
  modulesRules: [{ type: "CompiledWasm", include: ["**/*.wasm"] }],
  compatibilityDate: "2026-10-01",
  kvNamespaces: ["RELAY_CACHE"],
  durableObjects: { RELAY_COORDINATOR: "Coordinator" },
  bindings: { RELAY_AUTH_TOKEN: AUTH, RELAY_ENCRYPTION_KEY: "e2e-encryption-key", RELAY_ALLOW_INSECURE_OAUTH: "1" },
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

async function apiCall(method, path, body, token = AUTH) {
  const res = await fetch(`${base}/api/${path}`, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const json = await res.json().catch(() => null);
  transcript.push({ path: `/api/${path}`, method, request: body, status: res.status, response: json });
  return { status: res.status, json };
}

const initialConfig = {
  revision: 0,
  settings: { machine_name: "e2e", toon_output: false, cache_ttl_secs: 300, request_timeout_secs: 5 },
  endpoints: [
    { id: "alpha", name: "alpha", description: "Alpha", url: alpha.url, headers: [{ name: "Authorization", value: `Bearer ${ALPHA_TOKEN}` }], enabled: true, disabled_tools: [] },
    { id: "beta", name: "beta", url: beta.url, headers: [], enabled: true, disabled_tools: [] },
    { id: "down", name: "down", url: "http://127.0.0.1:9/mcp", headers: [], enabled: true, disabled_tools: [] },
  ],
  profiles: [{ id: "solo", name: "Solo", path: "solo", endpoint_ids: ["beta"], toon_output: null }],
};

await check("web app config API saves servers and profiles", async () => {
  const r = await apiCall("PUT", "config", initialConfig);
  eq(r.status, 200, "status");
  eq(r.json.revision, 1, "revision");
  eq(r.json.endpoints.map((e) => e.name), ["alpha", "beta", "down"], "servers");
});

await check("healthz is public and reveals nothing", async () => {
  const res = await fetch(`${base}/healthz`);
  eq(await res.json(), { status: "ok" }, "body");
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

await check("config API requires the bearer token", async () => {
  eq((await apiCall("GET", "config", null, null)).status, 401, "no token");
  eq((await apiCall("GET", "config", null, "wrong")).status, 401, "wrong token");
});

let current;
await check("config API never returns upstream secrets, only a masked preview", async () => {
  const r = await apiCall("GET", "config");
  current = r.json;
  expect(!JSON.stringify(r.json).includes(ALPHA_TOKEN), "secret leaked in API response");
  eq(r.json.endpoints[0].headers, [{ name: "Authorization", preview: "Bearer ••••cret" }], "headers");
});

await check("secrets are encrypted at rest in KV", async () => {
  const kv = await mf.getKVNamespace("RELAY_CACHE");
  const raw = await kv.get("config");
  expect(raw && raw.includes('"enc"'), "no encrypted header in stored config");
  expect(!raw.includes(ALPHA_TOKEN), "plaintext secret stored in KV");
});

await check("saving without a header value keeps the stored secret", async () => {
  const next = { ...current, endpoints: current.endpoints.map((e) => ({ ...e, description: e.id === "alpha" ? "Alpha renamed" : e.description, headers: e.headers.map((hd) => ({ name: hd.name })) })) };
  const r = await apiCall("PUT", "config", next);
  eq(r.status, 200, "status");
  current = r.json;
  const call = await rpc("tools/call", { name: "alpha__add", arguments: { a: 1, b: 1 } });
  eq(toolText(call), "2", "call still authenticates");
});

await check("stale revision is rejected with 409", async () => {
  const r = await apiCall("PUT", "config", { ...initialConfig, revision: 0 });
  eq(r.status, 409, "status");
});

await check("invalid config is rejected with a clear message", async () => {
  const dup = { ...current, endpoints: [...current.endpoints.map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) })), { name: "beta", url: beta.url, headers: [], enabled: true, disabled_tools: [] }] };
  const r = await apiCall("PUT", "config", dup);
  eq(r.status, 400, "status");
  expect(r.json.error.includes("two servers are named 'beta'"), r.json.error);
  const badUrl = { ...current, endpoints: [{ name: "x", url: "ftp://nope", headers: [], enabled: true, disabled_tools: [] }] };
  eq((await apiCall("PUT", "config", badUrl)).status, 400, "bad url");
});

await check("status lists each server's tools or error", async () => {
  const r = await apiCall("GET", "status?fresh=1");
  const byId = Object.fromEntries(r.json.servers.map((s) => [s.id, s]));
  eq(byId.alpha.tools.map((t) => t.name), ["add", "echo", "show_app"], "alpha tools");
  eq(byId.beta.status, "ok", "beta");
  eq(byId.down.status, "error", "down");
});

await check("test connection works with a stored secret and reports auth failures", async () => {
  const ok = await apiCall("POST", "test", { id: "alpha", url: alpha.url, headers: [{ name: "Authorization" }] });
  eq(ok.json.ok, true, "ok");
  eq(ok.json.more, true, "has a second page");
  const bad = await apiCall("POST", "test", { url: alpha.url, headers: [{ name: "Authorization", value: "Bearer nope" }] });
  eq(bad.json.ok, false, "bad ok");
  expect(bad.json.error.includes("401"), bad.json.error);
});

await check("disabling a server and a tool removes them from the catalog", async () => {
  const next = {
    ...current,
    endpoints: current.endpoints.map((e) => ({
      ...e,
      headers: e.headers.map((hd) => ({ name: hd.name })),
      enabled: e.id !== "down",
      disabled_tools: e.id === "alpha" ? ["echo"] : [],
    })),
  };
  current = (await apiCall("PUT", "config", next)).json;
  const r = await rpc("tools/list");
  eq(r.json.result.tools.map((t) => t.name), ["alpha__add", "alpha__show_app", "beta__echo", "list_tools", "search_tools"], "names");
});

// ---- OAuth (dynamic client registration) ----------------------------------

const withGamma = (cfg, extra = {}) => ({
  ...cfg,
  endpoints: [
    ...cfg.endpoints.filter((e) => e.id !== "gamma").map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) })),
    { id: "gamma", name: "gamma", url: gamma.url, headers: [], enabled: true, disabled_tools: [], oauth: true, ...extra },
  ],
});
const gammaStatus = async () => (await apiCall("GET", "status?fresh=1")).json.servers.find((x) => x.id === "gamma");
async function signInGamma() {
  const start = await apiCall("POST", "oauth/start", { id: "gamma" });
  expect(start.status === 200, `start: ${JSON.stringify(start.json)}`);
  const auth = await fetch(start.json.authorization_url, { redirect: "manual" });
  expect(auth.status === 302, `authorize: ${auth.status} ${await auth.text()}`);
  const callback = auth.headers.get("location");
  const page = await fetch(callback);
  const html = await page.text();
  transcript.push({ path: "/oauth/callback", status: page.status, response: html });
  return { start: start.json, callback, html };
}

await check("an OAuth server starts as 'sign in needed' and its tools stay hidden", async () => {
  current = (await apiCall("PUT", "config", withGamma(current))).json;
  eq((await gammaStatus()).status, "needs_sign_in", "status");
  const list = await rpc("tools/list");
  expect(!list.json.result.tools.some((t) => t.name.startsWith("gamma__")), "gamma tools listed before sign-in");
  const call = await rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  expect(call.json.error?.message.includes("needs sign-in"), JSON.stringify(call.json));
});

let firstSignIn;
await check("sign-in discovers metadata, registers a client and sends PKCE + resource", async () => {
  firstSignIn = await signInGamma();
  const u = new URL(firstSignIn.start.authorization_url);
  eq(u.origin + u.pathname, `${gamma.base}/auth/authorize`, "authorize endpoint");
  eq(u.searchParams.get("code_challenge_method"), "S256", "pkce");
  eq(u.searchParams.get("resource"), gamma.url, "resource");
  eq(u.searchParams.get("scope"), "mcp:read", "scope from WWW-Authenticate");
  eq(u.searchParams.get("redirect_uri"), `${base}/oauth/callback`, "redirect_uri");
  expect(firstSignIn.html.includes("Signed in to gamma"), firstSignIn.html);
  eq(gamma.stats.register, 1, "registrations");
});

await check("a sign-in link works only once", async () => {
  const html = await (await fetch(firstSignIn.callback)).text();
  expect(html.includes("expired or was already used"), html);
});

await check("after sign-in the server's tools are listed and callable", async () => {
  const st = await gammaStatus();
  eq(st.status, "ok", "status");
  eq(st.oauth.signed_in, true, "signed_in");
  const call = await rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  eq(toolText(call), "user-1", "result");
});

// The raw stored record, read through the coordinator's own API.
async function storedOAuthRecord(id) {
  const ns = await mf.getDurableObjectNamespace("RELAY_COORDINATOR");
  const res = await ns.get(ns.idFromName("relay")).fetch("https://coordinator/oauth/get", { method: "POST", body: JSON.stringify({ id }) });
  return JSON.stringify((await res.json()).record);
}

await check("OAuth tokens are encrypted at rest", async () => {
  const raw = await storedOAuthRecord("gamma");
  expect(raw && raw.includes("access_token_enc"), "no oauth record");
  expect(!raw.includes(gamma.state.lastAccessToken), "plaintext access token in KV");
});

await check("a rejected access token is refreshed once and the call succeeds", async () => {
  const before = gamma.stats.refresh;
  await fetch(`${gamma.base}/admin/revoke-access`, { method: "POST" });
  const call = await rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  eq(toolText(call), "user-1", "result");
  eq(gamma.stats.refresh, before + 1, "refreshes");
});

await check("a revoked refresh token turns into 'sign in again'", async () => {
  await fetch(`${gamma.base}/admin/revoke-all`, { method: "POST" });
  const call = await rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  expect(call.json.error?.message.includes("sign-in again"), JSON.stringify(call.json));
  eq((await gammaStatus()).status, "needs_sign_in", "status");
});

await check("signing in again reuses the registered client", async () => {
  const again = await signInGamma();
  expect(again.html.includes("Signed in to gamma"), again.html);
  eq(gamma.stats.register, 1, "registrations");
  eq(toolText(await rpc("tools/call", { name: "gamma__whoami", arguments: {} })), "user-1", "result");
});

await check("a sign-in stored in KV by the previous version is migrated once", async () => {
  const ns = await mf.getDurableObjectNamespace("RELAY_COORDINATOR");
  const coord = ns.get(ns.idFromName("relay"));
  const record = JSON.parse(await storedOAuthRecord("gamma"));
  await coord.fetch("https://coordinator/oauth/forget", { method: "POST", body: JSON.stringify({ id: "gamma" }) });
  const kv = await mf.getKVNamespace("RELAY_CACHE");
  await kv.put("oauth:gamma", JSON.stringify(record));
  eq((await gammaStatus()).oauth.signed_in, true, "still signed in");
  eq(await kv.get("oauth:gamma"), null, "legacy KV copy deleted");
  eq(JSON.parse(await storedOAuthRecord("gamma")).grant_id, record.grant_id, "same grant in the coordinator");
});

await check("parallel calls that all hit an expired token share one refresh", async () => {
  const before = gamma.stats.refresh;
  await fetch(`${gamma.base}/admin/revoke-access`, { method: "POST" });
  const calls = await Promise.all(Array.from({ length: 5 }, () => rpc("tools/call", { name: "gamma__whoami", arguments: {} })));
  eq(calls.map(toolText), Array(5).fill("user-1"), "results");
  eq(gamma.stats.refresh, before + 1, "refreshes");
});

await check("a sign-out during a slow refresh wins; the refreshed token is discarded", async () => {
  await fetch(`${gamma.base}/admin/revoke-access`, { method: "POST" });
  await fetch(`${gamma.base}/admin/token-delay?ms=1500`, { method: "POST" });
  const call = rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  await new Promise((r) => setTimeout(r, 400));
  eq((await apiCall("POST", "oauth/sign-out", { id: "gamma" })).status, 200, "sign-out");
  await call;
  await fetch(`${gamma.base}/admin/token-delay?ms=0`, { method: "POST" });
  eq((await gammaStatus()).status, "needs_sign_in", "status after sign-out");
  const next = await rpc("tools/call", { name: "gamma__whoami", arguments: {} });
  expect(next.json.error?.message.includes("sign"), JSON.stringify(next.json));
});

await check("the same sign-in callback used twice at once succeeds only once", async () => {
  const start = await apiCall("POST", "oauth/start", { id: "gamma" });
  const auth = await fetch(start.json.authorization_url, { redirect: "manual" });
  const callback = auth.headers.get("location");
  const pages = await Promise.all([fetch(callback).then((r) => r.text()), fetch(callback).then((r) => r.text())]);
  eq(pages.filter((p) => p.includes("Signed in to gamma")).length, 1, "successes");
  eq(pages.filter((p) => p.includes("expired or was already used")).length, 1, "rejections");
});

await check("sign-out drops the tokens and the cached catalog", async () => {
  const before = await rpc("tools/list");
  expect(before.json.result.tools.some((t) => t.name === "gamma__whoami"), "gamma listed while signed in");
  eq((await apiCall("POST", "oauth/sign-out", { id: "gamma" })).status, 200, "status");
  eq((await gammaStatus()).status, "needs_sign_in", "after sign-out");
  const after = await rpc("tools/list");
  expect(!after.json.result.tools.some((t) => t.name.startsWith("gamma__")), "cached gamma catalog served after sign-out");
});

await check("sign-in is refused when the server does not advertise PKCE S256", async () => {
  const plain = current.endpoints.map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) }));
  current = (await apiCall("PUT", "config", { ...current, endpoints: [...plain, { id: "nopkce", name: "nopkce", url: noPkce.url, headers: [], enabled: true, disabled_tools: [], oauth: true }] })).json;
  const r = await apiCall("POST", "oauth/start", { id: "nopkce" });
  eq(r.status, 502, "status");
  expect(r.json.error.includes("PKCE S256"), r.json.error);
  eq(noPkce.stats.register, 0, "no client registered");
  current = (await apiCall("PUT", "config", { ...current, endpoints: current.endpoints.filter((e) => e.id !== "nopkce").map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) })) })).json;
});

await check("OAuth start refuses servers that are not saved as OAuth", async () => {
  eq((await apiCall("POST", "oauth/start", { id: "beta" })).status, 400, "non-oauth");
  eq((await apiCall("POST", "oauth/start", { id: "nope" })).status, 404, "unknown");
  eq((await apiCall("POST", "oauth/start", { id: "gamma" }, null)).status, 401, "no token");
});

await check("of parallel saves on the same revision exactly one succeeds", async () => {
  const fresh = (await apiCall("GET", "config")).json;
  const plain = fresh.endpoints.map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) }));
  const saves = await Promise.all(
    Array.from({ length: 5 }, (_, i) => apiCall("PUT", "config", { ...fresh, settings: { ...fresh.settings, machine_name: `race-${i}` }, endpoints: plain })),
  );
  eq(saves.filter((r) => r.status === 200).length, 1, "successes");
  eq(saves.filter((r) => r.status === 409).length, 4, "conflicts");
  current = (await apiCall("GET", "config")).json;
  eq(current.revision, fresh.revision + 1, "revision");
});

await check("ambiguous tool prefixes and duplicate server ids are rejected", async () => {
  const plain = (cfg) => cfg.endpoints.map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) }));
  const trailing = { ...current, endpoints: [...plain(current), { name: "extra", tool_prefix: "a_", url: beta.url, headers: [], enabled: true, disabled_tools: [] }] };
  const r1 = await apiCall("PUT", "config", trailing);
  eq(r1.status, 400, "trailing underscore");
  expect(r1.json.error.includes("must not start or end with '_'"), r1.json.error);
  const dupId = { ...current, endpoints: [...plain(current), { id: "beta", name: "beta2", url: beta.url, headers: [], enabled: true, disabled_tools: [] }] };
  const r2 = await apiCall("PUT", "config", dupId);
  eq(r2.status, 400, "duplicate id");
  expect(r2.json.error.includes("share the id 'beta'"), r2.json.error);
});

await check("an SSE reply is used as soon as it arrives, even if the stream stays open", async () => {
  const started = Date.now();
  const r = await rpc("tools/call", { name: "beta__echo", arguments: { text: "open", linger: true } });
  eq(toolText(r), "beta:open", "result");
  expect(Date.now() - started < 4000, `took ${Date.now() - started} ms (request timeout is 5 s, stream stays open 8 s)`);
});

await check("web app is served with a strict CSP", async () => {
  const res = await fetch(`${base}/`);
  eq(res.status, 200, "status");
  expect((res.headers.get("content-security-policy") || "").includes("script-src 'self'"), "csp");
  expect((await res.text()).includes("/app.js"), "html");
  eq((await fetch(`${base}/app.js`)).status, 200, "app.js");
});

if (!SERVE) {
  await mf.dispose();
  await alpha.close();
  await beta.close();
  await gamma.close();
  await noPkce.close();
}

const passed = checks.filter((c) => c.ok).length;
mkdirSync("e2e/out", { recursive: true });
writeFileSync(
  "e2e/out/report.json",
  JSON.stringify({ passed, total: checks.length, checks, upstreamStats: { alpha: alpha.stats, beta: beta.stats, gamma: gamma.stats }, transcript }, null, 2),
);
for (const c of checks) console.log(`${c.ok ? "PASS" : "FAIL"}  ${c.name}${c.ok ? "" : `\n      ${c.error}`}`);
console.log(`\n${passed}/${checks.length} checks passed — report: e2e/out/report.json`);
if (SERVE) console.log(`\nServing the relay at ${base}/ (token: ${AUTH}). Ctrl+C to stop.`);
else process.exit(passed === checks.length ? 0 : 1);
