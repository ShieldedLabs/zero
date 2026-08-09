//! zero-indexer-hub (ZIH): broadcasts diverted migration transactions.
//!
//! Shims divert Orchard-touching transactions here instead of handing them to
//! their operator's indexer, so the operator's indexer never sees a migration's
//! contents. That is **content privacy**, and it is the first of two properties.
//!
//! The second is the **batch**. A submission is not broadcast on arrival: it is
//! queued and published at the next cadence boundary, simultaneously with
//! everything else admitted in that window and in an order nobody can predict.
//! The batch IS the anonymity set, which is why the queue and the flush were
//! reviewed adversarially (`REVIEW.md`) before being written.
//!
//! **The honest bound on that claim, which must be stated wherever the claim
//! is.** At the measured mainnet rate of 0.77 Orchard-touching transactions per
//! block and realistic launch adoption of one to a few operators, the modal
//! published batch is 0 or 1. For a size-1 batch the anonymity set is the
//! transaction itself, and the shuffle, the simultaneous publish, Nym and the
//! enclave are all irrelevant to it. The property is real but conditional on
//! adoption, and there is no fix at v1: holding for a bigger batch needs a
//! window the wallet expiry does not permit, decoys cannot cover the class that
//! matters, and refusing to publish routes the transaction somewhere worse. The
//! hub therefore measures and exports its achieved batch size rather than
//! asserting the property.
//!
//! Layering:
//!
//! * [`chain`] is the connection to the network, through an indexer: tip in,
//!   transactions out.
//! * [`tls`] verifies that connection, which an enclave deployment requires.
//! * [`config`] is the command-line and environment surface.
//! * [`queue`] holds admitted migrations, and decides what is admitted at all.
//! * [`batcher`] drives the flush cadence and owns the hub's view of the tip.
//! * [`server`] is the inbound serving path: receive a migration, admit it.

#![forbid(unsafe_code)]

pub mod batcher;
pub mod chain;
pub mod config;
pub mod queue;
pub mod server;
pub mod tls;

/// Boxed error type shared across the hub.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
