//! Harness-local patch of `nym-upgrade-mode-check` (see Cargo.toml for why).
//!
//! The `UpgradeModeAttestation` data types are copied verbatim from the pinned
//! upstream (`nym-binaries-v2026.15-bydgoszcz`) because `nym-node-requests`
//! embeds them in API models. The JWT functions are stubbed WITHOUT jwt-simple:
//! validation reports the token malformed (upgrade-mode tokens simply never
//! validate in the harness; nothing in the client-side flows we exercise mints
//! or accepts one) and generation panics, so any codepath that unexpectedly
//! relied on it fails loudly in the e2e probe instead of silently passing.

use nym_crypto::asymmetric::ed25519;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

pub const UPGRADE_MODE_CREDENTIAL_TYPE: &str = "upgrade_mode_jwt";

pub const CREDENTIAL_PROXY_JWT_ISSUER: &str = "nym-credential-proxy";

#[derive(Debug, Error)]
pub enum UpgradeModeCheckError {
    #[error("the upgrade mode JWT is malformed")]
    MalformedToken,

    #[error("the jwt metadata didn't contain explicit public key")]
    MissingTokenPublicKey,

    #[error("the jwt signer does not appear in the authorised attestation set")]
    UnauthorisedIssuer,

    #[error("the attached public key was not valid ed25519 public key")]
    MalformedEd25519PublicKey {
        source: ed25519::Ed25519RecoveryError,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpgradeModeAttestation {
    #[serde(flatten)]
    pub content: UpgradeModeAttestationContent,

    #[serde(with = "ed25519::bs58_ed25519_signature")]
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub signature: ed25519::Signature,
}

impl UpgradeModeAttestation {
    pub fn authorised_to_issue_jwt(&self, key: &ed25519::PublicKey) -> bool {
        self.content.authorised_jwt_issuers.contains(key)
    }

    pub fn verify(&self) -> bool {
        self.content
            .attester_public_key
            .verify(self.content.as_json(), &self.signature)
            .is_ok()
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "type")]
#[serde(rename = "upgrade_mode")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpgradeModeAttestationContent {
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub starting_time: OffsetDateTime,

    #[serde(with = "ed25519::bs58_ed25519_pubkey")]
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub attester_public_key: ed25519::PublicKey,

    #[serde(with = "ed25519::vec_bs58_ed25519_pubkey")]
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
    pub authorised_jwt_issuers: Vec<ed25519::PublicKey>,
}

impl UpgradeModeAttestationContent {
    pub fn as_json(&self) -> String {
        // SAFETY: Serialize impl is valid and we have no non-string map keys
        #[allow(clippy::unwrap_used)]
        serde_json::to_string(&self).unwrap()
    }
}

pub fn generate_new_attestation(
    key: &ed25519::PrivateKey,
    authorised_jwt_issuers: Vec<ed25519::PublicKey>,
) -> UpgradeModeAttestation {
    generate_new_attestation_with_starting_time(
        key,
        authorised_jwt_issuers,
        OffsetDateTime::now_utc(),
    )
}

pub fn generate_new_attestation_with_starting_time(
    key: &ed25519::PrivateKey,
    authorised_jwt_issuers: Vec<ed25519::PublicKey>,
    starting_time: OffsetDateTime,
) -> UpgradeModeAttestation {
    let content = UpgradeModeAttestationContent {
        starting_time,
        attester_public_key: key.into(),
        authorised_jwt_issuers,
    };
    UpgradeModeAttestation {
        signature: key.sign(content.as_json()),
        content,
    }
}

/// Stub: nothing in the harness mints upgrade-mode JWTs. Panics so a codepath
/// that unexpectedly needs one fails loudly rather than producing garbage.
pub fn generate_jwt_for_upgrade_mode_attestation(
    _attestation: UpgradeModeAttestation,
    _validity: std::time::Duration,
    _keys: &ed25519::KeyPair,
    _issuer: Option<&'static str>,
) -> String {
    unimplemented!("jwt generation is stubbed in the zeronym nymnet harness")
}

/// Stub: upgrade-mode tokens never validate in the harness.
pub fn validate_upgrade_mode_jwt(
    _token: &str,
    _expected_issuer: Option<&'static str>,
) -> Result<UpgradeModeAttestation, UpgradeModeCheckError> {
    Err(UpgradeModeCheckError::MalformedToken)
}

/// Stub: upgrade-mode tokens never decode in the harness.
pub fn try_decode_upgrade_mode_jwt_claims(
    _token: &str,
) -> Result<UpgradeModeAttestation, UpgradeModeCheckError> {
    Err(UpgradeModeCheckError::MalformedToken)
}
