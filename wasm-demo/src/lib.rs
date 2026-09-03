//! [zero] @claude A browser wallet over the wasm build of librustzcash.
//!
//! Derives keys, keeps a real SQLite wallet database, **syncs from a lightwalletd over
//! gRPC-Web**, and **persists across reloads**. See README.md for how the last two work
//! and what they cost.
//!
//! ## State
//!
//! The wallet lives in a `thread_local` rather than a `#[wasm_bindgen]` struct. An async
//! sync has to hold the database across `await` points, and a `RefCell` borrow held across
//! an await is a panic waiting to happen — so `sync` takes the whole state out of the cell
//! for its duration and puts it back afterwards. A second concurrent call finds the cell
//! empty and is refused rather than corrupting anything. wasm is single-threaded, so this
//! is the whole of the concurrency story.

use std::cell::RefCell;

use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use rusqlite::Connection;
use secrecy::SecretVec;
use serde::Serialize;
use sqlite_wasm_rs::{MemVfsUtil, WasmOsCallback};
use wasm_bindgen::prelude::*;
use zcash_client_backend::{
    data_api::{
        chain::ChainState, wallet::ConfirmationsPolicy, Account as _, AccountBirthday, WalletRead,
        WalletWrite,
    },
    proto::service::compact_tx_streamer_client::CompactTxStreamerClient,
    sync,
};
use zcash_client_sqlite::{util::Clock, wallet::init::init_wallet_db, WalletDb};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Network};

mod block_cache;

use block_cache::MemoryBlockCache;

/// The name the wallet database has inside the in-memory VFS.
///
/// It must be a real name rather than `:memory:`: `MemVfsUtil` addresses files by name, and
/// that is how the database is exported for persistence.
const DB_NAME: &str = "wallet.db";

/// A [`Clock`] backed by JavaScript's `Date.now()`.
///
/// `SystemTime::now` panics on `wasm32-unknown-unknown`, so `util::SystemClock` cannot be
/// used. Every browser host needs a clock of this shape.
#[derive(Clone, Copy)]
struct JsClock;

impl Clock for JsClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(js_sys::Date::now() as u64)
    }
}

struct WalletState {
    conn: Connection,
    network: Network,
    cache: MemoryBlockCache,
}

impl WalletState {
    fn db(&mut self) -> WalletDb<&mut Connection, Network, JsClock, ChaChaRng> {
        WalletDb::from_connection(
            &mut self.conn,
            self.network,
            JsClock,
            ChaChaRng::seed_from_u64(0),
        )
    }
}

thread_local! {
    static STATE: RefCell<Option<WalletState>> = const { RefCell::new(None) };
}

fn err(context: &str, e: impl std::fmt::Display) -> JsError {
    JsError::new(&format!("{context}: {e}"))
}

/// Runs `f` against the open wallet.
fn with_wallet<T>(f: impl FnOnce(&mut WalletState) -> Result<T, JsError>) -> Result<T, JsError> {
    STATE.with(|s| match s.borrow_mut().as_mut() {
        Some(state) => f(state),
        None => Err(JsError::new(
            "no wallet is open — call createWallet first (a sync may be in progress)",
        )),
    })
}

fn parse_network(network: &str) -> Result<Network, JsError> {
    match network {
        "main" => Ok(Network::MainNetwork),
        "test" => Ok(Network::TestNetwork),
        other => Err(JsError::new(&format!("unknown network {other:?}"))),
    }
}

fn parse_seed(seed_hex: &str) -> Result<Vec<u8>, JsError> {
    let seed = hex::decode(seed_hex.trim()).map_err(|e| err("the seed is not valid hex", e))?;
    if seed.len() < 32 {
        return Err(JsError::new("the seed must be at least 32 bytes"));
    }
    Ok(seed)
}

/// Reports Rust panics to the browser console.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Sends `sync::run`'s progress reporting to the browser console.
///
/// The sync state machine reports what it is doing through `tracing`, and without a
/// subscriber that output goes nowhere — which makes a slow sync indistinguishable from a
/// hung one.
///
/// Capped at `INFO` deliberately. `tracing_wasm`'s default is unfiltered, and `shardtree`
/// emits `TRACE` records containing `Debug` renderings of whole subtrees; turning those on
/// does not merely add noise, it dominates the runtime of the thing being measured.
#[wasm_bindgen(js_name = enableLogging)]
pub fn enable_logging() {
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO)
            .build(),
    );
}

/// A per-account balance, in zatoshis.
#[derive(Serialize)]
pub struct AccountView {
    /// The account's UUID as the wallet database assigned it.
    pub uuid: String,
    /// The account's default Unified Address.
    pub address: String,
    /// Everything the wallet can see, spendable or not.
    pub total_zatoshis: u64,
    /// What could be spent right now under the default confirmations policy.
    pub spendable_zatoshis: u64,
}

