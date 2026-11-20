// Minimal streamable-HTTP MCP servers for the E2E run.
// mode "json": issues Mcp-Session-Id, answers application/json, requires a bearer token.
// mode "sse": stateless, answers every request as a text/event-stream.
import http from "node:http";
import { createHash, randomUUID } from "node:crypto";

export function startMockUpstream({ name, mode, token, tools, resources = [], prompts = [] }) {
  const stats = { initialize: 0, "tools/list": 0, "tools/call": 0, requests: 0 };
  const sessions = new Set();

  const handle = (msg) => {
    const { id, method, params = {} } = msg;
    if (id === undefined) return null;
    stats[method] = (stats[method] ?? 0) + 1;
    const ok = (result) => ({ jsonrpc: "2.0", id, result });
    const err = (code, message) => ({ jsonrpc: "2.0", id, error: { code, message } });
    switch (method) {
      case "initialize":
        return ok({
          protocolVersion: "2025-06-18",
          capabilities: { tools: {}, resources: {}, prompts: {} },
          serverInfo: { name, version: "1.0.0" },
        });
      case "ping":
        return ok({});
      case "tools/list": {
        // Two pages to exercise nextCursor handling.
        const half = Math.ceil(tools.length / 2);
        return params.cursor === "p2"
          ? ok({ tools: tools.slice(half).map((t) => t.def) })
          : ok({ tools: tools.slice(0, half).map((t) => t.def), nextCursor: half < tools.length ? "p2" : undefined });
      }
      case "tools/call": {
        const tool = tools.find((t) => t.def.name === params.name);
        if (!tool) return err(-32602, `unknown tool ${params.name}`);
        return ok(tool.run(params.arguments ?? {}));
      }
      case "resources/list":
        return resources.length ? ok({ resources: resources.map((r) => r.def) }) : err(-32601, "Method not found");
      case "resources/templates/list":
        return err(-32601, "Method not found");
      case "resources/read": {
        const r = resources.find((r) => r.def.uri === params.uri);
        return r ? ok({ contents: [{ uri: r.def.uri, text: r.text }] }) : err(-32602, `unknown resource ${params.uri}`);
      }
      case "prompts/list":
        return prompts.length ? ok({ prompts: prompts.map((p) => p.def) }) : err(-32601, "Method not found");
      case "prompts/get": {
        const p = prompts.find((p) => p.def.name === params.name);
        return p ? ok(p.get(params.arguments ?? {})) : err(-32602, `unknown prompt ${params.name}`);
      }
      default:
        return err(-32601, "Method not found");
    }
  };

  const server = http.createServer(async (req, res) => {
    if (req.method === "POST" && req.url === "/admin/expire") {
      sessions.clear();
      res.writeHead(204).end();
      return;
    }
    if (req.method !== "POST" || req.url !== "/mcp") {
      res.writeHead(404).end();
      return;
    }
    if (token && req.headers.authorization !== `Bearer ${token}`) {
      res.writeHead(401, { "content-type": "application/json" }).end('{"error":"unauthorized"}');
      return;
    }
    let body = "";
    for await (const chunk of req) body += chunk;
    stats.requests++;
    const msg = JSON.parse(body);
    const headers = {};

    if (mode === "json") {
      if (msg.method === "initialize") {
        const sid = randomUUID();
        sessions.add(sid);
        headers["mcp-session-id"] = sid;
      } else if (!sessions.has(req.headers["mcp-session-id"])) {
        res.writeHead(404, { "content-type": "application/json" }).end('{"error":"session not found"}');
        return;
      }
    }

    const reply = handle(msg);
    if (!reply) {
      res.writeHead(202, headers).end();
      return;
    }
    if (mode === "sse") {
      res.writeHead(200, { ...headers, "content-type": "text/event-stream" });
      if (msg.params?.arguments?.linger) {
        // Reply, then keep the stream open like a server that sends more events later.
        res.write(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
        setTimeout(() => res.end(), 8000);
        return;
      }
      // A server-initiated notification first: the relay must skip it.
      res.write(`event: message\ndata: ${JSON.stringify({ jsonrpc: "2.0", method: "notifications/message", params: { level: "info", data: "working" } })}\n\n`);
      res.end(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
    } else {
      res.writeHead(200, { ...headers, "content-type": "application/json" }).end(JSON.stringify(reply));
    }
  });

  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () =>
      resolve({
        url: `http://127.0.0.1:${server.address().port}/mcp`,
        base: `http://127.0.0.1:${server.address().port}`,
        stats,
        close: () => new Promise((r) => server.close(r)),
      }),
    ),
  );
}

export const text = (t) => ({ content: [{ type: "text", text: String(t) }] });

