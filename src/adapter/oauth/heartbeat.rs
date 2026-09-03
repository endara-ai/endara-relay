//! Heartbeat probe for the inner (wrapped) adapter.
//!
//! Runs a periodic `tools/list` JSON-RPC request against the upstream MCP
//! server and updates `inner_health` accordingly. Uses hysteresis so a
//! single transient probe failure does not flip the endpoint to Offline
//! in the desktop sidebar.

use super::super::{AdapterError, HealthStatus, McpAdapter};
use super::state::OAuthState;
use super::OAuthAdapterInner;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tracing::{debug, trace, warn};

/// Error type for heartbeat probes.
#[derive(Debug)]
enum ProbeError {
    /// Transport-dead: the upstream did not answer at the transport level
    /// (connect failure, timeout, or a send error with no HTTP status).
    Network(String),
    /// Alive-but-erroring: the upstream answered, but with an error (HTTP
    /// status > 0 other than 401, JSON-RPC error, protocol error).
    Upstream(String),
    /// 401 from the upstream — credentials are no longer accepted.
    Auth,
}

/// Classify an [`AdapterError`] as a transport-dead ("upstream is gone")
/// signal versus an alive-but-erroring one. Mirrors
/// `HttpAdapter::is_transport_dead` (http.rs): dead is only `ConnectionFailed`,
/// `Timeout`, or `HttpError { status: 0 }` — a server answering with a
/// non-zero HTTP status, JSON-RPC error, or protocol error is alive.
fn is_transport_dead(err: &AdapterError) -> bool {
    matches!(
        err,
        AdapterError::ConnectionFailed(_)
            | AdapterError::Timeout(_)
            | AdapterError::HttpError { status: 0, .. }
    )
}

/// Map a probe's `AdapterError` to a `ProbeError`: 401 → `Auth`,
/// transport-dead → `Network`, everything else → `Upstream`.
fn classify_adapter_error(e: AdapterError) -> ProbeError {
    match e {
        AdapterError::HttpError { status: 401, .. } => ProbeError::Auth,
        e if is_transport_dead(&e) => ProbeError::Network(e.to_string()),
        e => ProbeError::Upstream(e.to_string()),
    }
}

/// Action the heartbeat loop should take in response to a probe result.
///
/// Factored out of `heartbeat_loop` so the hysteresis logic can be unit
/// tested without a real upstream adapter.
#[derive(Debug, PartialEq, Eq)]
enum ProbeAction {
    /// Reset failure counter and mark inner_health as Healthy.
    MarkHealthy,
    /// Probe failure but below the threshold — leave inner_health alone,
    /// just log at debug. `failures` is the new (post-increment) count.
    BelowThreshold { failures: u32, reason: String },
    /// Probe failure reached the threshold — mark Unhealthy.
    /// `transport_dead` records whether the threshold-crossing failure was a
    /// transport-dead one (`Network`) or an alive-but-erroring upstream
    /// (`Upstream`); it selects the health message written by
    /// `apply_probe_action`.
    MarkUnhealthy {
        reason: String,
        transport_dead: bool,
    },
    /// Auth failure — reset counter and transition to AuthRequired.
    AuthFailed,
}

/// Apply hysteresis to a probe result.
///
/// Mutates `failures` in place: increments on `Network`/`Upstream`, resets on
/// `Ok` or `Auth`. Returns the action the loop should perform.
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
        Err(ProbeError::Network(reason)) => record_probe_failure(reason, true, failures, threshold),
        Err(ProbeError::Upstream(reason)) => {
            record_probe_failure(reason, false, failures, threshold)
        }
    }
}

/// Shared hysteresis increment for both failure kinds (`Network` /
/// `Upstream`); `transport_dead` is carried through to `MarkUnhealthy`.
fn record_probe_failure(
    reason: String,
    transport_dead: bool,
    failures: &mut u32,
    threshold: u32,
) -> ProbeAction {
    *failures = failures.saturating_add(1);
    if *failures >= threshold {
        ProbeAction::MarkUnhealthy {
            reason,
            transport_dead,
        }
    } else {
        ProbeAction::BelowThreshold {
            failures: *failures,
            reason,
        }
    }
}