/// The wallet as the page displays it.
#[derive(Serialize)]
pub struct Summary {
    /// The network the wallet is on.
    pub network: String,
    /// Every account, with balances.
    pub accounts: Vec<AccountView>,
    /// The wallet's birthday height, if it has one.
    pub wallet_birthday: Option<u32>,
    /// The chain tip as of the last sync.
    pub chain_height: Option<u32>,
    /// The height below which every block has been scanned.
    pub fully_scanned_height: Option<u32>,
    /// Migrations applied to the database.
    pub migrations: u32,
    /// Size of the database as it would be persisted, in bytes.
    pub db_bytes: usize,
}

fn summarize(state: &mut WalletState) -> Result<Summary, JsError> {
    let network = state.network;
    let migrations = state
        .conn
        .query_row("SELECT COUNT(*) FROM schemer_migrations", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|e| err("could not count migrations", e))? as u32;
    let db_bytes = export_bytes()?.len();

    let db = state.db();
    let wallet_birthday = db
        .get_wallet_birthday()
        .map_err(|e| err("could not read the wallet birthday", e))?;
    let chain_height = db
        .chain_height()
        .map_err(|e| err("could not read the chain height", e))?;
    let wallet_summary = db
        .get_wallet_summary(ConfirmationsPolicy::default())
        .map_err(|e| err("could not read the wallet summary", e))?;

    let mut accounts = Vec::new();
    for account_id in db
        .get_account_ids()
        .map_err(|e| err("could not list accounts", e))?
    {
        let account = db
            .get_account(account_id)
            .map_err(|e| err("could not read an account", e))?
            .ok_or_else(|| JsError::new("an account disappeared while being read"))?;
        let (address, _) = account
            .uivk()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| err("could not derive an address", format!("{e:?}")))?;
        let balance = wallet_summary
            .as_ref()
            .and_then(|s| s.account_balances().get(&account_id).copied());
        accounts.push(AccountView {
            uuid: account.id().expose_uuid().to_string(),
            address: address.encode(&network),
            total_zatoshis: balance.map_or(0, |b| b.total().into_u64()),
            spendable_zatoshis: balance.map_or(0, |b| b.spendable_value().into_u64()),
        });
    }

    Ok(Summary {
        network: match network {
            Network::MainNetwork => "main".to_owned(),
            Network::TestNetwork => "test".to_owned(),
        },
        accounts,
        wallet_birthday: wallet_birthday.map(u32::from),
        chain_height: chain_height.map(u32::from),
        fully_scanned_height: wallet_summary
            .as_ref()
            .map(|s| s.fully_scanned_height().into()),
        migrations,
        db_bytes,
    })
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsError::new(&e.to_string()))
}

/// Opens the wallet database, restoring `existing` if one was persisted.
///
/// `existing` is the byte image of a previously exported database, or an empty slice for a
/// fresh wallet. Migrations run either way: on a restored database they are a no-op, which
/// is also how a new release with new migrations upgrades an existing wallet.
#[wasm_bindgen(js_name = createWallet)]
pub fn create_wallet(network: &str, existing: &[u8]) -> Result<JsValue, JsError> {
    let network = parse_network(network)?;
    let vfs = MemVfsUtil::<WasmOsCallback>::new();

    // A previous wallet may still be open on this page; the VFS is global, so clear it.
    STATE.with(|s| *s.borrow_mut() = None);
    vfs.delete_db(DB_NAME);
    if !existing.is_empty() {
        vfs.import_db(DB_NAME, existing)
            .map_err(|e| err("the stored wallet database could not be imported", e))?;
    }

    let conn = Connection::open(DB_NAME).map_err(|e| err("could not open the database", e))?;
    // `WalletDb::from_connection` requires this, and only `for_path` does it for you —
    // and `for_path` needs a filesystem. Skipping it compiles, then fails at runtime
    // inside any query that binds a list parameter.
    rusqlite::vtab::array::load_module(&conn)
        .map_err(|e| err("could not load the rarray module", e))?;

    let mut state = WalletState {
        conn,
        network,
        cache: MemoryBlockCache::new(),
    };
    init_wallet_db(&mut state.db(), None)
        .map_err(|e| err("migrations failed", format!("{e:?}")))?;

    let summary = summarize(&mut state)?;
    STATE.with(|s| *s.borrow_mut() = Some(state));
    to_js(&summary)
}

/// Adds an account derived from `seed_hex`, scanning from `birthday_height`.
#[wasm_bindgen(js_name = addAccount)]
pub fn add_account(name: &str, seed_hex: &str, birthday_height: u32) -> Result<JsValue, JsError> {
    let seed = SecretVec::new(parse_seed(seed_hex)?);
    with_wallet(|state| {
        // A wallet that has not synced has no chain state, so this is an empty birthday at
        // the chosen height and the wallet scans forward from there. A wallet recovering
        // real funds takes the tree state at that height from the server instead, so that
        // notes received earlier in the block are witnessed correctly.
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(birthday_height), BlockHash([0; 32])),
            None,
        );
        state
            .db()
            .create_account(name, &seed, &birthday, None)
            .map_err(|e| err("could not create the account", e))?;
        summarize(state).and_then(|s| to_js(&s))
    })
}

/// Reads the wallet's current state back out of the database.
#[wasm_bindgen]
pub fn summary() -> Result<JsValue, JsError> {
    with_wallet(|state| summarize(state).and_then(|s| to_js(&s)))
}

