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

        let oauth_state = adapter.state.read().await.clone();
        let result = probe_inner(&adapter).await;
        let action = classify_probe_result(result, &mut consecutive_failures, threshold);
        apply_probe_action(&adapter, action, threshold, &oauth_state).await;
    }
}

/// Apply the `ProbeAction` dispatched from `classify_probe_result` to the
/// shared adapter state (writes `inner_health`, increments metrics, may
/// transition `OAuthState`).
///
/// Extracted from `heartbeat_loop` so the side-effect dispatch can be
/// driven directly by tests without spinning a real timer or probe.
async fn apply_probe_action(
    adapter: &OAuthAdapterInner,
    action: ProbeAction,
    threshold: u32,
    oauth_state: &OAuthState,
) {
    let endpoint = &adapter.config.endpoint_name;
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

    // -- End-to-end harness -------------------------------------------------
    //
    // The tests below drive `classify_probe_result` + `apply_probe_action` on
    // a real `OAuthAdapterInner` (constructed via the public `OAuthAdapter`
    // ctor) so we can observe `inner_health` and `state` transitions through
    // the same dispatch path as the production loop. The actual `probe_inner`
    // call is bypassed — tests feed pre-canned `ProbeError`/`Ok` results.

    use crate::adapter::oauth::{OAuthAdapter, OAuthAdapterConfig};
    use crate::adapter::HealthStatus;
    use crate::token_manager::TokenManager;
    use std::sync::Arc;

    fn make_test_config(threshold: u32) -> OAuthAdapterConfig {
        OAuthAdapterConfig {
            endpoint_name: "heartbeat-e2e".to_string(),
            url: "http://localhost/mcp".to_string(),
            token_endpoint_url: "http://localhost/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
            heartbeat_interval_secs: 30,
            probe_timeout_secs: 10,
            probe_failure_threshold: threshold,
        }
    }

    fn make_test_inner(threshold: u32) -> Arc<OAuthAdapterInner> {
        let tmp = tempfile::tempdir().unwrap().keep();
        let tm = Arc::new(TokenManager::new(tmp));
        let adapter = OAuthAdapter::new(make_test_config(threshold), tm);
        adapter.shared_inner()
    }

    /// Drive a single iteration of the heartbeat loop body with a canned
    /// probe result, mirroring `heartbeat_loop` minus the timer and the
    /// real `probe_inner` call.
    async fn step_once(
        adapter: &OAuthAdapterInner,
        result: Result<(), ProbeError>,
        failures: &mut u32,
        threshold: u32,
    ) {
        let oauth_state = adapter.state.read().await.clone();
        let action = classify_probe_result(result, failures, threshold);
        apply_probe_action(adapter, action, threshold, &oauth_state).await;
    }

    /// Set the adapter into the `Authenticated` / `Healthy` baseline that
    /// the heartbeat loop assumes when it actually runs (the real loop
    /// `continue`s out of any other state).
    async fn arm_healthy(inner: &OAuthAdapterInner) {
        *inner.state.write().await = OAuthState::Authenticated;
        *inner.inner_health.write().await = HealthStatus::Healthy;
    }

    #[tokio::test]
    async fn heartbeat_below_threshold_does_not_flip_inner_health() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        // N-1 = 2 consecutive failures; inner_health must remain Healthy.
        for i in 0..(threshold - 1) {
            step_once(&inner, net("transient"), &mut failures, threshold).await;
            assert_eq!(failures, i + 1);
            assert_eq!(
                *inner.inner_health.read().await,
                HealthStatus::Healthy,
                "inner_health flipped after {} failures (threshold={})",
                i + 1,
                threshold
            );
        }
        // Metrics: no healthy/unhealthy increments since BelowThreshold is
        // a no-op on side effects other than logging.
        let snap = inner.metrics.snapshot();
        assert_eq!(snap.oauth_heartbeat_probe_total_healthy, 0);
        assert_eq!(snap.oauth_heartbeat_probe_total_unhealthy, 0);
    }

    #[tokio::test]
    async fn heartbeat_at_threshold_flips_to_unhealthy() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        for _ in 0..threshold {
            step_once(&inner, net("dns"), &mut failures, threshold).await;
        }
        assert_eq!(failures, threshold);
        match &*inner.inner_health.read().await {
            HealthStatus::Unhealthy(reason) => {
                assert_eq!(reason, "upstream unreachable");
            }
            other => panic!("expected Unhealthy, got {:?}", other),
        }
        // Exactly one unhealthy increment (the threshold-crossing probe).
        let snap = inner.metrics.snapshot();
        assert_eq!(snap.oauth_heartbeat_probe_total_unhealthy, 1);
        assert_eq!(snap.oauth_heartbeat_probe_total_healthy, 0);
    }

    #[tokio::test]
    async fn heartbeat_single_ok_after_failures_recovers_to_healthy() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        // Drive to Unhealthy.
        for _ in 0..threshold {
            step_once(&inner, net("dns"), &mut failures, threshold).await;
        }
        assert!(matches!(
            *inner.inner_health.read().await,
            HealthStatus::Unhealthy(_)
        ));

        // A single Ok probe must immediately recover to Healthy
        // (asymmetric hysteresis: slow to fail, fast to recover).
        step_once(&inner, ok(), &mut failures, threshold).await;
        assert_eq!(failures, 0);
        assert_eq!(*inner.inner_health.read().await, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn heartbeat_alternating_ok_fail_does_not_flap() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        // 8 iterations of Fail, Ok, Fail, Ok, ... — counter resets on every
        // Ok before reaching the threshold, so inner_health must never flip
        // to Unhealthy.
        for i in 0..8 {
            let result = if i % 2 == 0 { net("blip") } else { ok() };
            step_once(&inner, result, &mut failures, threshold).await;
            assert_eq!(
                *inner.inner_health.read().await,
                HealthStatus::Healthy,
                "inner_health flapped at step {}",
                i
            );
        }
    }
}
