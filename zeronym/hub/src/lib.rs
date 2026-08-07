//! zero-indexer-hub (ZIH): broadcasts diverted migration transactions.
//!
//! Shims divert Orchard-touching transactions here instead of handing them to
//! their operator's indexer, so the operator's indexer never sees a migration's
//! contents. Near-term this hub broadcasts each migration the moment it arrives.
//! That is **content privacy**: it keeps the transaction out of the operator's
//! view, but it does NOT hide timing (the operator still sees that a wallet sent
//! *something* it did not forward) and it forms no anonymity set. At this scope
//! the "batch is the anonymity set" property is simply not in play.
//!
//! The batching layer that would provide that anonymity, the queue and the
//! flush, is designed in `REVIEW.md` and deliberately deferred. Two reasons: at
//! launch adoption the modal batch is 0 or 1 transactions, so batching buys
//! almost nothing until volume grows; and it is the part most likely to be
//! wrong, which is why it was reviewed adversarially before being built. Content
//! privacy is the layer that delivers a real, honest gain now.
//!
//! Layering:
//!
//! * [`chain`] is the connection to the Zcash network: tip in, transactions out.
//! * [`config`] is the command-line and environment surface.
//! * [`server`] is the inbound serving path: receive a migration, broadcast it.

#![forbid(unsafe_code)]

pub mod chain;
pub mod config;
pub mod server;

/// Boxed error type shared across the hub.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
