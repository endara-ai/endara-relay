use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

use super::super::HealthStatus;

/// Internal state of an OAuth-authenticated endpoint.
///
/// Maps to `HealthStatus` in the `health()` method but carries richer
/// semantic meaning for lifecycle management (refresh scheduling,
/// startup restore, management API responses).
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthState {
    /// No tokens, never authenticated.
    NeedsLogin,
    /// Valid tokens, inner adapter healthy.
    Authenticated,
    /// Proactive or reactive refresh in progress.
    Refreshing,
    /// Refresh failed, needs re-login.
    AuthRequired,
    /// Token valid but MCP server unreachable.
    ConnectionFailed,
    /// User explicitly disconnected.
    Disconnected,
}

/// Maximum number of transitions kept in the ring buffer.
pub const TRANSITION_RING_BUFFER_CAPACITY: usize = 16;

/// A record of an OAuthState transition, stored in the ring buffer.
#[derive(Debug, Clone)]
pub struct TransitionRecord {
    pub from: OAuthState,
    pub to: OAuthState,
    pub reason: String,
    pub timestamp: Instant,
}

/// Derive the composite `HealthStatus` from the OAuth lifecycle state and
/// the inner (wrapped) adapter's health.
///
/// This is a **pure function** and the single source of truth for OAuth
/// adapter health. No other code path should produce a `HealthStatus` for
/// an OAuth adapter.
pub fn derive_health(oauth_state: &OAuthState, inner_health: &HealthStatus) -> HealthStatus {
    match oauth_state {
        // Authenticated: propagate the inner adapter's health verbatim.
        OAuthState::Authenticated => inner_health.clone(),
        // Refresh in progress: report Starting regardless of inner state.
        OAuthState::Refreshing => HealthStatus::Starting,
        // Hard-error / terminal states override inner health.
        OAuthState::AuthRequired => HealthStatus::Unhealthy("auth required".into()),
        OAuthState::ConnectionFailed => HealthStatus::Unhealthy("connection failed".into()),
        OAuthState::NeedsLogin => HealthStatus::Unhealthy("needs login".into()),
        OAuthState::Disconnected => HealthStatus::Stopped,
    }
}

/// Debug-assert that a transition from `from` to `to` is legal.
///
/// Panics in debug builds on illegal transitions. In release builds this
/// is a no-op so we never crash the relay in production.
pub fn assert_legal_transition(from: &OAuthState, to: &OAuthState) {
    use OAuthState::*;
    let legal = from == to
        || matches!(
            (from, to),
            (NeedsLogin, Refreshing)
                | (NeedsLogin, AuthRequired)
                | (Refreshing, Authenticated)
                | (Refreshing, AuthRequired)
                | (Authenticated, Refreshing)
                | (Authenticated, ConnectionFailed)
                | (Authenticated, AuthRequired)
                | (Authenticated, Disconnected)
                | (ConnectionFailed, Authenticated)
                | (ConnectionFailed, AuthRequired)
                | (AuthRequired, Refreshing)
                | (AuthRequired, Disconnected)
                | (Disconnected, NeedsLogin)
                | (Disconnected, Refreshing)
        );
    debug_assert!(
        legal,
        "illegal OAuth state transition: {:?} -> {:?}",
        from, to
    );
}

/// Perform a state transition: validate, record in the ring buffer, and
/// update the state. This is a free function so it can be called from
/// `OAuthAdapterInner::transition_to` in mod.rs.
///
/// The caller must hold the state write lock and pass the mutable reference.
/// Returns the old state for logging.
pub fn do_transition(
    current_state: &mut OAuthState,
    new_state: OAuthState,
    reason: &str,
    history: &mut VecDeque<TransitionRecord>,
) -> OAuthState {
    let old_state = current_state.clone();
    assert_legal_transition(&old_state, &new_state);

    if history.len() >= TRANSITION_RING_BUFFER_CAPACITY {
        history.pop_front();
    }
    history.push_back(TransitionRecord {
        from: old_state.clone(),
        to: new_state.clone(),
        reason: reason.to_string(),
        timestamp: Instant::now(),
    });

    *current_state = new_state;
    old_state
}

