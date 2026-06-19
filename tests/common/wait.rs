//! Poll-until-ready helpers for integration tests.
//!
//! Prefer these over fixed `tokio::time::sleep` calls when waiting for a
//! positive condition (a server to start, a catalog to populate, an API to
//! report the expected state). They return as soon as the condition holds and
//! fail fast on timeout instead of hanging.

#![allow(dead_code)]

use std::future::Future;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Interval between predicate checks.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Poll `predicate` roughly every 50ms until it returns `true` or `deadline`
/// elapses. The predicate returns a future, so both async checks
/// (`|| async { thing().await }`) and sync checks (`|| async { thing() }`)
/// work. Returns `true` if the condition was met, `false` on timeout.
pub async fn poll_until<F, Fut>(deadline: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let stop = Instant::now() + deadline;
    loop {
        if predicate().await {
            return true;
        }
        if Instant::now() >= stop {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Wait until the host/port behind `url` accepts a TCP connection or `deadline`
/// elapses. A TCP connect is used (rather than an HTTP GET) so it works
/// uniformly for streaming endpoints (e.g. SSE) that would otherwise hold a GET
/// open. Returns `true` once the server is reachable, `false` on timeout.
pub async fn wait_http_ready(url: &str, deadline: Duration) -> bool {
    let parsed = url::Url::parse(url).expect("wait_http_ready: invalid URL");
    let host = parsed
        .host_str()
        .expect("wait_http_ready: URL has no host")
        .to_string();
    let port = parsed
        .port_or_known_default()
        .expect("wait_http_ready: URL has no port");
    poll_until(deadline, || {
        let host = host.clone();
        async move { TcpStream::connect((host.as_str(), port)).await.is_ok() }
    })
    .await
}
