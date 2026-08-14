//! The hub's mixnet IDENTITY, and why it has to outlive a client rebuild.
//!
//! The hub's Nym address is baked into every shim's enclave config, and a
//! Caution managed app is immutable, so an address change costs every operator a
//! re-assemble and a redeploy. The driver used to mint a fresh identity on every
//! rebuild (`MixnetClientBuilder::new_ephemeral()` per attempt), which meant a
//! single gateway blip invalidated the whole shim fleet's configuration.
//!
//! The fix is that one [`Ephemeral`] store is created once and cloned into every
//! build. These tests pin the storage-layer property that makes that work — that
//! a clone carries the SAME keys, so the SDK's initialisation (which loads
//! existing keys before generating any) returns the same identity, and therefore
//! the same address.
//!
//! What this canNOT assert is the address itself: a Nym address is
//! `identity.encryption@gateway`, and the gateway half only exists once a client
//! has registered with one. That needs a live mixnet, which is what
//! `nymnet/localnet.sh e2e-driver` is for — kill the gateway process, let the
//! driver rebuild, and assert the logged address is unchanged. This file is the
//! fast half of that pair; the harness is the half that proves it end to end.

#![cfg(feature = "mixnet-driver")]

use nym_sdk::mixnet::{Ephemeral, KeyStore, MixnetClientStorage};

/// A clone of the store yields the SAME identity, which is what makes a rebuilt
/// client keep the address the shims are configured with.
///
/// The store is `Arc`-backed, so this is sharing rather than copying — stronger
/// than the property the driver needs, and worth pinning because a future SDK
/// bump that made `Ephemeral::clone` deep-copy would silently reintroduce the
/// fleet-invalidating behaviour with no compile error.
#[tokio::test]
async fn a_cloned_store_keeps_the_same_identity() {
    let storage = Ephemeral::default();

    let first = storage
        .key_store()
        .load_keys()
        .await
        .expect("a default Ephemeral generates its keys on construction");
    let second = storage
        .clone()
        .key_store()
        .load_keys()
        .await
        .expect("the clone must carry keys too");

    assert_eq!(
        first.identity_keypair().public_key().to_base58_string(),
        second.identity_keypair().public_key().to_base58_string(),
        "a cloned store must present the same identity: this is the identity half \
         of the hub's Nym address, and changing it re-points every shim"
    );
    assert_eq!(
        first.encryption_keypair().public_key().to_base58_string(),
        second.encryption_keypair().public_key().to_base58_string(),
        "the encryption half of the address must be stable too — deriving only the \
         identity key (as `with_derivation_material` does) still moves the address"
    );
}

/// Two INDEPENDENT stores differ. The mirror of the test above: it is what makes
/// the driver's last-resort fallback (a fresh `Ephemeral` after
/// `REBUILDS_BEFORE_NEW_IDENTITY` failures) actually change the address, and it
/// is why that fallback warns as loudly as it does.
#[tokio::test]
async fn independent_stores_have_different_identities() {
    let first = Ephemeral::default()
        .key_store()
        .load_keys()
        .await
        .expect("keys");
    let second = Ephemeral::default()
        .key_store()
        .load_keys()
        .await
        .expect("keys");

    assert_ne!(
        first.identity_keypair().public_key().to_base58_string(),
        second.identity_keypair().public_key().to_base58_string(),
        "a fresh store must be a fresh identity, or the fallback would not recover \
         from an unusable gateway registration"
    );
}
