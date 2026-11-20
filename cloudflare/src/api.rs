//! JSON API for the web app. Every route requires the bearer token.

use crate::cache::Cache;
use crate::config::{fingerprint, validate_url, Config, Endpoint};
use crate::relay::Relay;
use crate::store::{self, ApiConfig, ApiHeader, Crypto, Stored, StoredEndpoint, StoredProfile};
use crate::upstream::Upstream;
use crate::{json_error, Ctx};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::rc::Rc;
use worker::{Method, Request, Response, Result};

const MAX_BODY_BYTES: usize = 512 * 1024;

pub async fn handle(mut req: Request, ctx: &Ctx, rest: &[&str]) -> Result<Response> {
    let origin = req.url()?.origin().ascii_serialization();
    let resp = match (req.method(), rest) {
        (Method::Post, ["oauth", "start"]) => match read_json::<EndpointRef>(&mut req).await {
            Ok(body) => oauth_start(ctx, &body.id, &origin).await,
            Err(r) => Ok(r),
        },
        (Method::Post, ["oauth", "sign-out"]) => match read_json::<EndpointRef>(&mut req).await {
            Ok(body) => match ctx.oauth().sign_out(&body.id).await {
                Ok(()) => Response::from_json(&json!({ "ok": true })),
                Err(e) => Ok(json_error(500, &e)),
            },
            Err(r) => Ok(r),
        },
        (Method::Get, ["config"]) => get_config(ctx).await,
        (Method::Put, ["config"]) => match read_json::<ApiConfig>(&mut req).await {
            Ok(body) => put_config(ctx, body).await,
            Err(r) => Ok(r),
        },
        (Method::Get, ["status"]) => {
            let fresh = req
                .url()
                .map(|u| u.query_pairs().any(|(k, v)| k == "fresh" && v == "1"))
                .unwrap_or(false);
            status(ctx, fresh).await
        }
        (Method::Post, ["test"]) => match read_json::<TestRequest>(&mut req).await {
            Ok(body) => test_connection(ctx, body).await,
            Err(r) => Ok(r),
        },
        _ => Ok(json_error(404, "not found")),
    }?;
    let _ = resp.headers().set("cache-control", "no-store");
    Ok(resp)
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    req: &mut Request,
) -> std::result::Result<T, Response> {
    let body = req
        .bytes()
        .await
        .map_err(|_| json_error(400, "could not read body"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(json_error(413, "request body too large"));
    }
    serde_json::from_slice(&body).map_err(|e| json_error(400, &format!("invalid JSON: {e}")))
}

async fn load_stored(ctx: &Ctx) -> std::result::Result<Stored, Response> {
    store::load(&ctx.kv).await.map_err(|e| json_error(500, &e))
}

async fn get_config(ctx: &Ctx) -> Result<Response> {
    match load_stored(ctx).await {
        Ok(stored) => Response::from_json(&stored.to_api()),
        Err(r) => Ok(r),
    }
}

async fn put_config(ctx: &Ctx, body: ApiConfig) -> Result<Response> {
    let current = match load_stored(ctx).await {
        Ok(s) => s,
        Err(r) => return Ok(r),
    };
    if body.revision != current.revision {
        return Ok(Response::from_json(&json!({
            "error": "the configuration changed in another window; reload to see the latest",
            "revision": current.revision,
        }))?
        .with_status(409));
    }
    let next = match merge(&ctx.crypto, &current, body) {
        Ok(s) => s,
        Err(e) => return Ok(json_error(400, &e)),
    };
    let config = match Config::build(&next, &ctx.crypto) {
        Ok(c) => c,
        Err(e) => return Ok(json_error(400, &e)),
    };
    if let Err(e) = store::save(&ctx.kv, &next).await {
        return Ok(json_error(500, &format!("could not save: {e}")));
    }
    let oauth = ctx.oauth();
    for removed in current
        .endpoints
        .iter()
        .filter(|old| next.endpoint(&old.id).is_none_or(|new| !new.oauth))
    {
        oauth.forget(&removed.id).await;
    }
    crate::remember_config(Rc::new(config));
    Response::from_json(&next.to_api())
}

fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn merge(
    crypto: &Crypto,
    current: &Stored,
    body: ApiConfig,
) -> std::result::Result<Stored, String> {
    let mut endpoints = Vec::with_capacity(body.endpoints.len());
    for ep in body.endpoints {
        let id = if ep.id.trim().is_empty() {
            store::new_id()
        } else {
            ep.id.trim().to_string()
        };
        let headers =
            store::resolve_headers(crypto, current.endpoint(&id), ep.name.trim(), &ep.headers)?;
        let mut disabled_tools: Vec<String> = ep
            .disabled_tools
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        disabled_tools.sort();
        endpoints.push(StoredEndpoint {
            id,
            name: ep.name.trim().to_string(),
            description: clean(&ep.description),
            tool_prefix: clean(&ep.tool_prefix),
            url: ep.url.trim().to_string(),
            headers,
            enabled: ep.enabled,
            disabled_tools,
            oauth: ep.oauth,
        });
    }
    let mut seen_ids = HashSet::new();
    for e in &endpoints {
        if !seen_ids.insert(e.id.as_str()) {
            return Err(format!("two servers share the id '{}'", e.id));
        }
    }
    let known: HashSet<&str> = endpoints.iter().map(|e| e.id.as_str()).collect();
    let profiles = body
        .profiles
        .into_iter()
        .map(|p| StoredProfile {
            id: if p.id.trim().is_empty() {
                store::new_id()
            } else {
                p.id
            },
            name: p.name.trim().to_string(),
            path: p.path.trim().to_string(),
            endpoint_ids: p
                .endpoint_ids
                .into_iter()
                .filter(|id| known.contains(id.as_str()))
                .collect(),
            toon_output: p.toon_output,
        })
        .collect();
    let mut settings = body.settings;
    settings.machine_name = settings.machine_name.trim().to_string();
    Ok(Stored {
        revision: current.revision + 1,
        settings,
        endpoints,
        profiles,
    })
}

async fn status(ctx: &Ctx, fresh: bool) -> Result<Response> {
    let config = match ctx.config().await {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let cache = Cache::new(&ctx.env);
    let oauth = ctx.oauth();
    let relay = Relay {
        config: &config,
        cache: &cache,
        oauth: Some(&oauth),
    };
    Response::from_json(
        &json!({ "revision": config.revision, "servers": relay.status(fresh).await }),
    )
}

#[derive(Deserialize)]
struct TestRequest {
    #[serde(default)]
    id: Option<String>,
    url: String,
    #[serde(default)]
    headers: Vec<ApiHeader>,
}

/// Connect to a draft server (not yet saved) and list its tools.
async fn test_connection(ctx: &Ctx, body: TestRequest) -> Result<Response> {
    let url = body.url.trim().to_string();
    if let Err(e) = validate_url(&url) {
        return Response::from_json(&json!({ "ok": false, "error": e }));
    }
    let stored = match load_stored(ctx).await {
        Ok(s) => s,
        Err(r) => return Ok(r),
    };
    let previous = body.id.as_deref().and_then(|id| stored.endpoint(id));
    let mut headers = Vec::new();
    for h in &body.headers {
        let value = match &h.value {
            Some(v) => v.clone(),
            None => match previous
                .and_then(|p| {
                    p.headers
                        .iter()
                        .find(|s| s.name.eq_ignore_ascii_case(&h.name))
                })
                .map(|s| ctx.crypto.open(s))
            {
                Some(Ok(v)) => v,
                Some(Err(e)) => return Response::from_json(&json!({ "ok": false, "error": e })),
                None => {
                    return Response::from_json(&json!({
                        "ok": false,
                        "error": format!("header '{}' needs a value", h.name),
                    }))
                }
            },
        };
        headers.push((h.name.trim().to_string(), value));
    }
    let endpoint = Endpoint {
        id: String::new(),
        name: "test".into(),
        label: "test".into(),
        prefix: "test".into(),
        fingerprint: format!("test-{}", fingerprint("test", &url, &headers, false)),
        url,
        headers,
        enabled: true,
        oauth: false,
        disabled_tools: HashSet::new(),
    };
    let cache = Cache::memory_only();
    let timeout = stored.settings.request_timeout_secs.clamp(1, 300);
    let upstream = Upstream::new(&endpoint, &cache, timeout, None);
    let reply: Value = match upstream.request("tools/list", json!({})).await {
        Ok(result) => json!({
            "ok": true,
            "tools": result.get("tools").and_then(|t| t.as_array()).map(|tools| {
                tools.iter().map(|t| json!({ "name": t.get("name"), "description": t.get("description") })).collect::<Vec<_>>()
            }).unwrap_or_default(),
            "more": result.get("nextCursor").is_some(),
        }),
        Err(e) => json!({ "ok": false, "error": e.message() }),
    };
    Response::from_json(&reply)
}

#[derive(Deserialize)]
struct EndpointRef {
    id: String,
}

/// Begin an OAuth sign-in for a saved server; the browser opens the URL.
async fn oauth_start(ctx: &Ctx, id: &str, origin: &str) -> Result<Response> {
    let config = match ctx.config().await {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let Some(ep) = config.endpoints.iter().find(|e| e.id == id) else {
        return Ok(json_error(404, "save the server before signing in"));
    };
    if !ep.oauth {
        return Ok(json_error(400, "this server does not use OAuth"));
    }
    let redirect_uri = format!("{origin}/oauth/callback");
    match ctx.oauth().start(ep, &redirect_uri).await {
        Ok(url) => Response::from_json(&json!({ "authorization_url": url })),
        Err(e) => Ok(json_error(502, &e)),
    }
}
