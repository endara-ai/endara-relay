//! Heartbeat probe for the inner (wrapped) adapter.
//!
//! Runs a periodic `tools/list` JSON-RPC request against the upstream MCP
//! server and updates `inner_health` accordingly.

use super::super::{AdapterError, HealthStatus, McpAdapter};
use super::state::OAuthState;
use super::OAuthAdapterInner;
use std::sync::Weak;
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

/// Error type for heartbeat probes.
#[derive(Debug)]
enum ProbeError {
    Network(String),
    Auth,
}

/// Probe the inner adapter by sending a `tools/list` JSON-RPC request
/// with a 5-second timeout.
async fn probe_inner(inner: &OAuthAdapterInner) -> Result<(), ProbeError> {
    let guard = inner.inner_adapter.read().await;
    let adapter = match guard.as_ref() {
        Some(a) => a,
        None => return Err(ProbeError::Network("no inner adapter".into())),
    };

    match tokio::time::timeout(Duration::from_secs(5), adapter.list_tools()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(AdapterError::HttpError { status: 401, .. })) => Err(ProbeError::Auth),
        Ok(Err(e)) => Err(ProbeError::Network(e.to_string())),
        Err(_) => Err(ProbeError::Network("probe timed out after 5s".into())),
    }
}

/// Background heartbeat loop that periodically probes the inner adapter.
///
/// Uses a `Weak` reference so the loop exits automatically when the
/// adapter is dropped.
pub async fn heartbeat_loop(inner: Weak<OAuthAdapterInner>) {
    let interval_secs = match inner.upgrade() {
        Some(arc) => arc.config.heartbeat_interval_secs,
        None => return,
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let Some(adapter) = inner.upgrade() else {
            return;
        };

        if !matches!(*adapter.state.read().await, OAuthState::Authenticated) {
            continue;
        }

        let endpoint = &adapter.config.endpoint_name;
        match probe_inner(&adapter).await {
            Ok(()) => {
                *adapter.inner_health.write().await = HealthStatus::Healthy;
                debug!(endpoint = %endpoint, "heartbeat probe succeeded");
            }
            Err(ProbeError::Network(reason)) => {
                *adapter.inner_health.write().await =
                    HealthStatus::Unhealthy("upstream unreachable".into());
                warn!(endpoint = %endpoint, reason = %reason, "heartbeat probe failed");
            }
            Err(ProbeError::Auth) => {
                warn!(endpoint = %endpoint, "heartbeat probe got 401, transitioning to AuthRequired");
                *adapter.state.write().await = OAuthState::AuthRequired;
            }
        }
    }
}
