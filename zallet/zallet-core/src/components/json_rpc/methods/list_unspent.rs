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
        wallet::{ConfirmationsPolicy, TargetHeight, input_selection::LockFilter},
    },
    encoding::AddressCodec,
    fees::{orchard::InputView as _, sapling::InputView as _},
    wallet::{NoteId, WalletTransparentOutput},
};
use zcash_client_sqlite::AccountUuid;
use zcash_keys::address::{Address, Receiver};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::{
    ShieldedPool,
    consensus::{BlockHeight, COINBASE_MATURITY_BLOCKS},
    value::Zatoshis,
};
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
    /// One of `["sapling", "orchard", "ironwood", "transparent"]`.
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

    /// `true` if the output was produced by a coinbase transaction.
    ///
    /// Omitted if this is a shielded output.
    #[serde(skip_serializing_if = "Option::is_none")]
    generated: Option<bool>,

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

/// The number of confirmations that an output of a transaction mined at `mined_height` has as
/// of `target_height`.
///
/// An output of a transaction that is not mined in the main chain has zero confirmations. Such
/// an output can be reported by this RPC when its transaction is in the mempool, or when a
/// transaction that had been mined has been un-mined by a reorg and its containing block has
/// not yet been re-scanned.
fn confirmation_count(target_height: TargetHeight, mined_height: Option<BlockHeight>) -> u32 {
    // Subtraction of block heights saturates at zero, which correctly reports a transaction
    // mined at or above the target height (possible when `asOfHeight` places the target below
    // the chain tip) as having no confirmations as of that height.
    mined_height.map_or(0, |h| target_height - h)
}

