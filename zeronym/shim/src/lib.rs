//! zero-indexer-shim (ZIS): a transparent reverse proxy for the light-wallet
//! indexer API.
//!
//! An operator puts the shim in front of their existing lightwalletd or Zaino.
//! Every CompactTxStreamer method, stream, and gRPC trailer is forwarded to the
//! backing indexer unchanged. The single exception is `SendTransaction`, whose
//! body is decoded and classified by [`classify`]. In production a transaction
//! that carries ANY Orchard actions is diverted away from the operator's
//! indexer, whatever its value balances say and wherever the value went; in this
//! proof of concept the verdict is only logged, and the transaction is still
//! forwarded. Ironwood-only transactions are ordinary commerce and pass through.
//!
//! Layering, smallest and highest-stakes first:
//!
//! * [`classify`] is a pure function from raw transaction bytes to a verdict.
//!   No I/O, no state, no config. This is the part to audit line by line.
//! * [`intercept`] unwraps one buffered unary `SendTransaction` body down to
//!   those bytes (gRPC framing, then protobuf), logs the verdict, and replays
//!   the original bytes upstream.
//! * [`proxy`] is the h2c reverse proxy: everything else is opaque and is
//!   relayed frame for frame, trailers included.
//! * [`config`] is two socket addresses.
//!
//! * [`tls`] terminates the wallet-facing link (ACME, key born in the enclave)
//!   and originates the backend link (WebPKI). Both are optional: with no TLS
//!   configured the shim serves plaintext h2c, which is what the tests and a
//!   local demo use.
//!
//! Diversion is being built on top of this PoC. The hub client ([`hub`]) and the
//! divert-recognition state ([`state`]) have landed; the proxy wiring that makes
//! the shim actually divert, and interception of a diverted migration's
//! follow-up queries, are in progress. Nym and STEVE remain out of scope. The
//! enclave and attestation are no longer out of scope; see `deploy/caution/`.
//!
//! The crate is a library with a thin binary wrapper so tests can bind
//! ephemeral ports and drive the proxy in-process.

#![forbid(unsafe_code)]

pub mod classify;
pub mod config;
pub mod hub;
pub mod intercept;
pub mod proxy;
pub mod state;
pub mod tls;

/// Boxed error type shared by the proxy paths.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub use proxy::{serve, serve_with_shutdown};
