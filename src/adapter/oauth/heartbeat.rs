//! Heartbeat probe for the inner (wrapped) adapter.
//!
//! Runs a periodic `tools/list` JSON-RPC request against the upstream MCP
//! server and updates `inner_health` accordingly. Uses hysteresis so a
//! single transient probe failure does not flip the endpoint to Offline
//! in the desktop sidebar.

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

/// Action the heartbeat loop should take in response to a probe result.
///
/// Factored out of `heartbeat_loop` so the hysteresis logic can be unit
/// tested without a real upstream adapter.
#[derive(Debug, PartialEq, Eq)]
enum ProbeAction {
    /// Reset failure counter and mark inner_health as Healthy.
    MarkHealthy,
    /// Network failure but below the threshold — leave inner_health alone,
    /// just log at debug. `failures` is the new (post-increment) count.
    BelowThreshold { failures: u32, reason: String },
    /// Network failure reached the threshold — mark Unhealthy.
    MarkUnhealthy { reason: String },
    /// Auth failure — reset counter and transition to AuthRequired.
    AuthFailed,
}

/// Apply hysteresis to a probe result.
///
/// Mutates `failures` in place: increments on `Network`, resets on `Ok` or
/// `Auth`. Returns the action the loop should perform.
fn classify_probe_result(
    result: Result<(), ProbeError>,
    failures: &mut u32,
    threshold: u32,
) -> ProbeAction {
    match result {
        Ok(()) => {
            *failures = 0;
            ProbeAction::MarkHealthy
        }
        Err(ProbeError::Auth) => {
            *failures = 0;
            ProbeAction::AuthFailed
        }
        Err(ProbeError::Network(reason)) => {
            *failures = failures.saturating_add(1);
            if *failures >= threshold {
                ProbeAction::MarkUnhealthy { reason }
            } else {
                ProbeAction::BelowThreshold {
                    failures: *failures,
                    reason,
                }
            }
        }
    }
}

/// Probe the inner adapter by sending a `tools/list` JSON-RPC request
/// with a configurable timeout.
async fn probe_inner(inner: &OAuthAdapterInner) -> Result<(), ProbeError> {
    let guard = inner.inner_adapter.read().await;
    let adapter = match guard.as_ref() {
        Some(a) => a,
        None => return Err(ProbeError::Network("no inner adapter".into())),
    };

    let timeout_secs = inner.config.probe_timeout_secs;
    match tokio::time::timeout(Duration::from_secs(timeout_secs), adapter.list_tools()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(AdapterError::HttpError { status: 401, .. })) => Err(ProbeError::Auth),
        Ok(Err(e)) => Err(ProbeError::Network(e.to_string())),
        Err(_) => Err(ProbeError::Network(format!(
            "probe timed out after {}s",
            timeout_secs
        ))),
    }
}

