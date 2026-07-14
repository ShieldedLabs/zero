use std::collections::HashSet;
use std::num::NonZeroU32;

use documented::Documented;
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorCode as RpcErrorCode, ErrorObjectOwned as RpcError},
};
use schemars::JsonSchema;
use serde::Serialize;

use transparent::{
    address::{Script, TransparentAddress},
    bundle::{OutPoint, TxOut},
    keys::TransparentKeyScope,
};
use zcash_client_backend::{
    address::UnifiedAddress,
    data_api::{
        Account, AccountPurpose, CoinbaseFilter, InputSource, WalletRead,
        wallet::{ConfirmationsPolicy, TargetHeight},
    },
    encoding::AddressCodec,
    fees::{orchard::InputView as _, sapling::InputView as _},
    wallet::{NoteId, WalletTransparentOutput},
};
use zcash_client_sqlite::AccountUuid;
use zcash_keys::address::{Address, Receiver};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::{ShieldedPool, consensus::BlockHeight, value::Zatoshis};
use zcash_script::script;
use zip32::Scope;

use crate::components::{
    database::DbConnection,
    json_rpc::{
        server::LegacyCode,
        utils::{JsonZec, parse_as_of_height, parse_minconf, value_from_zatoshis},
    },
};

/// Response to a `z_listunspent` RPC request.
pub(crate) type Response = RpcResult<ResultType>;

/// A list of unspent notes.
#[derive(Clone, Debug, Serialize, Documented, JsonSchema)]
#[serde(transparent)]
pub(crate) struct ResultType(Vec<UnspentOutput>);

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct UnspentOutput {
    /// The ID of the transaction that created this output.
    txid: String,

    /// The shielded value pool.
    ///
    /// One of `["sapling", "orchard", "transparent"]`.
    pool: String,

    /// The Transparent UTXO, Sapling output or Orchard action index.
    outindex: u32,

    /// The number of confirmations.
    confirmations: u32,

    /// `true` if the account that received the output is watch-only
    is_watch_only: bool,

    /// The Zcash address that received the output.
    ///
    /// Omitted if this output was received on an account-internal address (for example, change
    /// and shielding outputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,

    /// The UUID of the wallet account that received this output.
    account_uuid: String,

    /// `true` if the output was received by the account's internal viewing key.
    ///
    /// The `address` field is guaranteed be absent when this field is set to `true`, in which case
    /// it indicates that this may be a change output, an output of a wallet-internal shielding
    /// transaction, an output of a wallet-internal cross-account transfer, or otherwise is the
    /// result of some wallet-internal operation.
    #[serde(rename = "walletInternal")]
    wallet_internal: bool,

    /// The value of the output in ZEC.
    value: JsonZec,

    /// The value of the output in zatoshis.
    #[serde(rename = "valueZat")]
    value_zat: u64,

    /// Hexadecimal string representation of the memo field.
    ///
    /// Omitted if this is a transparent output.
    #[serde(skip_serializing_if = "Option::is_none")]
    memo: Option<String>,

    /// UTF-8 string representation of memo field (if it contains valid UTF-8).
    #[serde(rename = "memoStr")]
    #[serde(skip_serializing_if = "Option::is_none")]
    memo_str: Option<String>,
}

pub(super) const PARAM_MINCONF_DESC: &str =
    "Only include outputs of transactions confirmed at least this many times.";
pub(super) const PARAM_MAXCONF_DESC: &str =
    "Only include outputs of transactions confirmed at most this many times.";
pub(super) const PARAM_INCLUDE_WATCHONLY_DESC: &str =
    "Also include outputs received at watch-only addresses.";
pub(super) const PARAM_ADDRESSES_DESC: &str =
    "If non-empty, only outputs received by the provided addresses will be returned.";
pub(super) const PARAM_AS_OF_HEIGHT_DESC: &str = "Execute the query as if it were run when the blockchain was at the height specified by this argument.";