fn export_bytes() -> Result<Vec<u8>, JsError> {
    MemVfsUtil::<WasmOsCallback>::new()
        .export_db(DB_NAME)
        .map_err(|e| err("could not export the wallet database", e))
}

/// The wallet database as a byte image, for the page to persist.
///
/// This is the whole persistence mechanism. `sqlite-wasm-rs` 0.5 ships only the in-memory
/// VFS — the OPFS and IndexedDB backends its README describes are not in the version
/// `rusqlite` depends on — so rather than SQLite writing through to storage, the page
/// takes the finished file and stores it itself. That is sound for a wallet database of
/// this size and has one real consequence: a write is only durable once the page has
/// stored the export, so export after anything worth keeping.
#[wasm_bindgen(js_name = exportDb)]
pub fn export_db() -> Result<Vec<u8>, JsError> {
    export_bytes()
}

/// Asks a lightwalletd for its current chain tip.
///
/// Cheap, and the quickest check that a gRPC-Web endpoint is reachable and speaking the
/// right protocol before committing to a sync.
#[wasm_bindgen(js_name = fetchTipHeight)]
pub async fn fetch_tip_height(url: String) -> Result<u32, JsError> {
    let mut client = CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(url));
    let tip = client
        .get_latest_block(zcash_client_backend::proto::service::ChainSpec::default())
        .await
        .map_err(|e| err("the lightwalletd request failed", e))?;
    Ok(tip.into_inner().height as u32)
}

/// Streams a range of compact blocks and reports how many arrived.
///
/// `sync::run` is built on server-streaming calls, and streaming is the part of gRPC-Web
/// most likely to behave differently from plain gRPC. This isolates it.
#[wasm_bindgen(js_name = probeBlockStream)]
pub async fn probe_block_stream(url: String, from: u32, count: u32) -> Result<u32, JsError> {
    use futures_util::StreamExt;
    use zcash_client_backend::proto::service::{BlockId, BlockRange};

    let mut client = CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(url));
    let range = BlockRange {
        start: Some(BlockId {
            height: u64::from(from),
            hash: vec![],
        }),
        end: Some(BlockId {
            height: u64::from(from + count - 1),
            hash: vec![],
        }),
        ..Default::default()
    };
    let mut stream = client
        .get_block_range(range)
        .await
        .map_err(|e| err("the block range request failed", e))?
        .into_inner();

    let mut seen = 0u32;
    while let Some(block) = stream.next().await {
        block.map_err(|e| err("the block stream failed", e))?;
        seen += 1;
    }
    Ok(seen)
}

/// Syncs the wallet against a lightwalletd, over gRPC-Web.
///
/// `url` must be an endpoint speaking gRPC-Web — plain gRPC over HTTP/2 will not answer a
/// browser. The whole state machine is `zcash_client_backend::sync::run`; this function
/// supplies the transport, a block cache, and the database.
#[wasm_bindgen(js_name = syncNow)]
pub async fn sync_now(url: String, batch_size: u32) -> Result<JsValue, JsError> {
    // Take the wallet out for the duration: a `RefCell` borrow cannot be held across an
    // await, and a second concurrent sync must be refused rather than allowed to
    // interleave writes.
    let mut state = STATE
        .with(|s| s.borrow_mut().take())
        .ok_or_else(|| JsError::new("no wallet is open, or a sync is already running"))?;

    let mut client = CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(url));
    let cache = state.cache.clone();
    let network = state.network;

    let result = sync::run(&mut client, &network, &cache, &mut state.db(), batch_size).await;

    // Scanned blocks are dead weight and are the bulk of a sync's memory use.
    cache.clear();

    let outcome = result
        .map_err(|e| err("sync failed", format!("{e:?}")))
        .and_then(|()| summarize(&mut state));

    STATE.with(|s| *s.borrow_mut() = Some(state));
    to_js(&outcome?)
}

/// Derives a Unified Address without touching a database.
#[wasm_bindgen(js_name = deriveAddress)]
pub fn derive_address(
    network: &str,
    seed_hex: &str,
    account_index: u32,
) -> Result<String, JsError> {
    let network = parse_network(network)?;
    let seed = parse_seed(seed_hex)?;
    let account_index = zip32::AccountId::try_from(account_index)
        .map_err(|_| JsError::new("the account index is out of range"))?;
    let usk = UnifiedSpendingKey::from_seed(&network, &seed, account_index)
        .map_err(|e| err("could not derive the spending key", format!("{e:?}")))?;
    let (address, _) = usk
        .to_unified_full_viewing_key()
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .map_err(|e| err("could not derive an address", format!("{e:?}")))?;
    Ok(address.encode(&network))
}

/// Build-time facts for the page to display.
#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info() -> Result<JsValue, JsError> {
    #[derive(Serialize)]
    struct BuildInfo {
        sqlite: String,
        storage: &'static str,
    }
    to_js(&BuildInfo {
        sqlite: rusqlite::version().to_owned(),
        storage: "in-memory VFS, exported to IndexedDB by the page",
    })
}