/// Background heartbeat loop that periodically probes the inner adapter.
///
/// Uses a `Weak` reference so the loop exits automatically when the
/// adapter is dropped.
pub async fn heartbeat_loop(inner: Weak<OAuthAdapterInner>) {
    let (interval_secs, threshold) = match inner.upgrade() {
        Some(arc) => (
            arc.config.heartbeat_interval_secs,
            arc.config.probe_failure_threshold,
        ),
        None => return,
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut consecutive_failures: u32 = 0;

    loop {
        ticker.tick().await;
        let Some(adapter) = inner.upgrade() else {
            return;
        };

        if !matches!(*adapter.state.read().await, OAuthState::Authenticated) {
            continue;
        }

        let endpoint = &adapter.config.endpoint_name;
        let oauth_state = adapter.state.read().await.clone();
        let result = probe_inner(&adapter).await;
        let action = classify_probe_result(result, &mut consecutive_failures, threshold);

        match action {
            ProbeAction::MarkHealthy => {
                *adapter.inner_health.write().await = HealthStatus::Healthy;
                adapter.metrics.inc_heartbeat_healthy();
                debug!(
                    endpoint = %endpoint,
                    oauth_state = ?oauth_state,
                    result = "healthy",
                    "heartbeat probe succeeded"
                );
            }
            ProbeAction::BelowThreshold { failures, reason } => {
                debug!(
                    endpoint = %endpoint,
                    oauth_state = ?oauth_state,
                    result = "transient",
                    failures = failures,
                    threshold = threshold,
                    reason = %reason,
                    "heartbeat probe failed below threshold; not flipping inner_health"
                );
            }
            ProbeAction::MarkUnhealthy { reason } => {
                *adapter.inner_health.write().await =
                    HealthStatus::Unhealthy("upstream unreachable".into());
                adapter.metrics.inc_heartbeat_unhealthy();
                warn!(
                    endpoint = %endpoint,
                    oauth_state = ?oauth_state,
                    result = "unhealthy",
                    threshold = threshold,
                    reason = %reason,
                    "heartbeat probe failed at threshold"
                );
            }
            ProbeAction::AuthFailed => {
                adapter.metrics.inc_heartbeat_unhealthy();
                warn!(
                    endpoint = %endpoint,
                    oauth_state = ?oauth_state,
                    result = "unhealthy",
                    "heartbeat probe got 401, transitioning to AuthRequired"
                );
                *adapter.state.write().await = OAuthState::AuthRequired;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(reason: &str) -> Result<(), ProbeError> {
        Err(ProbeError::Network(reason.to_string()))
    }

    fn ok() -> Result<(), ProbeError> {
        Ok(())
    }

    fn auth() -> Result<(), ProbeError> {
        Err(ProbeError::Auth)
    }

    #[test]
    fn two_consecutive_network_failures_stay_below_threshold() {
        let mut failures: u32 = 0;
        let threshold = 3;

        let a1 = classify_probe_result(net("dns"), &mut failures, threshold);
        assert_eq!(failures, 1);
        assert!(
            matches!(a1, ProbeAction::BelowThreshold { failures: 1, .. }),
            "expected BelowThreshold(1), got {:?}",
            a1
        );

        let a2 = classify_probe_result(net("timeout"), &mut failures, threshold);
        assert_eq!(failures, 2);
        assert!(
            matches!(a2, ProbeAction::BelowThreshold { failures: 2, .. }),
            "expected BelowThreshold(2), got {:?}",
            a2
        );
    }

    #[test]
    fn three_consecutive_network_failures_flip_to_unhealthy() {
        let mut failures: u32 = 0;
        let threshold = 3;

        let _ = classify_probe_result(net("dns"), &mut failures, threshold);
        let _ = classify_probe_result(net("dns"), &mut failures, threshold);
        let a3 = classify_probe_result(net("dns"), &mut failures, threshold);

        assert_eq!(failures, 3);
        match a3 {
            ProbeAction::MarkUnhealthy { reason } => assert_eq!(reason, "dns"),
            other => panic!("expected MarkUnhealthy, got {:?}", other),
        }
    }

    #[test]
    fn success_in_between_resets_counter() {
        let mut failures: u32 = 0;
        let threshold = 3;

        let _ = classify_probe_result(net("e1"), &mut failures, threshold);
        let _ = classify_probe_result(net("e2"), &mut failures, threshold);
        assert_eq!(failures, 2);

        let mid = classify_probe_result(ok(), &mut failures, threshold);
        assert_eq!(failures, 0);
        assert_eq!(mid, ProbeAction::MarkHealthy);

        // After the reset, a single failure must NOT flip to Unhealthy.
        let after = classify_probe_result(net("e3"), &mut failures, threshold);
        assert_eq!(failures, 1);
        assert!(
            matches!(after, ProbeAction::BelowThreshold { failures: 1, .. }),
            "expected BelowThreshold(1) after reset, got {:?}",
            after
        );
    }

    #[test]
    fn auth_failure_resets_counter_and_signals_auth() {
        let mut failures: u32 = 0;
        let threshold = 3;

        let _ = classify_probe_result(net("e1"), &mut failures, threshold);
        let _ = classify_probe_result(net("e2"), &mut failures, threshold);
        assert_eq!(failures, 2);

        let action = classify_probe_result(auth(), &mut failures, threshold);
        assert_eq!(failures, 0);
        assert_eq!(action, ProbeAction::AuthFailed);
    }

    #[test]
    fn threshold_of_one_flips_immediately() {
        // Backwards-compatible knob: threshold=1 reproduces the old behavior.
        let mut failures: u32 = 0;
        let action = classify_probe_result(net("boom"), &mut failures, 1);
        assert_eq!(failures, 1);
        match action {
            ProbeAction::MarkUnhealthy { reason } => assert_eq!(reason, "boom"),
            other => panic!("expected MarkUnhealthy, got {:?}", other),
        }
    }
}