// FIXME: the following parameters are not yet properly supported
// * include_watchonly
pub(crate) fn call(
    wallet: &DbConnection,
    minconf: Option<u32>,
    maxconf: Option<u32>,
    _include_watchonly: Option<bool>,
    addresses: Option<Vec<String>>,
    as_of_height: Option<i64>,
) -> Response {
    let as_of_height = parse_as_of_height(as_of_height)?;
    let minconf = parse_minconf(minconf, 1, as_of_height)?;

    let confirmations_policy = match NonZeroU32::new(minconf) {
        Some(c) => ConfirmationsPolicy::new_symmetrical(c, false),
        None => ConfirmationsPolicy::new_symmetrical(NonZeroU32::new(1).unwrap(), true),
    };

    //let include_watchonly = include_watchonly.unwrap_or(false);
    let addresses = addresses
        .unwrap_or_default()
        .iter()
        .map(|addr| {
            Address::decode(wallet.params(), addr).ok_or_else(|| {
                RpcError::owned(
                    LegacyCode::InvalidParameter.into(),
                    "Not a valid Zcash address",
                    Some(addr),
                )
            })
        })
        .collect::<Result<Vec<Address>, _>>()?;

    // The transparent receivers named by the address filter. `Address::Tex` re-encodes a
    // P2PKH receiver, and a unified address may carry a transparent receiver alongside
    // its shielded ones. Empty when no filter was provided (or the filter names no
    // transparent receivers, in which case no transparent output can match it).
    let transparent_filter: HashSet<TransparentAddress> = addresses
        .iter()
        .flat_map(|addr| match addr {
            Address::Transparent(t) => vec![*t],
            Address::Tex(data) => vec![TransparentAddress::PublicKeyHash(*data)],
            _ => addr
                .as_understood_unified_receivers()
                .into_iter()
                .filter_map(|r| match r {
                    Receiver::Transparent(t) => Some(t),
                    _ => None,
                })
                .collect(),
        })
        .collect();

    let target_height = match as_of_height.map_or_else(
        || {
            wallet.chain_height().map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::chain_height failed",
                    Some(format!("{e}")),
                )
            })
        },
        |h| Ok(Some(h)),
    )? {
        Some(h) => TargetHeight::from(h + 1),
        None => {
            return Ok(ResultType(vec![]));
        }
    };

    let mut unspent_outputs = vec![];

    for account_id in wallet.get_account_ids().map_err(|e| {
        RpcError::owned(
            LegacyCode::Database.into(),
            "WalletDb::get_account_ids failed",
            Some(format!("{e}")),
        )
    })? {
        let account = wallet
            .get_account(account_id)
            .map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::get_account failed",
                    Some(format!("{e}")),
                )
            })?
            // This would be a race condition between this and account deletion.
            .ok_or(RpcErrorCode::InternalError)?;

        let account_watch_only = !matches!(account.purpose(), AccountPurpose::Spending { .. });

        let receivers = wallet
            .get_transparent_receivers(account_id, true, true)
            .map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::get_transparent_receivers failed",
                    Some(format!("{e}")),
                )
            })?;

        // One batched query for all (matching) receivers: querying per receiver was
        // one SQLite round-trip per imported address, which is minutes of wall clock
        // on an exchange-scale wallet.
        let query_addrs: Vec<TransparentAddress> = receivers
            .keys()
            // When an address filter was provided, only its transparent receivers are
            // queried (a filter naming no transparent receivers matches no UTXOs).
            .filter(|addr| addresses.is_empty() || transparent_filter.contains(addr))
            .copied()
            .collect();
        let mut utxos = wallet
            .get_spendable_transparent_outputs_for_addresses(
                &query_addrs,
                target_height,
                confirmations_policy,
                CoinbaseFilter::AllTransparentOutputs,
            )
            .map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::get_spendable_transparent_outputs_for_addresses failed",
                    Some(format!("{e}")),
                )
            })?;

        // `get_spendable_transparent_outputs*` are coin-selection queries: they
        // exclude outputs at or below the ZIP 317 marginal fee, which cost more to
        // spend than they are worth. RPC enumeration must list them regardless, so
        // gather them with a single query that mirrors the spendability conditions
        // of `InputSource::get_unspent_transparent_output` (transaction mined or
        // definitely unexpired; output unspent by any mined-or-unexpired
        // transaction; the likely-spent wallet-internal ephemeral heuristic) with
        // the value floor inverted. Semantic parity with the per-outpoint check is
        // pinned by qa/regtest-harness (dust and hang-guard scenarios). Remove once
        // the upstream query exposes its minimum value as a parameter.
        //
        // An earlier version admitted candidates through the per-outpoint check one
        // query at a time, which cost ~30ms per dust output: a six-minute listing on
        // a wallet holding ~10k dust UTXOs.
        let filter_strings: Vec<String> = transparent_filter
            .iter()
            .map(|t| t.encode(wallet.params()))
            .collect();
        let account_uuid = account_id.expose_uuid();
        let marginal_fee = u64::from(zip317::MARGINAL_FEE);
        let target_height_u32 = u32::from(target_height);
        // A filter that names no transparent receiver can match no transparent
        // output, so skip the sweep rather than scanning every dust row in the
        // account only for the retain below to discard the lot.
        let sweep_dust = addresses.is_empty() || !transparent_filter.is_empty();
        type DustRow = ([u8; 32], u32, Vec<u8>, i64, uuid::Uuid, Option<u32>);
        let dust_rows: Vec<DustRow> = if !sweep_dust {
            vec![]
        } else {
            wallet
                .with_raw(|conn, _| {
                    // The filter binds as one carray pointer (`rarray`, loaded on every
                    // pooled connection) rather than one SQL variable per address, which
                    // would hit SQLITE_MAX_VARIABLE_NUMBER at exchange-scale filters.
                    let filter_values: std::rc::Rc<Vec<rusqlite::types::Value>> = std::rc::Rc::new(
                        filter_strings
                            .iter()
                            .cloned()
                            .map(rusqlite::types::Value::from)
                            .collect(),
                    );
                    let address_clause = if filter_strings.is_empty() {
                        ""
                    } else {
                        " AND ad.cached_transparent_receiver_address IN rarray(:addresses)"
                    };
                    // Clause bodies mirror zcash_client_sqlite's
                    // `get_wallet_transparent_output` (tx_unexpired_condition,
                    // spent_utxos_clause, excluding_wallet_internal_ephemeral_outputs);
                    // 40 below is DEFAULT_TX_EXPIRY_DELTA and 2 is KeyScope::Ephemeral's
                    // encoding.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT t.txid, u.output_index, u.script, u.value_zat,
                            a.uuid AS account_uuid, t.mined_height
                     FROM transparent_received_outputs u
                     JOIN transactions t ON t.id_tx = u.transaction_id
                     JOIN accounts a ON a.id = u.account_id
                     JOIN addresses ad ON ad.id = u.address_id
                     WHERE a.uuid = :account_uuid
                     AND u.value_zat <= :marginal_fee
                     AND (
                         t.mined_height < :target_height
                         OR t.expiry_height = 0
                         OR t.expiry_height >= :target_height
                         OR (
                             t.expiry_height IS NULL
                             AND t.min_observed_height + 40 >= :target_height
                         )
                     )
                     AND u.id NOT IN (
                         SELECT s.transparent_received_output_id
                         FROM transparent_received_output_spends s
                         JOIN transactions stx ON stx.id_tx = s.transaction_id
                         WHERE stx.mined_height < :target_height
                         OR stx.expiry_height = 0
                         OR stx.expiry_height >= :target_height
                         OR (
                             stx.expiry_height IS NULL
                             AND stx.min_observed_height + 40 >= :target_height
                         )
                     )
                     AND (
                         ad.key_scope != 2
                         OR t.id_tx NOT IN (
                             SELECT transaction_id
                             FROM v_received_output_spends
                             WHERE v_received_output_spends.account_id = a.id
                         )
                         OR u.max_observed_unspent_height > t.expiry_height
                     ){address_clause}"
                    ))?;
                    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
                        (":account_uuid", &account_uuid),
                        (":marginal_fee", &marginal_fee),
                        (":target_height", &target_height_u32),
                    ];
                    if !filter_strings.is_empty() {
                        params.push((":addresses", &filter_values));
                    }
                    let rows = stmt.query_map(&params[..], |row| {
                        let txid: [u8; 32] = row.get(0)?;
                        let n: u32 = row.get(1)?;
                        let script: Vec<u8> = row.get(2)?;
                        let value: i64 = row.get(3)?;
                        let uuid: uuid::Uuid = row.get(4)?;
                        let mined_height: Option<u32> = row.get(5)?;
                        Ok((txid, n, script, value, uuid, mined_height))
                    })?;
                    rows.collect::<Result<Vec<_>, _>>()
                })
                .map_err(|e| {
                    RpcError::owned(
                        LegacyCode::Database.into(),
                        "uneconomic transparent output enumeration failed",
                        Some(format!("{e}")),
                    )
                })?
        };
        for (txid, n, script, value, uuid, mined_height) in dust_rows {
            let outpoint = OutPoint::new(txid, n);
            // A row failing either conversion below is database corruption; surface
            // it as an error, matching the per-outpoint path this query mirrors
            // (`SqliteClientError::CorruptedData`), rather than silently omitting a
            // UTXO from the listing.
            let value = Zatoshis::from_nonnegative_i64(value).map_err(|_| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "corrupt wallet database",
                    Some(format!("invalid value for UTXO {}:{n}", outpoint.txid())),
                )
            })?;
            // Key scope and funding account are not read by this RPC's response
            // construction; the address metadata lookup below supplies the
            // wallet-internal flag.
            let utxo = WalletTransparentOutput::from_parts(
                outpoint,
                TxOut::new(value, Script(script::Code(script))),
                mined_height.map(BlockHeight::from),
                Some(AccountUuid::from_uuid(uuid)),
                None,
                None,
            )
            .ok_or_else(|| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "corrupt wallet database",
                    Some(format!(
                        "script of UTXO {}:{n} is not P2PKH or P2SH",
                        OutPoint::new(txid, n).txid()
                    )),
                )
            })?;
            // The batched queries apply `confirmations_policy`; the dust query does
            // not, so enforce the caller's minconf here.
            let confirmations = utxo.mined_height().map(|h| target_height - h).unwrap_or(0);
            if confirmations >= minconf {
                utxos.push(utxo);
            }
        }
        if !addresses.is_empty() {
            utxos.retain(|u| transparent_filter.contains(u.recipient_address()));
        }

        // Standalone addresses whose spending key is in the keystore: a zcashd
        // wallet migration stores legacy transparent keys there and registers the
        // pubkeys as standalone rows, so (unlike `z_importaddress` imports) those
        // addresses ARE spendable despite having no derivation scope. Key presence
        // is checked without decryption, so a locked wallet answers the same way.
        // (The keystore tables are part of the wallet database's unconditional
        // migration set, so this query is valid on wallets that never migrated.)
        let keyed_standalone: HashSet<TransparentAddress> = wallet
            .with_raw(|conn, _| {
                let mut stmt = conn.prepare(
                    "SELECT a.cached_transparent_receiver_address
                     FROM ext_zallet_keystore_standalone_transparent_keys ztk
                     JOIN addresses a ON ztk.pubkey = a.imported_transparent_receiver_pubkey
                     JOIN accounts acct ON acct.id = a.account_id
                     WHERE acct.uuid = :account_uuid",
                )?;
                let rows = stmt.query_map(
                    rusqlite::named_params! {":account_uuid": account_uuid},
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "keystore standalone-key enumeration failed",
                    Some(format!("{e}")),
                )
            })?
            .into_iter()
            .map(|s| {
                TransparentAddress::decode(wallet.params(), &s).map_err(|e| {
                    RpcError::owned(
                        LegacyCode::Database.into(),
                        "corrupt wallet database",
                        Some(format!("undecodable keystore address {s}: {e}")),
                    )
                })
            })
            .collect::<Result<_, _>>()?;

        for utxo in utxos {
            let confirmations = utxo.mined_height().map(|h| target_height - h).unwrap_or(0);

            let metadata = wallet
                .get_transparent_address_metadata(account_id, utxo.recipient_address())
                .map_err(|e| {
                    RpcError::owned(
                        LegacyCode::Database.into(),
                        "WalletDb::get_transparent_address_metadata failed",
                        Some(format!("{e}")),
                    )
                })?;

            let wallet_internal = metadata
                .as_ref()
                .is_some_and(|m| m.scope() == Some(TransparentKeyScope::INTERNAL));

            // The wallet holds no spending key for a standalone imported address
            // (`z_importaddress`/`z_importpubkey`), so its outputs are watch-only
            // even inside a spending account. A missing derivation scope identifies
            // standalone rows (every derived scope, external, internal, and
            // ephemeral TEX intermediates, is spendable with the account's key),
            // except that a migrated zcashd wallet holds real spending keys for
            // some standalone rows: those are carved back out via the keystore set.
            let is_watch_only = account_watch_only
                || (metadata.as_ref().and_then(|m| m.scope()).is_none()
                    && !keyed_standalone.contains(utxo.recipient_address()));

            unspent_outputs.push(UnspentOutput {
                txid: utxo.outpoint().txid().to_string(),
                pool: "transparent".into(),
                outindex: utxo.outpoint().n(),
                confirmations,
                is_watch_only,
                account_uuid: account_id.expose_uuid().to_string(),
                address: utxo
                    .txout()
                    .recipient_address()
                    .map(|addr| addr.encode(wallet.params())),
                value: value_from_zatoshis(utxo.value()),
                value_zat: u64::from(utxo.value()),
                memo: None,
                memo_str: None,
                wallet_internal,
            })
        }

        let notes = wallet
            .select_unspent_notes(
                account_id,
                &[ShieldedPool::Sapling, ShieldedPool::Orchard],
                target_height,
                &[],
            )
            .map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::select_unspent_notes failed",
                    Some(format!("{e}")),
                )
            })?;

        let get_memo = |txid, protocol, output_index| -> RpcResult<_> {
            Ok(wallet
                .get_memo(NoteId::new(txid, protocol, output_index))
                .map_err(|e| {
                    RpcError::owned(
                        LegacyCode::Database.into(),
                        "WalletDb::get_memo failed",
                        Some(format!("{e}")),
                    )
                })?
                .map(|memo| {
                    (
                        hex::encode(memo.encode().as_array()),
                        match memo {
                            zcash_protocol::memo::Memo::Text(text_memo) => Some(text_memo.into()),
                            _ => None,
                        },
                    )
                })
                .unwrap_or(("TODO: Always enhance every note".into(), None)))
        };

        let get_mined_height = |txid| {
            wallet.get_tx_height(txid).map_err(|e| {
                RpcError::owned(
                    LegacyCode::Database.into(),
                    "WalletDb::get_tx_height failed",
                    Some(format!("{e}")),
                )
            })
        };

        for note in notes.sapling().iter().filter(|n| {
            // An empty filter matches everything; otherwise a note need only match one
            // of the provided addresses (`all` would reject every note as soon as two
            // addresses were given).
            addresses.is_empty()
                || addresses
                    .iter()
                    .any(|addr| addr.to_sapling_address() == Some(n.note().recipient()))
        }) {
            let tx_mined_height = get_mined_height(*note.txid())?;
            let confirmations = tx_mined_height
                .map_or(0, |h| u32::from(target_height.saturating_sub(u32::from(h))));

            // skip notes that do not have sufficient confirmations according to minconf,
            // or that have too many confirmations according to maxconf
            if tx_mined_height
                .iter()
                .any(|h| *h > target_height.saturating_sub(minconf))
                || maxconf.iter().any(|c| confirmations > *c)
            {
                continue;
            }

            let is_internal = note.spending_key_scope() == Scope::Internal;

            let (memo, memo_str) =
                get_memo(*note.txid(), ShieldedPool::Sapling, note.output_index())?;

            unspent_outputs.push(UnspentOutput {
                txid: note.txid().to_string(),
                pool: "sapling".into(),
                outindex: note.output_index().into(),
                confirmations,
                is_watch_only: account_watch_only,
                account_uuid: account_id.expose_uuid().to_string(),
                // TODO: Ensure we generate the same kind of shielded address as `zcashd`.
                address: (!is_internal).then(|| note.note().recipient().encode(wallet.params())),
                value: value_from_zatoshis(note.value()),
                value_zat: u64::from(note.value()),
                memo: Some(memo),
                memo_str,
                wallet_internal: is_internal,
            })
        }

        for note in notes.orchard().iter().filter(|n| {
            // Same `any` semantics as the Sapling filter above.
            addresses.is_empty()
                || addresses.iter().any(|addr| {
                    addr.as_understood_unified_receivers()
                        .iter()
                        .any(|r| match r {
                            Receiver::Orchard(address) => address == &n.note().recipient(),
                            _ => false,
                        })
                })
        }) {
            let tx_mined_height = get_mined_height(*note.txid())?;
            let confirmations = tx_mined_height
                .map_or(0, |h| u32::from(target_height.saturating_sub(u32::from(h))));

            // skip notes that do not have sufficient confirmations according to minconf,
            // or that have too many confirmations according to maxconf
            if tx_mined_height
                .iter()
                .any(|h| *h > target_height.saturating_sub(minconf))
                || maxconf.iter().any(|c| confirmations > *c)
            {
                continue;
            }

            let wallet_internal = note.spending_key_scope() == Scope::Internal;

            let (memo, memo_str) =
                get_memo(*note.txid(), ShieldedPool::Orchard, note.output_index())?;

            unspent_outputs.push(UnspentOutput {
                txid: note.txid().to_string(),
                pool: "orchard".into(),
                outindex: note.output_index().into(),
                confirmations,
                is_watch_only: account_watch_only,
                account_uuid: account_id.expose_uuid().to_string(),
                // TODO: Ensure we generate the same kind of shielded address as `zcashd`.
                address: (!wallet_internal).then(|| {
                    UnifiedAddress::from_receivers(Some(note.note().recipient()), None, None)
                        .expect("valid")
                        .encode(wallet.params())
                }),
                value: value_from_zatoshis(note.value()),
                value_zat: u64::from(note.value()),
                memo: Some(memo),
                memo_str,
                wallet_internal,
            })
        }
    }

    Ok(ResultType(unspent_outputs))
}
