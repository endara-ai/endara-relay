// Minimal streamable-HTTP MCP servers for the E2E run.
// mode "json": issues Mcp-Session-Id, answers application/json, requires a bearer token.
// mode "sse": stateless, answers every request as a text/event-stream.
import http from "node:http";
import { randomUUID } from "node:crypto";

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
