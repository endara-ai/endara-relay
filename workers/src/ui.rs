//! The web app's static files, compiled into the Worker.

use worker::{Headers, Response, Result};

const HTML: &str = include_str!("../ui/index.html");
const JS: &str = include_str!("../ui/app.js");
const CSS: &str = include_str!("../ui/app.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub enum Asset {
    Html,
    Js,
    Css,
}

pub fn page(asset: Asset) -> Result<Response> {
    let (body, content_type) = match asset {
        Asset::Html => (HTML, "text/html; charset=utf-8"),
        Asset::Js => (JS, "text/javascript; charset=utf-8"),
        Asset::Css => (CSS, "text/css; charset=utf-8"),
    };
    let headers = Headers::new();
    headers.set("content-type", content_type)?;
    headers.set("cache-control", "no-cache")?;
    headers.set("content-security-policy", CSP)?;
    headers.set("x-content-type-options", "nosniff")?;
    headers.set("referrer-policy", "no-referrer")?;
    Ok(Response::ok(body)?.with_headers(headers))
}