/// What the heartbeat loop should do on a given tick, decided purely from
/// the current `OAuthState`.
///
/// Factored out of `heartbeat_loop` so the per-state dispatch is unit
/// testable without a real timer (mirrors `classify_probe_result`).
#[derive(Debug, PartialEq, Eq)]
enum TickAction {
    /// `Authenticated`: run the upstream probe + hysteresis.
    Probe,
    /// `ConnectionFailed`: attempt a recovery token refresh.
    Recover,
    /// `Refreshing` (a refresh is already in flight) or a genuine-auth
    /// terminal state (`AuthRequired`, `NeedsLogin`, `Disconnected`): do
    /// nothing this tick.
    Skip,
}

/// Decide what the heartbeat loop should do based on the current OAuth state.
fn classify_tick_action(state: &OAuthState) -> TickAction {
    match state {
        OAuthState::Authenticated => TickAction::Probe,
        OAuthState::ConnectionFailed => TickAction::Recover,
        OAuthState::Refreshing
        | OAuthState::AuthRequired
        | OAuthState::NeedsLogin
        | OAuthState::Disconnected => TickAction::Skip,
    }
}

/// Attempt to recover from `ConnectionFailed` by driving a token refresh.
///
/// On success, `apply_tokens` re-initializes the inner adapter, transitions
/// back to `Authenticated`, and re-arms the proactive-refresh timer. On
/// failure, `do_token_refresh` has already set the appropriate terminal
/// state (`ConnectionFailed` / `AuthRequired`), so we do nothing extra and
/// let the next heartbeat tick try again. The recovery cadence is gated by
/// the heartbeat interval, so there is no tight retry loop here.
async fn attempt_recovery(adapter: &Arc<OAuthAdapterInner>) {
    let refresh_epoch = adapter.current_grant_epoch();
    match adapter.do_token_refresh_with_epoch(refresh_epoch).await {
        Ok(tokens) => {
            adapter.apply_refreshed_tokens(tokens, refresh_epoch).await;
        }
        Err(e) => {
            debug!(
                error = %e,
                "heartbeat recovery refresh failed; will retry next tick"
            );
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
        Ok(Err(e)) => Err(classify_adapter_error(e)),
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

        // Snapshot state AND generation inside one read-lock critical
        // section: generation bumps only happen inside state write-lock
        // sections, so holding the read lock guarantees the pair is
        // coherent — a bump cannot interleave between the two reads and
        // hand the probe a post-apply generation while it still probes
        // the pre-apply adapter.
        let (oauth_state, generation) = {
            let state = adapter.state.read().await;
            let generation = adapter
                .lifecycle_generation
                .load(std::sync::atomic::Ordering::Relaxed);
            (state.clone(), generation)
        };
        match classify_tick_action(&oauth_state) {
            TickAction::Skip => continue,
            TickAction::Probe => {
                let result = probe_inner(&adapter).await;
                let action = classify_probe_result(result, &mut consecutive_failures, threshold);
                apply_probe_action(&adapter, action, threshold, &oauth_state, generation).await;
            }
            TickAction::Recover => {
                attempt_recovery(&adapter).await;
            }
        }
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
    dispatched_generation: u64,
) {
    match action {
        ProbeAction::MarkHealthy => {
            *adapter.inner_health.write().await = HealthStatus::Healthy;
            adapter.metrics.inc_heartbeat_healthy();
            trace!(
                oauth_state = ?oauth_state,
                result = "healthy",
                "heartbeat probe succeeded"
            );
        }
        ProbeAction::BelowThreshold { failures, reason } => {
            debug!(
                oauth_state = ?oauth_state,
                result = "transient",
                failures = failures,
                threshold = threshold,
                reason = %reason,
                "heartbeat probe failed below threshold; not flipping inner_health"
            );
        }
        ProbeAction::MarkUnhealthy {
            reason,
            transport_dead,
        } => {
            // "upstream unreachable" only when the upstream is genuinely
            // dead at the transport level; an alive-but-erroring upstream
            // (403/5xx/JSON-RPC/protocol) surfaces its actual error text.
            let message = if transport_dead {
                "upstream unreachable".to_string()
            } else {
                reason.clone()
            };
            *adapter.inner_health.write().await = HealthStatus::Unhealthy(message);
            adapter.metrics.inc_heartbeat_unhealthy();
            warn!(
                oauth_state = ?oauth_state,
                result = "unhealthy",
                threshold = threshold,
                transport_dead = transport_dead,
                reason = %reason,
                "heartbeat probe failed at threshold"
            );
        }
        ProbeAction::AuthFailed => {
            // The probe ran without holding the state lock, so an apply /
            // refresh may have taken over mid-probe (state moved off
            // `Authenticated`, e.g. to `Refreshing`). Re-check under the
            // write lock and only apply the stale 401 if the state that
            // dispatched this probe still holds; otherwise the in-flight
            // lifecycle operation's own outcome decides the final state,
            // and stomping it here would resurrect the error banner
            // mid-apply. The state check alone has an ABA hole: an entire
            // apply (Authenticated → Refreshing → Authenticated, publishing
            // a NEW inner adapter) can complete mid-probe, so the enum
            // matches again while the 401 belongs to the replaced adapter.
            // The lifecycle generation catches exactly that: it is bumped
            // inside every state write-lock section and snapshotted with
            // the state under one read lock at dispatch, so "generation
            // unchanged" here proves no transition — hence no apply —
            // interleaved, and the probed adapter is still the published
            // one.
            let mut state = adapter.state.write().await;
            let current_generation = adapter
                .lifecycle_generation
                .load(std::sync::atomic::Ordering::Relaxed);
            if *state != OAuthState::Authenticated || current_generation != dispatched_generation {
                debug!(
                    oauth_state = ?*state,
                    dispatched_generation = dispatched_generation,
                    current_generation = current_generation,
                    result = "stale",
                    "heartbeat probe got 401 but the lifecycle moved on \
                     mid-probe (state changed or an apply ran); dropping \
                     stale result"
                );
                return;
            }
            adapter.metrics.inc_heartbeat_unhealthy();
            warn!(
                oauth_state = ?oauth_state,
                result = "unhealthy",
                "heartbeat probe got 401, transitioning to AuthRequired"
            );
            // Inside the state write critical section so the baseline
            // clear is atomic with the degraded transition, matching the
            // other state writers (`transition_to` /
            // `transition_if_current`): while AuthRequired the registry
            // may rebuild the merged catalog without this endpoint's
            // tools, so the recovery apply must tick even when the
            // re-probed tool set is unchanged.
            adapter
                .clear_fingerprint_on_degraded(&OAuthState::AuthRequired)
                .await;
            *state = OAuthState::AuthRequired;
            // Keep the invariant "every state write bumps the generation
            // inside the write critical section" (see
            // `lifecycle_generation`).
            adapter
                .lifecycle_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(reason: &str) -> Result<(), ProbeError> {
        Err(ProbeError::Network(reason.to_string()))
    }

    fn upstream(reason: &str) -> Result<(), ProbeError> {
        Err(ProbeError::Upstream(reason.to_string()))
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
            ProbeAction::MarkUnhealthy {
                reason,
                transport_dead,
            } => {
                assert_eq!(reason, "dns");
                assert!(transport_dead, "Network failures are transport-dead");
            }
            other => panic!("expected MarkUnhealthy, got {:?}", other),
        }
    }

    #[test]
    fn three_consecutive_upstream_failures_flip_to_unhealthy_not_transport_dead() {
        let mut failures: u32 = 0;
        let threshold = 3;

        let _ = classify_probe_result(
            upstream("HTTP error 403: forbidden"),
            &mut failures,
            threshold,
        );
        let _ = classify_probe_result(
            upstream("HTTP error 403: forbidden"),
            &mut failures,
            threshold,
        );
        let a3 = classify_probe_result(
            upstream("HTTP error 403: forbidden"),
            &mut failures,
            threshold,
        );

        assert_eq!(failures, 3);
        match a3 {
            ProbeAction::MarkUnhealthy {
                reason,
                transport_dead,
            } => {
                assert_eq!(reason, "HTTP error 403: forbidden");
                assert!(!transport_dead, "Upstream failures are alive-but-erroring");
            }
            other => panic!("expected MarkUnhealthy, got {:?}", other),
        }
    }

    #[test]
    fn classify_adapter_error_maps_transport_dead_vs_upstream() {
        // 401 → Auth.
        assert!(matches!(
            classify_adapter_error(AdapterError::HttpError {
                status: 401,
                body: "unauthorized".into(),
            }),
            ProbeError::Auth
        ));

        // Transport-dead (mirrors HttpAdapter::is_transport_dead) → Network.
        assert!(matches!(
            classify_adapter_error(AdapterError::ConnectionFailed("refused".into())),
            ProbeError::Network(_)
        ));
        assert!(matches!(
            classify_adapter_error(AdapterError::Timeout(30)),
            ProbeError::Network(_)
        ));
        assert!(matches!(
            classify_adapter_error(AdapterError::HttpError {
                status: 0,
                body: "send error".into(),
            }),
            ProbeError::Network(_)
        ));

        // Alive-but-erroring → Upstream, carrying the real error text.
        match classify_adapter_error(AdapterError::HttpError {
            status: 403,
            body: "forbidden".into(),
        }) {
            ProbeError::Upstream(reason) => assert_eq!(reason, "HTTP error 403: forbidden"),
            other => panic!("expected Upstream, got {:?}", other),
        }
        assert!(matches!(
            classify_adapter_error(AdapterError::HttpError {
                status: 500,
                body: "boom".into(),
            }),
            ProbeError::Upstream(_)
        ));
        assert!(matches!(
            classify_adapter_error(AdapterError::JsonRpcError {
                code: -32000,
                message: "nope".into(),
                data: None,
            }),
            ProbeError::Upstream(_)
        ));
        assert!(matches!(
            classify_adapter_error(AdapterError::ProtocolError("bad".into())),
            ProbeError::Upstream(_)
        ));
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
            ProbeAction::MarkUnhealthy { reason, .. } => assert_eq!(reason, "boom"),
            other => panic!("expected MarkUnhealthy, got {:?}", other),
        }
    }

    #[test]
    fn classify_tick_action_maps_each_state() {
        // Authenticated → probe (existing hysteresis path).
        assert_eq!(
            classify_tick_action(&OAuthState::Authenticated),
            TickAction::Probe
        );
        // ConnectionFailed → recovery refresh.
        assert_eq!(
            classify_tick_action(&OAuthState::ConnectionFailed),
            TickAction::Recover
        );
        // Refresh in flight and genuine-auth terminal states are skipped:
        // recovery must NOT be attempted for any of these.
        for state in [
            OAuthState::Refreshing,
            OAuthState::AuthRequired,
            OAuthState::NeedsLogin,
            OAuthState::Disconnected,
        ] {
            assert_eq!(
                classify_tick_action(&state),
                TickAction::Skip,
                "state {:?} must be skipped (no recovery)",
                state
            );
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
    use crate::token_manager::{TokenManager, TokenSet};
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
            server_type_override: None,
            allow_insecure_oauth: false,
            ema: None,
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
        let generation = adapter
            .lifecycle_generation
            .load(std::sync::atomic::Ordering::Relaxed);
        let action = classify_probe_result(result, failures, threshold);
        apply_probe_action(adapter, action, threshold, &oauth_state, generation).await;
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

    /// Regression: an alive-but-erroring upstream (e.g. Gmail MCP answering
    /// 403) must surface its actual error text at the threshold, not the
    /// transport-dead "upstream unreachable".
    #[tokio::test]
    async fn heartbeat_upstream_errors_surface_real_reason_not_unreachable() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        for _ in 0..threshold {
            let err = AdapterError::HttpError {
                status: 403,
                body: "forbidden".into(),
            };
            step_once(
                &inner,
                Err(classify_adapter_error(err)),
                &mut failures,
                threshold,
            )
            .await;
        }
        assert_eq!(failures, threshold);
        match &*inner.inner_health.read().await {
            HealthStatus::Unhealthy(reason) => {
                assert!(
                    reason.contains("HTTP error 403"),
                    "message must carry the 403 error text, got {:?}",
                    reason
                );
                assert_ne!(reason, "upstream unreachable");
            }
            other => panic!("expected Unhealthy, got {:?}", other),
        };
    }

    /// The 3-tick hysteresis applies to alive-but-erroring failures too: two
    /// upstream errors must not flip inner_health.
    #[tokio::test]
    async fn heartbeat_upstream_errors_below_threshold_do_not_flip() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        let mut failures = 0u32;
        for _ in 0..(threshold - 1) {
            step_once(
                &inner,
                upstream("HTTP error 500: boom"),
                &mut failures,
                threshold,
            )
            .await;
            assert_eq!(*inner.inner_health.read().await, HealthStatus::Healthy);
        }
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

    /// Regression: a probe is dispatched while `Authenticated`, but an
    /// apply/refresh moves the state to `Refreshing` before the probe's 401
    /// result is applied. The stale `AuthFailed` must be DROPPED — not
    /// stomp the in-flight apply with `AuthRequired` (which would resurrect
    /// the error banner mid-apply).
    #[tokio::test]
    async fn heartbeat_stale_auth_failed_mid_apply_is_dropped() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        // Snapshot what the loop read when it dispatched the probe.
        let dispatched_state = inner.state.read().await.clone();
        let dispatched_gen = inner
            .lifecycle_generation
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dispatched_state, OAuthState::Authenticated);

        // An apply takes over mid-probe.
        *inner.state.write().await = OAuthState::Refreshing;

        // The probe's 401 result lands afterwards: it must be dropped.
        let mut failures = 0u32;
        let action = classify_probe_result(auth(), &mut failures, threshold);
        assert_eq!(action, ProbeAction::AuthFailed);
        apply_probe_action(&inner, action, threshold, &dispatched_state, dispatched_gen).await;

        assert_eq!(
            *inner.state.read().await,
            OAuthState::Refreshing,
            "stale probe 401 must not overwrite an in-flight apply"
        );
        // No unhealthy metric increment for a dropped stale result.
        let snap = inner.metrics.snapshot();
        assert_eq!(snap.oauth_heartbeat_probe_total_unhealthy, 0);

        // Control: with the state still Authenticated and the generation
        // unchanged, the 401 applies.
        *inner.state.write().await = OAuthState::Authenticated;
        let action = classify_probe_result(auth(), &mut failures, threshold);
        apply_probe_action(&inner, action, threshold, &dispatched_state, dispatched_gen).await;
        assert_eq!(*inner.state.read().await, OAuthState::AuthRequired);
    }

    /// Regression (ABA): an ENTIRE apply completes mid-probe — the state
    /// goes Authenticated → Refreshing → Authenticated and a NEW inner
    /// adapter is published. The state enum matches `Authenticated` again
    /// when the old probe's 401 lands, but the result belongs to the
    /// replaced adapter: the lifecycle generation (bumped inside every
    /// state write-lock section) must cause the stale 401 to be dropped.
    #[tokio::test]
    async fn heartbeat_stale_auth_failed_after_full_apply_aba_is_dropped() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;

        // Probe dispatched: snapshot state + generation (the loop reads
        // both under one state read lock).
        let dispatched_state = inner.state.read().await.clone();
        let dispatched_gen = inner
            .lifecycle_generation
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dispatched_state, OAuthState::Authenticated);

        // A full apply completes mid-probe via the real transition path,
        // which bumps the generation inside each state write: the state
        // returns to Authenticated (mirrors apply_tokens_inner's sequence).
        inner
            .transition_to(OAuthState::Refreshing, "test: apply takes over")
            .await;
        inner
            .transition_to(OAuthState::Authenticated, "test: apply finished")
            .await;

        // The old probe's 401 lands: state matches but generation differs —
        // the stale result must be dropped.
        let mut failures = 0u32;
        let action = classify_probe_result(auth(), &mut failures, threshold);
        assert_eq!(action, ProbeAction::AuthFailed);
        apply_probe_action(&inner, action, threshold, &dispatched_state, dispatched_gen).await;

        assert_eq!(
            *inner.state.read().await,
            OAuthState::Authenticated,
            "stale probe 401 must not stomp the freshly applied adapter (ABA)"
        );
        let snap = inner.metrics.snapshot();
        assert_eq!(snap.oauth_heartbeat_probe_total_unhealthy, 0);
    }

    /// Pins the invariant the stale-probe drop check relies on: every state
    /// write bumps `lifecycle_generation` inside the same write-lock
    /// critical section — both `transition_to` and the heartbeat's own
    /// AuthFailed arm — so "generation unchanged" proves no transition
    /// (hence no apply/publish) interleaved with a probe.
    #[tokio::test]
    async fn every_state_write_bumps_lifecycle_generation() {
        let threshold = 3;
        let inner = make_test_inner(threshold);
        arm_healthy(&inner).await;
        let load = |inner: &OAuthAdapterInner| {
            inner
                .lifecycle_generation
                .load(std::sync::atomic::Ordering::Relaxed)
        };

        let g0 = load(&inner);
        inner
            .transition_to(OAuthState::Refreshing, "test: bump check")
            .await;
        assert_eq!(load(&inner), g0 + 1, "transition_to must bump");
        inner
            .transition_to(OAuthState::Authenticated, "test: bump check")
            .await;
        assert_eq!(load(&inner), g0 + 2, "transition_to must bump");

        // The heartbeat's AuthFailed arm writes AuthRequired directly under
        // the write lock; it must bump too.
        let dispatched_state = inner.state.read().await.clone();
        let dispatched_gen = load(&inner);
        let mut failures = 0u32;
        let action = classify_probe_result(auth(), &mut failures, threshold);
        assert_eq!(action, ProbeAction::AuthFailed);
        apply_probe_action(&inner, action, threshold, &dispatched_state, dispatched_gen).await;
        assert_eq!(*inner.state.read().await, OAuthState::AuthRequired);
        assert_eq!(load(&inner), g0 + 3, "AuthFailed arm must bump");
    }

    // -- Recovery-from-ConnectionFailed harness -----------------------------
    //
    // These drive `attempt_recovery` (the heartbeat's `TickAction::Recover`
    // branch) on a real `OAuthAdapterInner` so we observe the live
    // `do_token_refresh` + `apply_tokens` state transitions, again without a
    // real timer.

    /// Spawn a token endpoint that always returns a valid refreshed token set.
    async fn spawn_token_server_success() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handler() -> Json<Value> {
            Json(json!({
                "access_token": "recovered-access-token",
                "token_type": "Bearer",
                "expires_in": 3600u64,
                "refresh_token": "recovered-refresh-token",
            }))
        }

        let router = Router::new().route("/token", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/token", addr.port()), handle)
    }

    /// Spawn a minimal MCP server so `apply_tokens` can re-init the inner
    /// adapter and reach `Authenticated`. Serves a FIXED tool set so the
    /// fingerprint probe stores a stable `Some` baseline.
    async fn spawn_minimal_mcp_server() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handle(Json(body): Json<Value>) -> Json<Value> {
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "initialize" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "test-server", "version": "0.0.1"},
                    },
                })),
                "tools/list" => Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "alpha",
                            "description": "fixed tool",
                            "inputSchema": {"type": "object", "properties": {}},
                        }],
                    },
                })),
                _ => Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            }
        }

        let router = Router::new().route("/mcp", post(handle));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}/mcp", addr.port()), handle)
    }

    /// Build an inner pinned in `ConnectionFailed` with a refresh token, the
    /// state the heartbeat recovery branch starts from.
    async fn make_recovery_inner(mcp_url: String, token_url: String) -> Arc<OAuthAdapterInner> {
        let tmp = tempfile::tempdir().unwrap().keep();
        let tm = Arc::new(TokenManager::new(tmp));
        let mut config = make_test_config(3);
        config.url = mcp_url;
        config.token_endpoint_url = token_url;
        let adapter = OAuthAdapter::new(config, tm);
        let inner = adapter.shared_inner();
        *inner.tokens.write().await = Some(TokenSet {
            access_token: "stale-access".to_string(),
            refresh_token: Some("stale-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        });
        *inner.state.write().await = OAuthState::ConnectionFailed;
        inner
    }

    #[tokio::test]
    async fn recovery_refresh_success_returns_to_authenticated() {
        let (mcp_url, mcp_srv) = spawn_minimal_mcp_server().await;
        let (token_url, token_srv) = spawn_token_server_success().await;
        let inner = make_recovery_inner(mcp_url, token_url).await;

        assert_eq!(*inner.state.read().await, OAuthState::ConnectionFailed);
        attempt_recovery(&inner).await;
        assert_eq!(
            *inner.state.read().await,
            OAuthState::Authenticated,
            "successful recovery refresh must restore Authenticated via apply_tokens"
        );

        mcp_srv.abort();
        token_srv.abort();
    }

    #[tokio::test]
    async fn recovery_refresh_failure_stays_connection_failed() {
        // Unreachable token endpoint (reserved port 1) → network error, so
        // do_token_refresh transitions back to ConnectionFailed.
        let inner = make_recovery_inner(
            "http://127.0.0.1:19997/mcp".to_string(),
            "http://127.0.0.1:1/token".to_string(),
        )
        .await;

        attempt_recovery(&inner).await;
        assert_eq!(
            *inner.state.read().await,
            OAuthState::ConnectionFailed,
            "failed recovery refresh must remain ConnectionFailed for the next tick"
        );

        // A subsequent tick must not panic or wedge the loop.
        attempt_recovery(&inner).await;
        assert_eq!(
            *inner.state.read().await,
            OAuthState::ConnectionFailed,
            "recovery must keep retrying without wedging"
        );
    }

    // -- Heartbeat-401 degradation → recovery-tick regression ---------------

    fn make_tokens(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        }
    }

    /// Wait briefly for an outer tick. Returns whether one arrived.
    async fn recv_outer_tick(
        rx: &mut tokio::sync::broadcast::Receiver<()>,
        timeout: Duration,
    ) -> bool {
        use tokio::sync::broadcast::error::RecvError;
        matches!(
            tokio::time::timeout(timeout, rx.recv()).await,
            Ok(Ok(())) | Ok(Err(RecvError::Lagged(_)))
        )
    }

    /// Drain any already-queued ticks so subsequent `recv` reflects new sends.
    async fn drain_ticks(rx: &mut tokio::sync::broadcast::Receiver<()>) {
        while recv_outer_tick(rx, Duration::from_millis(20)).await {}
    }

    /// Regression (PR #151 review): the heartbeat's AuthFailed arm writes
    /// `AuthRequired` directly under the state write lock (bypassing
    /// `transition_to`), so it must ALSO clear the tools-fingerprint
    /// baseline — without the clear, a heartbeat-401 degradation followed
    /// by a re-login reproducing the SAME tool set suppresses the recovery
    /// tick (A==A comparison) and leaves the endpoint's tools missing from
    /// the merged catalog until an unrelated invalidation.
    #[tokio::test]
    async fn heartbeat_401_degradation_clears_fingerprint_so_recovery_apply_ticks() {
        use crate::adapter::McpAdapter as _;

        let threshold = 3;
        let (mcp_url, mcp_srv) = spawn_minimal_mcp_server().await;
        let tmp = tempfile::tempdir().unwrap().keep();
        let tm = Arc::new(TokenManager::new(tmp));
        let mut config = make_test_config(threshold);
        config.url = mcp_url;
        let mut adapter = OAuthAdapter::new(config, tm);
        adapter.initialize().await.unwrap();
        let inner = adapter.shared_inner();
        let mut outer_rx = adapter.subscribe_tools_changed().expect("outer rx");

        // Baseline: apply with the server's fixed tool set → Authenticated
        // with a stored fingerprint.
        inner.apply_tokens(make_tokens("first")).await;
        assert_eq!(*inner.state.read().await, OAuthState::Authenticated);
        assert!(
            inner.last_tools_fingerprint.read().await.is_some(),
            "baseline fingerprint must be stored after a successful probe"
        );
        drain_ticks(&mut outer_rx).await;

        // A heartbeat probe 401 lands through the real dispatch path,
        // degrading to AuthRequired via the AuthFailed arm.
        let mut failures = 0u32;
        step_once(&inner, auth(), &mut failures, threshold).await;
        assert_eq!(*inner.state.read().await, OAuthState::AuthRequired);
        assert!(
            inner.last_tools_fingerprint.read().await.is_none(),
            "heartbeat 401 → AuthRequired must clear the fingerprint baseline"
        );

        // Recovery re-login reproducing the SAME tool set: the baseline is
        // unknown, so the Some→Some swap must tick so the registry restores
        // the endpoint's tools to the merged catalog.
        inner.apply_tokens(make_tokens("second")).await;
        assert!(
            recv_outer_tick(&mut outer_rx, Duration::from_millis(500)).await,
            "recovery apply after a heartbeat-401 degradation must tick even \
             when the probed tool set is unchanged"
        );

        mcp_srv.abort();
    }
}
