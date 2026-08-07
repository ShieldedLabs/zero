//! Recognition state the shim keeps about migrations it has diverted, so a
//! follow-up query that names one can be routed away from the operator's
//! indexer.
//!
//! All of it is in RAM. The enclave is diskless, and this is per-process state
//! that must never touch a disk an operator could read. It is held behind an
//! `RwLock` because reads (query interception) vastly outnumber writes (a
//! divert), and the lock is recovered rather than unwrapped on poison: a panic
//! in an enclave destroys this map for every in-flight migration at once.

use std::collections::HashMap;
use std::sync::RwLock;

use bytes::Bytes;

/// The shared, thread-safe divert state.
#[derive(Default)]
pub struct DivertState {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    /// txid (lowercase hex) -> the exact bytes the shim diverted and holds.
    migrations: HashMap<String, Bytes>,
    /// transparent address -> the diverted migration txid that touched it.
    tainted: HashMap<String, String>,
}

impl DivertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a diverted migration: its bytes under its txid, and every
    /// transparent address it touched as tainted by that txid.
    pub fn record(&self, txid: String, tx_bytes: Bytes, addresses: Vec<String>) {
        let mut inner = self.inner.write().unwrap_or_else(|poison| poison.into_inner());
        for addr in addresses {
            inner.tainted.insert(addr, txid.clone());
        }
        inner.migrations.insert(txid, tx_bytes);
    }

    /// The bytes held for a diverted txid, if this shim diverted it.
    pub fn migration_bytes(&self, txid: &str) -> Option<Bytes> {
        let inner = self.inner.read().unwrap_or_else(|poison| poison.into_inner());
        inner.migrations.get(txid).cloned()
    }

    /// The diverted migration txid that tainted an address, if any.
    pub fn taint(&self, address: &str) -> Option<String> {
        let inner = self.inner.read().unwrap_or_else(|poison| poison.into_inner());
        inner.tainted.get(address).cloned()
    }

    /// Count of held migrations. For a health line only; never a value returned
    /// down a wallet channel, where it would be a live anonymity-set-size oracle.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .migrations
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_migration_is_retrievable_and_taints_its_addresses() {
        let state = DivertState::new();
        state.record(
            "aa".into(),
            Bytes::from_static(b"txbytes"),
            vec!["t1abc".into(), "t1def".into()],
        );

        assert_eq!(state.migration_bytes("aa").as_deref(), Some(&b"txbytes"[..]));
        assert_eq!(state.taint("t1abc").as_deref(), Some("aa"));
        assert_eq!(state.taint("t1def").as_deref(), Some("aa"));
        assert!(state.taint("t1zzz").is_none());
        assert!(state.migration_bytes("bb").is_none());
        assert_eq!(state.len(), 1);
    }
}
