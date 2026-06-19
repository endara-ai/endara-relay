//! Test-only support for stabilising `tracing` callsite-interest caching.
//!
//! tracing's callsite-interest cache is process-global and is fixed by the
//! first thread to touch a callsite. Tests that route through production code
//! without installing a subscriber would otherwise register shared callsites
//! (e.g. the `request` span in `server::mcp_*_logged`) as `Interest::never()`
//! under the no-op default, statically disabling them for the capturing tests
//! that later assert on those spans. Installing a permissive global default —
//! one that never returns `never` but drops everything via `enabled() == false`
//! — keeps every callsite dynamically re-evaluated against whatever scoped
//! subscriber a test installs with `with_default`/`set_default`.

use std::sync::OnceLock;
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

struct PermissiveSubscriber;

impl Subscriber for PermissiveSubscriber {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

static INIT: OnceLock<()> = OnceLock::new();

/// Idempotently install the permissive global default subscriber. Safe to call
/// from every test; only the first call wins.
pub(crate) fn init_permissive_tracing() {
    INIT.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(PermissiveSubscriber);
    });
}
