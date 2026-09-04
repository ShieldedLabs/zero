//! [zero] @claude Trial decryption of compact blocks against a merchant's viewing key.

use std::collections::HashMap;

use orchard::note_encryption::{CompactAction, OrchardDomain};
use sapling::note_encryption::{CompactOutputDescription, SaplingDomain};
use serde::{Deserialize, Serialize};
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_keys::keys::UnifiedIncomingViewingKey;
use zcash_note_encryption::try_compact_note_decryption;
use zcash_protocol::consensus::{BlockHeight, Network};

use crate::enforcement;

/// A payment found in a block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payment {
    /// Height of the block it was mined in.
    pub height: u32,
    /// Transaction id, hex-encoded.
    pub txid: String,
    /// Value in zatoshis.
    pub zatoshis: u64,
    /// Diversifier index of the address paid, when it is one the caller minted.
    ///
    /// `None` means the payment decrypted with this viewing key but landed on an address the
    /// caller did not supply — funds arriving outside the invoice flow, not an error.
    pub index: Option<u32>,
    /// Which pool the note is in.
    pub pool: &'static str,
}

/// Scans blocks for payments to any of `addresses`.
///
/// `addresses` maps a diversifier index to the encoded address minted for it. Matching is by
/// address rather than by memo, so it needs nothing from the payer's wallet.
pub fn scan_blocks(
    params: &Network,
    uivk: &UnifiedIncomingViewingKey,
    addresses: &HashMap<String, u32>,
    blocks: &[CompactBlock],
) -> Vec<Payment> {
    let sapling_ivk = uivk.sapling().as_ref().map(|ivk| ivk.prepare());
    let orchard_ivk = uivk.orchard().as_ref().map(|ivk| ivk.prepare());
    let mut found = Vec::new();

    for block in blocks {
        let height = BlockHeight::from_u32(block.height as u32);
        // Derived per block rather than fixed once: the rules differ either side of Canopy,
        // and a scan of older history with the wrong setting silently finds nothing.
        let sapling_domain = SaplingDomain::new(enforcement(params, height));

        for tx in &block.vtx {
            let txid = hex::encode(&tx.txid);

            if let Some(ivk) = &sapling_ivk {
                for out in &tx.outputs {
                    let Ok(output) = CompactOutputDescription::try_from(out) else {
                        continue;
                    };
                    if let Some((note, recipient)) =
                        try_compact_note_decryption(&sapling_domain, ivk, &output)
                    {
                        found.push(Payment {
                            height: block.height as u32,
                            txid: txid.clone(),
                            zatoshis: note.value().inner(),
                            index: addresses
                                .get(&crate::address_string(params, &recipient))
                                .copied(),
                            pool: "sapling",
                        });
                    }
                }
            }

            if let Some(ivk) = &orchard_ivk {
                for act in &tx.actions {
                    let Ok(action) = CompactAction::try_from(act) else {
                        continue;
                    };
                    let domain = OrchardDomain::for_compact_action(&action);
                    if let Some((note, _recipient)) =
                        try_compact_note_decryption(&domain, ivk, &action)
                    {
                        // Orchard invoice addresses are not minted here yet, so an Orchard
                        // note is reported without an index rather than silently dropped —
                        // money that arrived is money the merchant should see.
                        found.push(Payment {
                            height: block.height as u32,
                            txid: txid.clone(),
                            zatoshis: note.value().inner(),
                            index: None,
                            pool: "orchard",
                        });
                    }
                }
            }
        }
    }
    found
}