/// Whether an output with the given number of confirmations is within the range of
/// confirmations requested for this query.
///
/// Both bounds are inclusive: an output with exactly `minconf` (or exactly `maxconf`)
/// confirmations is reported. Because an unmined transaction's outputs have zero
/// confirmations, they are in range only for `minconf = 0`, which this RPC permits whenever
/// `asOfHeight` is absent.
fn confirmations_in_range(confirmations: u32, minconf: u32, maxconf: Option<u32>) -> bool {
    confirmations >= minconf && maxconf.is_none_or(|c| confirmations <= c)
}

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

    // zcashd parity: an inverted confirmation window is a parameter error, not an
    // empty result.
    if maxconf.is_some_and(|c| c < minconf) {
        return Err(RpcError::owned(
            LegacyCode::InvalidParameter.into(),
            "Maximum number of confirmations must be greater or equal to the minimum number of confirmations",
            None::<String>,
        ));
    }

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
        let account_uuid = account_id.expose_uuid();

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

        // Query non-coinbase and coinbase outputs separately so that each UTXO can be
        // tagged with its coinbase origin. The two filters partition the full set of
        // spendable outputs: outputs with an unknown transaction index are treated as
        // non-coinbase by `NonCoinbaseOnly` and excluded by `CoinbaseOnly`, so nothing
        // is dropped or duplicated.
        let mut utxos: Vec<(WalletTransparentOutput<AccountUuid>, bool)> = vec![];
        for (coinbase_filter, generated) in [
            (CoinbaseFilter::NonCoinbaseOnly, false),
            (CoinbaseFilter::CoinbaseOnly, true),
        ] {
            let outputs = wallet
                .get_spendable_transparent_outputs_for_addresses(
                    &query_addrs,
                    target_height,
                    confirmations_policy,
                    coinbase_filter,
                    // A locked output is still an unspent output belonging to the
                    // wallet, and this RPC reports the wallet's holdings rather than
                    // selecting inputs, so lock state is not a filter here.
                    LockFilter::Unfiltered,
                )
                .map_err(|e| {
                    RpcError::owned(
                        LegacyCode::Database.into(),
                        "WalletDb::get_spendable_transparent_outputs_for_addresses failed",
                        Some(format!("{e}")),
                    )
                })?;

            utxos.extend(outputs.into_iter().map(|utxo| (utxo, generated)));
        }

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
        let marginal_fee = u64::from(zip317::MARGINAL_FEE);
        let target_height_u32 = u32::from(target_height);
        // A filter that names no transparent receiver can match no transparent
        // output, so skip the sweep rather than scanning every dust row in the
        // account only for the retain below to discard the lot.
        let sweep_dust = addresses.is_empty() || !transparent_filter.is_empty();
        type DustRow = ([u8; 32], u32, Vec<u8>, i64, uuid::Uuid, Option<u32>, bool);
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
                    // encoding. `is_coinbase` uses the same `IFNULL(t.tx_index, 1) == 0`
                    // encoding as the coinbase filter in those queries, so a dust output
                    // whose transaction index is unknown is reported as non-coinbase.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT t.txid, u.output_index, u.script, u.value_zat,
                            a.uuid AS account_uuid, t.mined_height,
                            (IFNULL(t.tx_index, 1) == 0) AS is_coinbase
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
                        let is_coinbase: bool = row.get(6)?;
                        Ok((txid, n, script, value, uuid, mined_height, is_coinbase))
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
        for (txid, n, script, value, uuid, mined_height, is_coinbase) in dust_rows {
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
            // not, so enforce the caller's minconf here. An immature coinbase output
            // is not spendable and is excluded by the batched queries, so exclude it
            // here too rather than reporting dust the other path would hide.
            let confirmations = confirmation_count(target_height, utxo.mined_height());
            if confirmations >= minconf && !(is_coinbase && confirmations < COINBASE_MATURITY_BLOCKS) {
                utxos.push((utxo, is_coinbase));
            }
        }
        if !addresses.is_empty() {
            utxos.retain(|(u, _)| transparent_filter.contains(u.recipient_address()));
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

        for (utxo, generated) in utxos {
            let confirmations = confirmation_count(target_height, utxo.mined_height());

            // `get_spendable_transparent_outputs*` applies `minconf` itself, but not
            // `maxconf`; both bounds are checked here so that every pool reports the
            // same range.
            if !confirmations_in_range(confirmations, minconf, maxconf) {
                continue;
            }

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

            unspent_outputs.push(transparent_unspent_output(
                utxo.outpoint().txid().to_string(),
                utxo.outpoint().n(),
                confirmations,
                is_watch_only,
                utxo.txout()
                    .recipient_address()
                    .map(|addr| addr.encode(wallet.params())),
                account_id.expose_uuid().to_string(),
                wallet_internal,
                utxo.value(),
                generated,
            ))
        }

        let notes = wallet
            .select_unspent_notes(
                account_id,
                &[
                    ShieldedPool::Sapling,
                    ShieldedPool::Orchard,
                    ShieldedPool::Ironwood,
                ],
                target_height,
                &[],
                LockFilter::Unfiltered,
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
            let confirmations = confirmation_count(target_height, get_mined_height(*note.txid())?);

            // Skip notes that do not have sufficient confirmations according to `minconf`, or
            // that have too many confirmations according to `maxconf`.
            if !confirmations_in_range(confirmations, minconf, maxconf) {
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
                generated: None,
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
            let confirmations = confirmation_count(target_height, get_mined_height(*note.txid())?);

            // Skip notes that do not have sufficient confirmations according to `minconf`, or
            // that have too many confirmations according to `maxconf`.
            if !confirmations_in_range(confirmations, minconf, maxconf) {
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
                generated: None,
            })
        }

        // Ironwood notes are Orchard-shaped (their recipient is an Orchard
        // address and they live behind an Orchard receiver), so this mirrors
        // the Orchard block above; only the reported pool and the memo lookup
        // protocol differ.
        for note in notes.ironwood().iter().filter(|n| {
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
            let confirmations = confirmation_count(target_height, get_mined_height(*note.txid())?);

            // Skip notes that do not have sufficient confirmations according to `minconf`, or
            // that have too many confirmations according to `maxconf`.
            if !confirmations_in_range(confirmations, minconf, maxconf) {
                continue;
            }

            let wallet_internal = note.spending_key_scope() == Scope::Internal;

            let (memo, memo_str) =
                get_memo(*note.txid(), ShieldedPool::Ironwood, note.output_index())?;

            unspent_outputs.push(UnspentOutput {
                txid: note.txid().to_string(),
                pool: "ironwood".into(),
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
                generated: None,
            })
        }
    }

    Ok(ResultType(unspent_outputs))
}

/// Builds the `z_listunspent` entry for a transparent UTXO.
///
/// Transparent outputs always report their coinbase origin via the `generated` field,
/// have no memo, and belong to the `transparent` pool. This is a pure function over
/// values already extracted from the wallet, so that its JSON rendering can be
/// unit-tested without a database.
#[allow(clippy::too_many_arguments)]
fn transparent_unspent_output(
    txid: String,
    outindex: u32,
    confirmations: u32,
    is_watch_only: bool,
    address: Option<String>,
    account_uuid: String,
    wallet_internal: bool,
    value: Zatoshis,
    generated: bool,
) -> UnspentOutput {
    UnspentOutput {
        txid,
        pool: "transparent".into(),
        outindex,
        confirmations,
        is_watch_only,
        address,
        account_uuid,
        wallet_internal,
        generated: Some(generated),
        value: value_from_zatoshis(value),
        value_zat: u64::from(value),
        memo: None,
        memo_str: None,
    }
}

