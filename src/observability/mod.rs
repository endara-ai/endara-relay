//! Agent call observability: a two-tier store keyed by `request_uid`.
//!
//! - [`store`] persists per-`tools/call` metadata in SQLite (durable, indexed,
//!   retention- and size-capped).
//! - [`payloads`] holds full request/response bodies in an in-memory ring
//!   buffer (ephemeral, time- and memory-capped).
//!
//! - [`pipeline`] is the async ingestion pipeline (R4) feeding both tiers: a
//!   bounded, overflow-dropping channel drained by a background consumer.

pub mod payloads;
pub mod pipeline;
pub mod store;
