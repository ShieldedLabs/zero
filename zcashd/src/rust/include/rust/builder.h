// Copyright (c) 2022-2023 The Zcash developers
// Distributed under the MIT software license, see the accompanying
// file COPYING or https://www.opensource.org/licenses/mit-license.php .

#ifndef ZCASH_RUST_INCLUDE_RUST_BUILDER_H
#define ZCASH_RUST_INCLUDE_RUST_BUILDER_H

#include "rust/orchard.h"
#include "rust/orchard/keys.h"
#include "rust/transaction.h"

#ifdef __cplusplus
// Do NOT include "rust/bridge.h" here: the generated bridge.h includes THIS
// header (to obtain the Orchard*Ptr C typedefs below) before it declares the
// `orchard` cxx types, so including it back would form a cycle in which each
// header uses a type the other hasn't declared yet. Forward-declare the two
// cxx structs we reference; they are only used by value in declarations, which
// does not require a complete type.
namespace orchard {
struct BundleVersion;
struct Flags;
}

extern "C" {
#endif

/// A type-safe pointer to a Rust-allocated struct containing the information
/// needed to spend an Orchard note.
struct OrchardSpendInfoPtr;
typedef struct OrchardSpendInfoPtr OrchardSpendInfoPtr;

/// Pointer to Rust-allocated Orchard bundle builder.
struct OrchardBuilderPtr;
typedef struct OrchardBuilderPtr OrchardBuilderPtr;

/// Pointer to Rust-allocated Orchard bundle without proofs
/// or authorizing data.
struct OrchardUnauthorizedBundlePtr;
typedef struct OrchardUnauthorizedBundlePtr OrchardUnauthorizedBundlePtr;

/// Frees the memory associated with an Orchard spend info struct that was
/// allocated by Rust.
void orchard_spend_info_free(OrchardSpendInfoPtr* ptr);

/// Construct a new Orchard transaction builder.
///
/// If `anchor` is `null`, the root of the empty Orchard commitment tree is used.
OrchardBuilderPtr* orchard_builder_new(
    bool coinbase,
    orchard::BundleVersion bundle_version,
    orchard::Flags flags,
    const unsigned char* anchor);

/// Frees an Orchard builder returned from `orchard_builder_new`.
void orchard_builder_free(OrchardBuilderPtr* ptr);

/// Adds a note to be spent in this bundle.
///
/// Returns `false` if the Merkle path in `spend_info` does not have the
/// required anchor.
///
/// `spend_info` is always freed by this method, whether or not it succeeds.
bool orchard_builder_add_spend(
    OrchardBuilderPtr* ptr,
    OrchardSpendInfoPtr* spend_info);

/// Adds an address which will receive funds in this bundle.
///
/// `ovk` is a pointer to the outgoing viewing key to make this recipient recoverable by,
/// or `null` to make the recipient unrecoverable by the sender.
///
/// `memo` is a pointer to the 512-byte memo field encoding, or `null` for "no memo".
bool orchard_builder_add_recipient(
    OrchardBuilderPtr* ptr,
    const unsigned char* ovk,
    const OrchardRawAddressPtr* recipient,
    uint64_t value,
    const unsigned char* memo);

/// Adds a wallet-controlled change output owned by `full_viewing_key`.
///
/// Unlike `orchard_builder_add_recipient`, this is permitted in bundles that
/// disable cross-address transfers (such as the Orchard pool under protocol V3).
/// The paired fabricated spend is authorized at proving time by the spending key
/// matching `full_viewing_key`, which must be supplied to
/// `orchard_unauthorized_bundle_prove_and_sign`.
///
/// `ovk` is a pointer to the outgoing viewing key to make this output recoverable
/// by, or `null`. `memo` is a pointer to the 512-byte memo field encoding, or
/// `null` for "no memo".
bool orchard_builder_add_change_output(
    OrchardBuilderPtr* ptr,
    const OrchardFullViewingKeyPtr* full_viewing_key,
    const unsigned char* ovk,
    const OrchardRawAddressPtr* recipient,
    uint64_t value,
    const unsigned char* memo);

/// Builds a bundle containing the given spent notes and recipients.
///
/// Returns `null` if an error occurs.
///
/// `builder` is always freed by this method.
OrchardUnauthorizedBundlePtr* orchard_builder_build(OrchardBuilderPtr* builder);

/// Frees an Orchard bundle returned from `orchard_bundle_build`.
void orchard_unauthorized_bundle_free(OrchardUnauthorizedBundlePtr* bundle);

/// Adds proofs and signatures to the bundle.
///
/// Returns `null` if an error occurs.
///
/// `bundle` is always freed by this method.
///
/// The proving key's circuit version is taken from the bundle itself (selected via the
/// `use_fixed_circuit_for_proving` argument to `orchard_builder_new`), so it cannot disagree with the
/// circuit the bundle's actions were built against.
OrchardBundlePtr* orchard_unauthorized_bundle_prove_and_sign(
    OrchardUnauthorizedBundlePtr* bundle,
    const OrchardSpendingKeyPtr** keys,
    size_t keys_len,
    const unsigned char* sighash);

#ifdef __cplusplus
}
#endif

#endif // ZCASH_RUST_INCLUDE_RUST_BUILDER_H