// An MCP server protected by its own OAuth authorization server, supporting
// RFC 9728 discovery, RFC 8414 metadata, RFC 7591 dynamic registration, PKCE
// S256, RFC 8707 resource checks, RFC 9207 `iss` and refresh-token rotation.
export function startMockOAuthUpstream({ name, advertisePkce = true }) {
  const stats = { register: 0, authorize: 0, token: 0, refresh: 0, mcp: 0, unauthorized: 0 };
  const clients = new Map(); // client_id -> redirect_uris
  const codes = new Map(); // code -> { client_id, redirect_uri, challenge, resource }
  const access = new Set();
  const refresh = new Set();
  let base = "";
  const b64url = (buf) => Buffer.from(buf).toString("base64url");
  const sha256 = (s) => b64url(createHash("sha256").update(s).digest());
  const issue = () => {
    const a = `at-${randomUUID()}`;
    const r = `rt-${randomUUID()}`;
    access.add(a);
    refresh.add(r);
    state.lastAccessToken = a;
    return { access_token: a, refresh_token: r, token_type: "Bearer", expires_in: 3600 };
  };
  const state = { lastAccessToken: null };
  const json = (res, status, body, headers = {}) => res.writeHead(status, { "content-type": "application/json", ...headers }).end(JSON.stringify(body));

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, base);
    let body = "";
    for await (const chunk of req) body += chunk;

    if (req.method === "POST" && url.pathname === "/admin/revoke-access") {
      access.clear();
      return res.writeHead(204).end();
    }
    if (req.method === "POST" && url.pathname === "/admin/revoke-all") {
      access.clear();
      refresh.clear();
      return res.writeHead(204).end();
    }
    if (url.pathname === "/.well-known/oauth-protected-resource/mcp") {
      return json(res, 200, { resource: `${base}/mcp`, authorization_servers: [`${base}/auth`], scopes_supported: ["mcp:read"] });
    }
    if (url.pathname === "/.well-known/oauth-authorization-server/auth") {
      return json(res, 200, {
        issuer: `${base}/auth`,
        authorization_endpoint: `${base}/auth/authorize`,
        token_endpoint: `${base}/auth/token`,
        registration_endpoint: `${base}/auth/register`,
        ...(advertisePkce ? { code_challenge_methods_supported: ["S256"] } : {}),
        authorization_response_iss_parameter_supported: true,
      });
    }
    if (req.method === "POST" && url.pathname === "/auth/register") {
      stats.register++;
      const reg = JSON.parse(body);
      if (!Array.isArray(reg.redirect_uris) || reg.redirect_uris.length === 0) return json(res, 400, { error: "invalid_redirect_uri" });
      const client_id = `client-${randomUUID()}`;
      clients.set(client_id, reg.redirect_uris);
      return json(res, 201, { client_id, redirect_uris: reg.redirect_uris, token_endpoint_auth_method: "none" });
    }
    if (req.method === "GET" && url.pathname === "/auth/authorize") {
      stats.authorize++;
      const p = url.searchParams;
      const fail = (msg) => res.writeHead(400).end(msg);
      if (!clients.has(p.get("client_id"))) return fail("unknown client");
      if (!clients.get(p.get("client_id")).includes(p.get("redirect_uri"))) return fail("redirect_uri not registered");
      if (p.get("code_challenge_method") !== "S256" || !p.get("code_challenge")) return fail("PKCE S256 required");
      if (p.get("resource") !== `${base}/mcp`) return fail(`bad resource ${p.get("resource")}`);
      if (p.get("scope") !== "mcp:read") return fail(`bad scope ${p.get("scope")}`);
      const code = `code-${randomUUID()}`;
      codes.set(code, { client_id: p.get("client_id"), redirect_uri: p.get("redirect_uri"), challenge: p.get("code_challenge"), resource: p.get("resource") });
      const back = new URL(p.get("redirect_uri"));
      back.searchParams.set("code", code);
      back.searchParams.set("state", p.get("state"));
      back.searchParams.set("iss", `${base}/auth`);
      return res.writeHead(302, { location: back.toString() }).end();
    }
    if (req.method === "POST" && url.pathname === "/auth/token") {
      stats.token++;
      const f = new URLSearchParams(body);
      if (f.get("grant_type") === "authorization_code") {
        const c = codes.get(f.get("code"));
        codes.delete(f.get("code"));
        if (!c) return json(res, 400, { error: "invalid_grant", error_description: "unknown code" });
        if (c.client_id !== f.get("client_id") || c.redirect_uri !== f.get("redirect_uri")) return json(res, 400, { error: "invalid_grant", error_description: "client mismatch" });
        if (sha256(f.get("code_verifier") || "") !== c.challenge) return json(res, 400, { error: "invalid_grant", error_description: "PKCE mismatch" });
        if (f.get("resource") !== c.resource) return json(res, 400, { error: "invalid_target" });
        return json(res, 200, issue());
      }
      if (f.get("grant_type") === "refresh_token") {
        stats.refresh++;
        if (!refresh.delete(f.get("refresh_token"))) return json(res, 400, { error: "invalid_grant", error_description: "refresh token revoked" });
        return json(res, 200, issue());
      }
      return json(res, 400, { error: "unsupported_grant_type" });
    }
    if (req.method === "POST" && url.pathname === "/mcp") {
      stats.mcp++;
      const token = (req.headers.authorization || "").replace(/^Bearer /, "");
      if (!access.has(token)) {
        stats.unauthorized++;
        return json(res, 401, { error: "invalid_token" }, {
          "www-authenticate": `Bearer resource_metadata="${base}/.well-known/oauth-protected-resource/mcp", scope="mcp:read"`,
        });
      }
      const msg = JSON.parse(body);
      if (msg.id === undefined) return res.writeHead(202).end();
      const ok = (result) => json(res, 200, { jsonrpc: "2.0", id: msg.id, result });
      if (msg.method === "initialize") return ok({ protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name, version: "1" } });
      if (msg.method === "tools/list") return ok({ tools: [{ name: "whoami", description: "Who is signed in", inputSchema: { type: "object" } }] });
      if (msg.method === "tools/call") return ok(text("user-1"));
      return json(res, 200, { jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "Method not found" } });
    }
    res.writeHead(404).end();
  });

  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () => {
      base = `http://127.0.0.1:${server.address().port}`;
      resolve({ url: `${base}/mcp`, base, stats, state, close: () => new Promise((r) => server.close(r)) });
    }),
  );
}
