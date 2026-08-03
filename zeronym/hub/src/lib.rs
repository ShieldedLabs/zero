//! zero-indexer-hub (ZIH): the batching broadcaster.
//!
//! Shims divert migration transactions here instead of handing them to their
//! operator's indexer. The hub collects migrations from many shims, holds them
//! briefly, and publishes them to the Zcash network together, so that an
//! observer sees a group of migrations appear at once with nothing to say which
//! shim, operator or user each came from.
//!
//! **The batch is the anonymity set.** Every other property here is in service
//! of that one, which is why flush timing and publish ordering are treated as
//! security-critical rather than as scheduling details.
//!
//! Layering:
//!
//! * [`chain`] is the connection to the Zcash network: tip in, transactions out.
//!
//! Not yet written, and deliberately so: the inbound channel, the queue, and
//! the flush. The channel's wire form waits on STEVE decisions that are not
//! ours to make, and the queue and flush wait on an adversarial review of the
//! batching design, because building them first would mean building the part
//! most likely to be wrong.

#![forbid(unsafe_code)]

pub mod chain;

/// Boxed error type shared across the hub.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
