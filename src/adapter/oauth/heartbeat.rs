//! Heartbeat probe for the inner (wrapped) adapter.
//!
//! Stubs — implementation will be added in a follow-up slice.

use super::OAuthAdapterInner;
use std::sync::{Arc, Weak};

/// Periodically probe the inner adapter to update `inner_health`.
///
/// Stub — will be implemented in a follow-up slice.
pub async fn heartbeat_loop(_inner: Weak<OAuthAdapterInner>) {
    // TODO: implement heartbeat polling
}

/// Single probe of the inner adapter health.
///
/// Stub — will be implemented in a follow-up slice.
pub async fn probe_inner(_inner: &Arc<OAuthAdapterInner>) {
    // TODO: implement single health probe
}
