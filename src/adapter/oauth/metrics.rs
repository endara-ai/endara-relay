//! In-process metric counters for the OAuth adapter.
//!
//! These are simple `AtomicU64` stubs intended for JSON export via the
//! management API. A future Prometheus integration can read the same
//! atomics.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// Relaxed ordering is sufficient — we only need eventual visibility,
/// not cross-counter consistency.
const ORD: Ordering = Ordering::Relaxed;

/// In-process OAuth metrics counters.
///
/// All counters use `AtomicU64` so they can be incremented from any
/// task without locking.
pub struct OAuthMetrics {
    /// Total successful token refreshes.
    pub token_refresh_success: AtomicU64,
    /// Total failed token refreshes.
    pub token_refresh_failure: AtomicU64,
    /// Total healthy heartbeat probes.
    pub heartbeat_healthy: AtomicU64,
    /// Total unhealthy heartbeat probes.
    pub heartbeat_unhealthy: AtomicU64,
    /// Total state transitions recorded.
    pub state_transitions: AtomicU64,
}

impl OAuthMetrics {
    /// Create a new zeroed metrics instance.
    pub fn new() -> Self {
        Self {
            token_refresh_success: AtomicU64::new(0),
            token_refresh_failure: AtomicU64::new(0),
            heartbeat_healthy: AtomicU64::new(0),
            heartbeat_unhealthy: AtomicU64::new(0),
            state_transitions: AtomicU64::new(0),
        }
    }

    /// Increment the token refresh success counter.
    pub fn inc_refresh_success(&self) {
        self.token_refresh_success.fetch_add(1, ORD);
    }

    /// Increment the token refresh failure counter.
    pub fn inc_refresh_failure(&self) {
        self.token_refresh_failure.fetch_add(1, ORD);
    }

    /// Increment the heartbeat healthy counter.
    pub fn inc_heartbeat_healthy(&self) {
        self.heartbeat_healthy.fetch_add(1, ORD);
    }

    /// Increment the heartbeat unhealthy counter.
    pub fn inc_heartbeat_unhealthy(&self) {
        self.heartbeat_unhealthy.fetch_add(1, ORD);
    }

    /// Increment the state transition counter.
    pub fn inc_state_transition(&self) {
        self.state_transitions.fetch_add(1, ORD);
    }

    /// Snapshot all counters into a serializable struct.
    pub fn snapshot(&self) -> OAuthMetricsSnapshot {
        OAuthMetricsSnapshot {
            oauth_token_refresh_total_success: self.token_refresh_success.load(ORD),
            oauth_token_refresh_total_failure: self.token_refresh_failure.load(ORD),
            oauth_heartbeat_probe_total_healthy: self.heartbeat_healthy.load(ORD),
            oauth_heartbeat_probe_total_unhealthy: self.heartbeat_unhealthy.load(ORD),
            oauth_state_transition_total: self.state_transitions.load(ORD),
        }
    }
}

impl Default for OAuthMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Serializable snapshot of OAuth metrics counters.
#[derive(Debug, Clone, Serialize)]
pub struct OAuthMetricsSnapshot {
    pub oauth_token_refresh_total_success: u64,
    pub oauth_token_refresh_total_failure: u64,
    pub oauth_heartbeat_probe_total_healthy: u64,
    pub oauth_heartbeat_probe_total_unhealthy: u64,
    pub oauth_state_transition_total: u64,
}

/// Generate a short correlation ID (8-char hex) for grouping related log entries.
pub fn generate_correlation_id() -> String {
    let id = uuid::Uuid::new_v4();
    // Take first 8 hex chars for brevity
    id.simple().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_increment_correctly() {
        let m = OAuthMetrics::new();
        assert_eq!(m.snapshot().oauth_token_refresh_total_success, 0);
        assert_eq!(m.snapshot().oauth_token_refresh_total_failure, 0);
        assert_eq!(m.snapshot().oauth_heartbeat_probe_total_healthy, 0);

        m.inc_refresh_success();
        m.inc_refresh_success();
        m.inc_refresh_failure();
        m.inc_heartbeat_healthy();
        m.inc_heartbeat_healthy();
        m.inc_heartbeat_healthy();
        m.inc_heartbeat_unhealthy();
        m.inc_state_transition();
        m.inc_state_transition();

        let snap = m.snapshot();
        assert_eq!(snap.oauth_token_refresh_total_success, 2);
        assert_eq!(snap.oauth_token_refresh_total_failure, 1);
        assert_eq!(snap.oauth_heartbeat_probe_total_healthy, 3);
        assert_eq!(snap.oauth_heartbeat_probe_total_unhealthy, 1);
        assert_eq!(snap.oauth_state_transition_total, 2);
    }

    #[test]
    fn correlation_id_is_8_char_hex() {
        let id = generate_correlation_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        // Two IDs should be different (with overwhelming probability)
        let id2 = generate_correlation_id();
        assert_ne!(id, id2);
    }
}
