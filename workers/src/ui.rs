//! The web app's static files, compiled into the Worker.

use worker::{Headers, Response, Result};

const HTML: &str = include_str!("../ui/index.html");
const JS: &str = include_str!("../ui/app.js");
const CSS: &str = include_str!("../ui/app.css");
const OAUTH_DONE_JS: &str = include_str!("../ui/oauth-done.js");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub enum Asset {
    Html,
    Js,
    Css,
    OAuthDoneJs,
}

pub fn page(asset: Asset) -> Result<Response> {
    let (body, content_type) = match asset {
        Asset::Html => (HTML, "text/html; charset=utf-8"),
        Asset::Js => (JS, "text/javascript; charset=utf-8"),
        Asset::Css => (CSS, "text/css; charset=utf-8"),
        Asset::OAuthDoneJs => (OAUTH_DONE_JS, "text/javascript; charset=utf-8"),
    };
    respond(body.to_string(), content_type)
}

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Result page for `/oauth/callback`. It tells the open web app to refresh
/// and closes itself when it was opened as a popup.
pub fn oauth_result(ok: bool, message: &str) -> Result<Response> {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"color-scheme\" content=\"light dark\"><title>Endara Relay</title>\
<link rel=\"stylesheet\" href=\"/app.css\"><script src=\"/oauth-done.js\" defer></script></head>\
<body data-result=\"{}\"><div class=\"login\"><div class=\"card\"><h1>Endara Relay</h1>\
<p class=\"note {}\">{}</p></div></div></body></html>",
        if ok { "ok" } else { "error" },
        if ok { "ok" } else { "error" },
        escape(message)
    );
    respond(html, "text/html; charset=utf-8")
}

fn respond(body: String, content_type: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", content_type)?;
    headers.set("cache-control", "no-cache")?;
    headers.set("content-security-policy", CSP)?;
    headers.set("x-content-type-options", "nosniff")?;
    headers.set("referrer-policy", "no-referrer")?;
    Ok(Response::ok(body)?.with_headers(headers))
}
