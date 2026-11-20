//! Endara Relay on Cloudflare Workers.
//!
//! - `POST /mcp`, `/mcp/{profile}`: stateless streamable-HTTP MCP, plain JSON
//!   replies, aggregated across the configured upstream servers.
//! - `/`: the web app that edits the configuration (stored in KV).
//! - `/api/*`: the web app's JSON API.
//!
//! Everything except `/healthz` and the static web app requires
//! `Authorization: Bearer <RELAY_AUTH_TOKEN>`.

mod api;
mod cache;
mod config;
mod coordinator;
mod oauth;
mod relay;
mod store;
mod ui;
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
use store::Crypto;
use worker::{event, Context, Date, Env, Headers, KvStore, Method, Request, Response, Result};

const TOKEN_SECRET: &str = "RELAY_AUTH_TOKEN";
const ENCRYPTION_SECRET: &str = "RELAY_ENCRYPTION_KEY";
/// Allows http:// OAuth endpoints. Only for local testing.
const INSECURE_OAUTH_VAR: &str = "RELAY_ALLOW_INSECURE_OAUTH";
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Isolate-local config reuse; saves from this isolate apply immediately.
const CONFIG_MEMORY_MS: u64 = 5_000;

thread_local! {
    static CONFIG: RefCell<Option<(u64, Rc<Config>)>> = const { RefCell::new(None) };
}

pub(crate) fn remember_config(config: Rc<Config>) {
    let exp = Date::now().as_millis() + CONFIG_MEMORY_MS;
    CONFIG.with(|c| *c.borrow_mut() = Some((exp, config)));
}

fn env_string(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|s| s.to_string())
        .or_else(|_| env.var(name).map(|v| v.to_string()))
        .ok()
        .filter(|s| !s.is_empty())
}

pub(crate) fn json_error(status: u16, message: &str) -> Response {
    Response::from_json(&json!({ "error": message }))
        .map(|r| r.with_status(status))
        .unwrap_or_else(|_| Response::empty().unwrap().with_status(status))
}

/// Per-request handles to the bindings every authenticated route needs.
pub(crate) struct Ctx {
    pub env: Env,
    pub kv: KvStore,
    pub crypto: Crypto,
    pub coord: coordinator::CoordinatorClient,
}

impl Ctx {
    fn new(env: Env) -> std::result::Result<Ctx, Response> {
        let kv = env.kv(cache::KV_BINDING).map_err(|_| {
            json_error(
                503,
                "relay is not configured: missing the RELAY_CACHE KV binding",
            )
        })?;
        let key = env_string(&env, ENCRYPTION_SECRET).ok_or_else(|| {
            json_error(
                503,
                "relay is not configured: set the RELAY_ENCRYPTION_KEY secret",
            )
        })?;
        let coord = coordinator::CoordinatorClient::new(&env).map_err(|e| json_error(503, &e))?;
        Ok(Ctx {
            crypto: Crypto::new(&key),
            coord,
            env,
            kv,
        })
    }

    pub fn oauth(&self) -> oauth::OAuth {
        let insecure = env_string(&self.env, INSECURE_OAUTH_VAR).is_some_and(|v| v == "1");
        oauth::OAuth::new(
            self.coord.clone(),
            self.kv.clone(),
            self.crypto.clone(),
            insecure,
        )
    }

    pub async fn config(&self) -> std::result::Result<Rc<Config>, Response> {
        let now = Date::now().as_millis();
        if let Some(cfg) = CONFIG.with(|c| {
            c.borrow()
                .as_ref()
                .filter(|(exp, _)| *exp > now)
                .map(|(_, cfg)| cfg.clone())
        }) {
            return Ok(cfg);
        }
        // Hot path: the KV copy. The coordinator is the source of truth and
        // is read when the copy is missing (first boot, or a failed copy).
        let mut stored = store::load(&self.kv)
            .await
            .map_err(|e| json_error(500, &e))?;
        if stored.revision == 0 {
            stored = api::authoritative_config(self)
                .await
                .map_err(|e| json_error(500, &e))?;
        }
        let cfg = Rc::new(Config::build(&stored, &self.crypto).map_err(|e| json_error(500, &e))?);
        remember_config(cfg.clone());
        Ok(cfg)
    }
}

/// Constant-time bearer check (compares digests, so length leaks nothing).
fn authorized(req: &Request, env: &Env) -> std::result::Result<(), Response> {
    let Some(expected) = env_string(env, TOKEN_SECRET) else {
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

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (req.method(), segments.as_slice()) {
        (Method::Get, [""]) | (Method::Get, ["index.html"]) => ui::page(ui::Asset::Html),
        (Method::Get, ["app.js"]) => ui::page(ui::Asset::Js),
        (Method::Get, ["app.css"]) => ui::page(ui::Asset::Css),
        (Method::Get, ["healthz"]) => Response::from_json(&json!({ "status": "ok" })),
        (Method::Get, ["oauth-done.js"]) => ui::page(ui::Asset::OAuthDoneJs),
        (Method::Get, ["oauth", "callback"]) => {
            let ctx = match Ctx::new(env) {
                Ok(c) => c,
                Err(r) => return Ok(r),
            };
            oauth_callback(&req, &ctx).await
        }
        (_, ["api", rest @ ..]) => {
            if let Err(resp) = authorized(&req, &env) {
                return Ok(resp);
            }
            let ctx = match Ctx::new(env) {
                Ok(c) => c,
                Err(r) => return Ok(r),
            };
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            api::handle(req, &ctx, &rest).await
        }
        (_, ["mcp"]) | (_, ["mcp", _]) => {
            if let Err(resp) = authorized(&req, &env) {
                return Ok(resp);
            }
            if req.method() != Method::Post {
                let headers = Headers::new();
                headers.set("allow", "POST")?;
                return Ok(json_error(405, "this relay only accepts POST").with_headers(headers));
            }
            let profile = segments.get(1).map(|s| s.to_string());
            let ctx = match Ctx::new(env) {
                Ok(c) => c,
                Err(r) => return Ok(r),
            };
            let config = match ctx.config().await {
                Ok(c) => c,
                Err(r) => return Ok(r),
            };
            let scope = match &profile {
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
            let cache = Cache::new(&ctx.env);
            let oauth = ctx.oauth();
            let relay = Relay {
                config: &config,
                cache: &cache,
                oauth: Some(&oauth),
            };
            handle_mcp(&relay, &scope, &body).await
        }
        _ => Ok(json_error(404, "not found")),
    }
}

/// The authorization server redirects the user's browser here. The `state`
/// value is the only credential: it is random, single-use and short-lived.
async fn oauth_callback(req: &Request, ctx: &Ctx) -> Result<Response> {
    let url = req.url()?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let Some(state) = param("state") else {
        return ui::oauth_result(false, "This page is missing the sign-in state.");
    };
    if let Some(error) = param("error") {
        let detail = param("error_description").unwrap_or_default();
        // Burn the flow so the link cannot be reused.
        let _ = ctx.coord.call("flow/take", json!({ "state": state })).await;
        return ui::oauth_result(
            false,
            &format!("Sign-in was not completed: {error} {detail}"),
        );
    }
    let Some(code) = param("code") else {
        return ui::oauth_result(false, "The authorization server sent no code.");
    };
    match ctx
        .oauth()
        .finish(&state, &code, param("iss").as_deref())
        .await
    {
        Ok(name) => ui::oauth_result(
            true,
            &format!("Signed in to {name}. You can close this window."),
        ),
        Err(e) => ui::oauth_result(false, &e),
    }
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