#[cfg(test)]
mod tests {
    use zcash_client_backend::data_api::wallet::TargetHeight;
    use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};

    use super::{
        UnspentOutput, confirmation_count, confirmations_in_range, transparent_unspent_output,
    };
    use crate::components::json_rpc::utils::value_from_zatoshis;

    /// The height of the next block to be mined, as used by the RPC. An output mined in block
    /// 99 therefore has one confirmation, and one mined in block 90 has ten.
    const TARGET: u32 = 100;

    fn target() -> TargetHeight {
        TargetHeight::from(BlockHeight::from_u32(TARGET))
    }

    fn mined_in(height: u32) -> Option<BlockHeight> {
        Some(BlockHeight::from_u32(height))
    }

    #[test]
    fn unmined_transaction_has_zero_confirmations() {
        assert_eq!(confirmation_count(target(), None), 0);
    }

    #[test]
    fn confirmations_count_the_mining_block() {
        assert_eq!(confirmation_count(target(), mined_in(TARGET - 1)), 1);
        assert_eq!(confirmation_count(target(), mined_in(TARGET - 10)), 10);
    }

    // An `asOfHeight` in the past places the target height below the chain tip, so a
    // transaction known to the wallet may be mined at or above it. Such a transaction has no
    // confirmations as of the target height, rather than a negative or wrapped count.
    #[test]
    fn transaction_mined_at_or_above_target_has_zero_confirmations() {
        assert_eq!(confirmation_count(target(), mined_in(TARGET)), 0);
        assert_eq!(confirmation_count(target(), mined_in(TARGET + 5)), 0);
    }

    // Regression: an output of an unmined transaction has zero confirmations, and so must be
    // excluded whenever at least one confirmation is required. This previously tested the
    // mined height with `Option::iter().any(..)`, which is vacuously false for an unmined
    // transaction, so such outputs were reported at every `minconf`.
    #[test]
    fn unmined_output_is_excluded_at_minconf_1() {
        assert!(!confirmations_in_range(
            confirmation_count(target(), None),
            1,
            None
        ));
    }

    // ... but `minconf = 0` is permitted when `asOfHeight` is absent, and admits exactly those
    // zero-confirmation outputs.
    #[test]
    fn unmined_output_is_included_at_minconf_0() {
        assert!(confirmations_in_range(
            confirmation_count(target(), None),
            0,
            None
        ));
    }

    #[test]
    fn minconf_bound_is_inclusive() {
        let confirmations = confirmation_count(target(), mined_in(TARGET - 10));
        assert!(!confirmations_in_range(confirmations, 11, None));
        assert!(confirmations_in_range(confirmations, 10, None));
        assert!(confirmations_in_range(confirmations, 9, None));
    }

    // Regression: `maxconf` was applied only to shielded notes, so transparent outputs with
    // more than `maxconf` confirmations were reported regardless. Both bounds now come from
    // this single predicate, which every pool consults.
    #[test]
    fn maxconf_bound_is_inclusive() {
        let confirmations = confirmation_count(target(), mined_in(TARGET - 10));
        assert!(!confirmations_in_range(confirmations, 1, Some(9)));
        assert!(confirmations_in_range(confirmations, 1, Some(10)));
        assert!(confirmations_in_range(confirmations, 1, Some(11)));
    }

    #[test]
    fn absent_maxconf_imposes_no_upper_bound() {
        assert!(confirmations_in_range(
            confirmation_count(target(), mined_in(0)),
            1,
            None
        ));
    }

    /// Renders a transparent UTXO entry with the given coinbase origin to its JSON
    /// representation (the actual RPC output contract).
    fn rendered_transparent(generated: bool) -> serde_json::Value {
        serde_json::to_value(transparent_unspent_output(
            "3ec4c1b4b1e61a13c11ec5b0ba1240cca66f0e0d5b1e0303403d0a44ae7d0219".into(),
            0,
            10,
            false,
            Some("t1UYsZVJkLPeMjxEtACvSxfWuNmddpWfxzs".into()),
            "3ad46f88-8f11-407b-b768-a2d587e971c9".into(),
            false,
            Zatoshis::const_from_u64(625_000_000),
            generated,
        ))
        .unwrap()
    }

    #[test]
    fn transparent_coinbase_output_is_generated() {
        let rendered = rendered_transparent(true);
        assert_eq!(rendered["generated"], serde_json::json!(true));
        assert_eq!(rendered["pool"], serde_json::json!("transparent"));
    }

    #[test]
    fn transparent_non_coinbase_output_is_not_generated() {
        let rendered = rendered_transparent(false);
        assert_eq!(rendered["generated"], serde_json::json!(false));
    }

    #[test]
    fn shielded_output_omits_generated() {
        // Shielded notes never set `generated`; the field must be omitted entirely
        // rather than rendered as `null`.
        let output = UnspentOutput {
            txid: "3ec4c1b4b1e61a13c11ec5b0ba1240cca66f0e0d5b1e0303403d0a44ae7d0219".into(),
            pool: "sapling".into(),
            outindex: 0,
            confirmations: 10,
            is_watch_only: false,
            address: None,
            account_uuid: "3ad46f88-8f11-407b-b768-a2d587e971c9".into(),
            wallet_internal: true,
            generated: None,
            value: value_from_zatoshis(Zatoshis::const_from_u64(100_000)),
            value_zat: 100_000,
            memo: Some("f600".into()),
            memo_str: None,
        };

        let rendered = serde_json::to_value(output).unwrap();
        assert!(rendered.get("generated").is_none());
    }
}