/// Compute the deadline at which a proactive token refresh should fire.
///
/// Returns the earlier of:
/// - 75 % of token lifetime after `issued_at`
/// - 5 minutes before `expires_at`
///
/// If `expires_at` is unknown (server didn't return `expires_in`), returns
/// `None` — the caller should skip proactive refresh and rely on 401 retry.
pub fn refresh_deadline(issued_at: Instant, expires_at: Instant) -> Instant {
    let lifetime = expires_at - issued_at;
    let seventy_five_pct = issued_at + (lifetime * 3 / 4);
    let five_min_before = expires_at - Duration::from_secs(300);
    std::cmp::min(seventy_five_pct, five_min_before)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive table-driven test: 6 OAuthState variants × 4 HealthStatus
    /// variants = 24 rows covering every (state, inner_health) → HealthStatus
    /// combination.
    #[test]
    fn derive_health_exhaustive_table() {
        let cases: Vec<(OAuthState, HealthStatus, HealthStatus)> = vec![
            // ── Authenticated: propagates inner verbatim ──
            (OAuthState::Authenticated, HealthStatus::Healthy, HealthStatus::Healthy),
            (OAuthState::Authenticated, HealthStatus::Starting, HealthStatus::Starting),
            (
                OAuthState::Authenticated,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Unhealthy("upstream timeout".into()),
            ),
            (OAuthState::Authenticated, HealthStatus::Stopped, HealthStatus::Stopped),
            // ── Refreshing: always Starting ──
            (OAuthState::Refreshing, HealthStatus::Healthy, HealthStatus::Starting),
            (OAuthState::Refreshing, HealthStatus::Starting, HealthStatus::Starting),
            (
                OAuthState::Refreshing,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Starting,
            ),
            (OAuthState::Refreshing, HealthStatus::Stopped, HealthStatus::Starting),
            // ── AuthRequired: always Unhealthy("auth required") ──
            (
                OAuthState::AuthRequired,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("auth required".into()),
            ),
            (
                OAuthState::AuthRequired,
                HealthStatus::Starting,
                HealthStatus::Unhealthy("auth required".into()),
            ),
            (
                OAuthState::AuthRequired,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Unhealthy("auth required".into()),
            ),
            (
                OAuthState::AuthRequired,
                HealthStatus::Stopped,
                HealthStatus::Unhealthy("auth required".into()),
            ),
            // ── ConnectionFailed: always Unhealthy("connection failed") ──
            (
                OAuthState::ConnectionFailed,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("connection failed".into()),
            ),
            (
                OAuthState::ConnectionFailed,
                HealthStatus::Starting,
                HealthStatus::Unhealthy("connection failed".into()),
            ),
            (
                OAuthState::ConnectionFailed,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Unhealthy("connection failed".into()),
            ),
            (
                OAuthState::ConnectionFailed,
                HealthStatus::Stopped,
                HealthStatus::Unhealthy("connection failed".into()),
            ),
            // ── NeedsLogin: always Unhealthy("needs login") ──
            (
                OAuthState::NeedsLogin,
                HealthStatus::Healthy,
                HealthStatus::Unhealthy("needs login".into()),
            ),
            (
                OAuthState::NeedsLogin,
                HealthStatus::Starting,
                HealthStatus::Unhealthy("needs login".into()),
            ),
            (
                OAuthState::NeedsLogin,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Unhealthy("needs login".into()),
            ),
            (
                OAuthState::NeedsLogin,
                HealthStatus::Stopped,
                HealthStatus::Unhealthy("needs login".into()),
            ),
            // ── Disconnected: always Stopped ──
            (OAuthState::Disconnected, HealthStatus::Healthy, HealthStatus::Stopped),
            (OAuthState::Disconnected, HealthStatus::Starting, HealthStatus::Stopped),
            (
                OAuthState::Disconnected,
                HealthStatus::Unhealthy("upstream timeout".into()),
                HealthStatus::Stopped,
            ),
            (OAuthState::Disconnected, HealthStatus::Stopped, HealthStatus::Stopped),
        ];

        // Verify we have exactly 24 cases (6 states × 4 inner variants)
        assert_eq!(cases.len(), 24, "expected 24 test cases (6 states × 4 inner)");

        for (state, inner, expected) in &cases {
            let got = derive_health(state, inner);
            assert_eq!(got, *expected, "state={:?} inner={:?}", state, inner);
        }
    }

    /// Test assert_legal_transition for all legal transitions.
    #[test]
    fn legal_transitions_accepted() {
        let legal_pairs = [
            (OAuthState::NeedsLogin, OAuthState::Refreshing),
            (OAuthState::NeedsLogin, OAuthState::AuthRequired),
            (OAuthState::Refreshing, OAuthState::Authenticated),
            (OAuthState::Refreshing, OAuthState::AuthRequired),
            (OAuthState::Authenticated, OAuthState::Refreshing),
            (OAuthState::Authenticated, OAuthState::ConnectionFailed),
            (OAuthState::Authenticated, OAuthState::AuthRequired),
            (OAuthState::Authenticated, OAuthState::Disconnected),
            (OAuthState::ConnectionFailed, OAuthState::Authenticated),
            (OAuthState::ConnectionFailed, OAuthState::AuthRequired),
            (OAuthState::AuthRequired, OAuthState::Refreshing),
            (OAuthState::AuthRequired, OAuthState::Disconnected),
            (OAuthState::Disconnected, OAuthState::NeedsLogin),
            (OAuthState::Disconnected, OAuthState::Refreshing),
        ];
        for (from, to) in &legal_pairs {
            assert_legal_transition(from, to);
        }
    }

    /// Test that idempotent transitions are legal.
    #[test]
    fn idempotent_transitions_accepted() {
        let states = [
            OAuthState::NeedsLogin,
            OAuthState::Authenticated,
            OAuthState::Refreshing,
            OAuthState::AuthRequired,
            OAuthState::ConnectionFailed,
            OAuthState::Disconnected,
        ];
        for state in &states {
            assert_legal_transition(state, &state.clone());
        }
    }

    /// Test that illegal transitions panic in debug builds.
    #[test]
    #[cfg(debug_assertions)]
    fn illegal_transitions_panic() {
        let illegal_pairs = [
            (OAuthState::NeedsLogin, OAuthState::Authenticated),
            (OAuthState::NeedsLogin, OAuthState::ConnectionFailed),
            (OAuthState::NeedsLogin, OAuthState::Disconnected),
            (OAuthState::Refreshing, OAuthState::NeedsLogin),
            (OAuthState::Refreshing, OAuthState::ConnectionFailed),
            (OAuthState::Refreshing, OAuthState::Disconnected),
            (OAuthState::Authenticated, OAuthState::NeedsLogin),
            (OAuthState::ConnectionFailed, OAuthState::NeedsLogin),
            (OAuthState::ConnectionFailed, OAuthState::Refreshing),
            (OAuthState::ConnectionFailed, OAuthState::Disconnected),
            (OAuthState::AuthRequired, OAuthState::NeedsLogin),
            (OAuthState::AuthRequired, OAuthState::Authenticated),
            (OAuthState::AuthRequired, OAuthState::ConnectionFailed),
            (OAuthState::Disconnected, OAuthState::Authenticated),
            (OAuthState::Disconnected, OAuthState::AuthRequired),
            (OAuthState::Disconnected, OAuthState::ConnectionFailed),
        ];
        for (from, to) in &illegal_pairs {
            let result = std::panic::catch_unwind(|| {
                assert_legal_transition(from, to);
            });
            assert!(
                result.is_err(),
                "expected panic for {:?} -> {:?} but it didn't panic",
                from, to
            );
        }
    }

    /// Test do_transition records in ring buffer and updates state.
    #[test]
    fn do_transition_records_and_updates() {
        let mut state = OAuthState::NeedsLogin;
        let mut history = VecDeque::with_capacity(TRANSITION_RING_BUFFER_CAPACITY);

        let old = do_transition(&mut state, OAuthState::AuthRequired, "test reason", &mut history);
        assert_eq!(old, OAuthState::NeedsLogin);
        assert_eq!(state, OAuthState::AuthRequired);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].from, OAuthState::NeedsLogin);
        assert_eq!(history[0].to, OAuthState::AuthRequired);
        assert_eq!(history[0].reason, "test reason");
    }

    /// Test ring buffer capacity is enforced.
    #[test]
    fn transition_ring_buffer_capacity() {
        let mut state = OAuthState::Authenticated;
        let mut history = VecDeque::with_capacity(TRANSITION_RING_BUFFER_CAPACITY);

        // Fill beyond capacity by toggling Authenticated <-> Refreshing
        for i in 0..(TRANSITION_RING_BUFFER_CAPACITY + 4) {
            if i % 2 == 0 {
                do_transition(&mut state, OAuthState::Refreshing, "proactive", &mut history);
            } else {
                do_transition(
                    &mut state,
                    OAuthState::Authenticated,
                    "refresh done",
                    &mut history,
                );
            }
        }

        assert_eq!(
            history.len(),
            TRANSITION_RING_BUFFER_CAPACITY,
            "ring buffer should be capped at {}",
            TRANSITION_RING_BUFFER_CAPACITY
        );
    }

    // --- refresh_deadline tests (moved from original oauth.rs) ---

    #[test]
    fn refresh_deadline_1h_token() {
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(3600);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(2700);
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }

    #[test]
    fn refresh_deadline_10min_token() {
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(600);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(300);
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }

    #[test]
    fn refresh_deadline_2min_token() {
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(120);
        let deadline = refresh_deadline(issued, expires);
        assert!(deadline <= issued + Duration::from_secs(90));
    }

    #[test]
    fn refresh_deadline_exactly_20min_token() {
        let issued = Instant::now();
        let expires = issued + Duration::from_secs(1200);
        let deadline = refresh_deadline(issued, expires);
        let expected = issued + Duration::from_secs(900);
        assert!(deadline >= expected - Duration::from_millis(1));
        assert!(deadline <= expected + Duration::from_millis(1));
    }
}
