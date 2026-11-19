//! Endara Relay on Cloudflare Workers.
//!
//! Stateless streamable-HTTP MCP front door: `POST /mcp` (and
//! `/mcp/{profile}`) answers with plain JSON. Upstreams are remote MCP
//! servers over streamable HTTP; their sessions and catalogs are cached in
//! isolate memory and the optional `RELAY_CACHE` KV namespace.

mod cache;
mod config;
mod relay;
mod upstream;

#[allow(dead_code)]
#[path = "../../src/prefix.rs"]
mod prefix;
#[allow(dead_code)]
#[path = "../../src/protocol.rs"]
mod protocol;
#[allow(dead_code)]
#[path = "../../src/resource_uri.rs"]
mod resource_uri;
#[allow(dead_code)]
#[path = "../../src/tool_call_rewrite.rs"]
mod tool_call_rewrite;
#[allow(dead_code)]
#[path = "../../src/toon_convert.rs"]
mod toon_convert;

use cache::Cache;
use config::Config;
use relay::{Relay, Reply, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::rc::Rc;
use worker::{event, Context, Env, Headers, Method, Request, Response, Result};

const CONFIG_VAR: &str = "RELAY_CONFIG";
const TOKEN_SECRET: &str = "RELAY_AUTH_TOKEN";
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

thread_local! {
    static CONFIG: RefCell<Option<(String, Rc<Config>)>> = const { RefCell::new(None) };
}

fn env_string(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|s| s.to_string())
        .or_else(|_| env.var(name).map(|v| v.to_string()))
        .ok()
}

fn load_config(env: &Env) -> std::result::Result<Rc<Config>, String> {
    let raw = env_string(env, CONFIG_VAR).unwrap_or_default();
    let digest = hex(&Sha256::digest(raw.as_bytes()));
    if let Some(cfg) = CONFIG.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(d, _)| *d == digest)
            .map(|(_, cfg)| cfg.clone())
    }) {
        return Ok(cfg);
    }
    let cfg = Rc::new(Config::parse(&raw, &|name| env_string(env, name))?);
    CONFIG.with(|c| *c.borrow_mut() = Some((digest, cfg.clone())));
    Ok(cfg)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time bearer check (compares digests, so length leaks nothing).
fn authorized(req: &Request, env: &Env) -> std::result::Result<(), Response> {
    let Some(expected) = env_string(env, TOKEN_SECRET).filter(|t| !t.is_empty()) else {
        return Err(json_error(
            503,
            "relay is not configured: set the RELAY_AUTH_TOKEN secret",
        ));
    };
    let presented = req
        .headers()
        .get("authorization")
        .ok()
        .flatten()
        .and_then(|h| {
            let (scheme, token) = h.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| token.trim().to_string())
        })
        .unwrap_or_default();
    let a = Sha256::digest(expected.as_bytes());
    let b = Sha256::digest(presented.as_bytes());
    let diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    if diff == 0 {
        Ok(())
    } else {
        let mut resp = json_error(401, "missing or invalid bearer token");
        let _ = resp.headers_mut().set("www-authenticate", "Bearer");
        Err(resp)
    }
}

fn json_error(status: u16, message: &str) -> Response {
    Response::from_json(&json!({ "error": message }))
        .map(|r| r.with_status(status))
        .unwrap_or_else(|_| Response::empty().unwrap().with_status(status))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (req.method(), segments.as_slice()) {
        (Method::Get, ["healthz"]) => healthz(&req, &env).await,
        (_, ["mcp"]) | (_, ["mcp", _]) => {
            if let Err(resp) = authorized(&req, &env) {
                return Ok(resp);
            }
            if req.method() != Method::Post {
                let headers = Headers::new();
                headers.set("allow", "POST")?;
                return Ok(json_error(405, "this relay only accepts POST").with_headers(headers));
            }
            let config = match load_config(&env) {
                Ok(c) => c,
                Err(e) => return Ok(json_error(500, &e)),
            };
            let scope = match segments.get(1) {
                None => Scope::global(&config),
                Some(p) => match Scope::profile(&config, p) {
                    Some(s) => s,
                    None => return Ok(json_error(404, &format!("unknown profile '{p}'"))),
                },
            };
            let body = req.bytes().await?;
            if body.len() > MAX_BODY_BYTES {
                return Ok(json_error(413, "request body too large"));
            }
            let cache = Cache::new(&env);
            let relay = Relay {
                config: &config,
                cache: &cache,
            };
            handle_mcp(&relay, &scope, &body).await
        }
        _ => Ok(json_error(404, "not found")),
    }
}

async fn healthz(req: &Request, env: &Env) -> Result<Response> {
    let config = match load_config(env) {
        Ok(c) => c,
        Err(e) => return Ok(json_error(500, &e)),
    };
    let mut body = json!({
        "status": "ok",
        "endpoints": config.endpoints.iter().map(|e| &e.name).collect::<Vec<_>>(),
        "skipped": config.skipped.iter().map(|s| json!({ "name": s.name, "reason": s.reason })).collect::<Vec<_>>(),
        "profiles": config.profiles.iter().map(|p| json!({ "name": p.name, "path": p.path })).collect::<Vec<_>>(),
    });
    let wants_probe = req
        .url()
        .map(|u| u.query_pairs().any(|(k, v)| k == "probe" && v == "1"))
        .unwrap_or(false);
    // Probing calls upstreams, so it sits behind the bearer token.
    if wants_probe && authorized(req, env).is_ok() {
        let cache = Cache::new(env);
        let relay = Relay {
            config: &config,
            cache: &cache,
        };
        body["probe"] = Value::Array(relay.probe().await);
    }
    Response::from_json(&body)
}

async fn handle_mcp(relay: &Relay<'_>, scope: &Scope<'_>, body: &[u8]) -> Result<Response> {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Response::from_json(&rpc_error(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            ))
            .map(|r| r.with_status(400))
        }
    };
    match parsed {
        Value::Array(items) => {
            if items.is_empty() {
                return Response::from_json(&rpc_error(Value::Null, -32600, "empty batch"))
                    .map(|r| r.with_status(400));
            }
            let replies = futures_util::future::join_all(
                items.into_iter().map(|m| handle_message(relay, scope, m)),
            )
            .await;
            let replies: Vec<Value> = replies.into_iter().flatten().collect();
            if replies.is_empty() {
                Ok(Response::empty()?.with_status(202))
            } else {
                Response::from_json(&replies)
            }
        }
        msg => match handle_message(relay, scope, msg).await {
            Some(reply) => Response::from_json(&reply),
            None => Ok(Response::empty()?.with_status(202)),
        },
    }
}

/// `None` for notifications and client responses (nothing to send back).
async fn handle_message(relay: &Relay<'_>, scope: &Scope<'_>, msg: Value) -> Option<Value> {
    let Value::Object(mut obj) = msg else {
        return Some(rpc_error(Value::Null, -32600, "Invalid Request"));
    };
    let id = obj.remove("id");
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let (Some(id), Some(method)) = (id, method) else {
        return None;
    };
    let params = obj.remove("params").unwrap_or(json!({}));
    Some(match relay.dispatch(scope, &method, params).await {
        Reply::Result(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Reply::Error {
            code,
            message,
            data,
        } => {
            let mut err = json!({ "code": code, "message": message });
            if let Some(d) = data {
                err["data"] = d;
            }
            json!({ "jsonrpc": "2.0", "id": id, "error": err })
        }
    })
}
