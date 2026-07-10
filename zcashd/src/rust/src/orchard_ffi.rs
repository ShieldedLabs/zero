use std::convert::TryInto;

use rand_core::OsRng;
use tracing::{debug, error};

use crate::{
    bundlecache::{orchard_bundle_validity_cache, orchard_bundle_validity_cache_mut, CacheEntries},
    orchard_bundle::Bundle,
};

struct BatchValidatorInner {
    // The lifetime here applies to a reference to a static VerifyingKey
    validator: orchard::bundle::BatchValidator<'static>,
    queued_entries: CacheEntries,
}

pub(crate) struct BatchValidator(Option<BatchValidatorInner>);

/// Creates an Orchard bundle batch validation context for the circuit in force at the height
/// of the block being validated: the NU6.2 fixed circuit if `nu6_2_active`, else the pre-NU6.2
/// circuit.
pub(crate) fn orchard_batch_validation_init(
    cache_store: bool,
    circuit_version: crate::bridge::ffi::OrchardCircuitVersion,
) -> Box<BatchValidator> {
    // NOTE(azmr): we convert back to the real type ASAP to try to catch future enum variants
    let circuit_version = match circuit_version {
        crate::bridge::ffi::OrchardCircuitVersion::PostNu6_3 => {
            orchard::circuit::OrchardCircuitVersion::PostNu6_3
        }
        crate::bridge::ffi::OrchardCircuitVersion::FixedPostNu6_2 => {
            orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2
        }
        crate::bridge::ffi::OrchardCircuitVersion::InsecurePreNu6_2 => {
            orchard::circuit::OrchardCircuitVersion::InsecurePreNu6_2
        }
        _ => panic!(
            "invalid orchard circuit version enum value: {}",
            circuit_version.repr
        ),
    };
    let vk: &'static orchard::circuit::VerifyingKey = match circuit_version {
        orchard::circuit::OrchardCircuitVersion::PostNu6_3 => &crate::ORCHARD_VK_NU6_3,
        orchard::circuit::OrchardCircuitVersion::FixedPostNu6_2 => &crate::ORCHARD_VK_FIXED,
        orchard::circuit::OrchardCircuitVersion::InsecurePreNu6_2 => &crate::ORCHARD_VK_INSECURE,
    };
    Box::new(BatchValidator(Some(BatchValidatorInner {
        validator: orchard::bundle::BatchValidator::new(vk),
        queued_entries: CacheEntries::new(cache_store),
    })))
}

impl BatchValidator {
    /// Adds an Orchard-protocol bundle to the validation batch (proof, spend-auth
    /// signatures, and binding signature), unless its validity is already cached.
    ///
    /// `format` names the slot this bundle occupies in its transaction; it selects
    /// the digest domain for the validity-cache entry (a v6 Orchard bundle digests
    /// differently from a v5 one, and the bundle alone cannot tell those apart).
    ///
    /// Returns `false` if the bundle cannot be queued — e.g. it requires a circuit
    /// feature the batch's verifying key does not support, or it is inconsistent
    /// with `format` — in which case the transaction must be treated as invalid.
    pub(crate) fn add_bundle(
        &mut self,
        bundle: Box<Bundle>,
        sighash: [u8; 32],
        format: crate::bridge::ffi::BundleFormat,
    ) -> bool {
        let batch = self.0.as_mut();
        let bundle = bundle.inner();

        match (batch, bundle) {
            (Some(batch), Some(bundle)) => {
                let cache = orchard_bundle_validity_cache();

                let tx_version = match format {
                    crate::bridge::ffi::BundleFormat::V5 => orchard::bundle::TxVersion::V5,
                    crate::bridge::ffi::BundleFormat::V6Orchard
                    | crate::bridge::ffi::BundleFormat::V6Ironwood => {
                        orchard::bundle::TxVersion::V6
                    }
                    // cxx shared enums are open sets in Rust
                    _ => return false,
                };

                // Compute the cache entry for this bundle. The commitments fail only
                // if the bundle's version is incompatible with the transaction format
                // the caller says it occupies; such a bundle cannot be valid there.
                let cache_entry = {
                    let Ok(bundle_commitment) = bundle.commitment(tx_version) else {
                        return false;
                    };
                    let Ok(bundle_authorizing_commitment) =
                        bundle.authorizing_commitment(tx_version)
                    else {
                        return false;
                    };
                    cache.compute_entry(
                        bundle_commitment.0.as_bytes().try_into().unwrap(),
                        bundle_authorizing_commitment
                            .0
                            .as_bytes()
                            .try_into()
                            .unwrap(),
                        &sighash,
                    )
                };

                // Check if this bundle's validation result exists in the cache.
                if !cache.contains(cache_entry, &mut batch.queued_entries) {
                    // The bundle has been added to `inner.queued_entries` because it
                    // was not in the cache. We now add its authorization to the
                    // validation batch. This fails only if the bundle requires a
                    // circuit feature the batch's verifying key does not support
                    // (e.g. a cross-address-restricted bundle against a pre-NU6.3
                    // key); such a bundle is invalid in this batch's epoch.
                    if batch.validator.add_bundle(bundle, sighash).is_err() {
                        return false;
                    }
                }

                true
            }
            (Some(_), None) => {
                debug!("Tx has no Orchard component");
                true
            }
            (None, _) => {
                error!("orchard::BatchValidator has already been used");
                false
            }
        }
    }

    /// Validates this batch.
    ///
    /// - Returns `true` if `batch` is null.
    /// - Returns `false` if any item in the batch is invalid.
    ///
    /// The batch validation context is freed by this function.
    ///
    /// ## Consensus rules
    ///
    /// [§4.6](https://zips.z.cash/protocol/protocol.pdf#actiondesc):
    /// - Canonical element encodings are enforced by [`orchard_bundle_parse`].
    /// - SpendAuthSig^Orchard validity is enforced here.
    /// - Proof validity is enforced here.
    ///
    /// [§7.1](https://zips.z.cash/protocol/protocol.pdf#txnencodingandconsensus):
    /// - `bindingSigOrchard` validity is enforced here.
    pub(crate) fn validate(&mut self) -> bool {
        if let Some(inner) = self.0.take() {
            // The verifying key for this batch's circuit was fixed at construction
            // (`orchard_batch_validation_init`).
            if inner.validator.validate(OsRng) {
                // `BatchValidator::validate()` is only called if every
                // `BatchValidator::check_bundle()` returned `true`, so at this point
                // every bundle that was added to `inner.queued_entries` has valid
                // authorization.
                orchard_bundle_validity_cache_mut().insert(inner.queued_entries);
                true
            } else {
                false
            }
        } else {
            error!("orchard::BatchValidator has already been used");
            false
        }
    }
}
