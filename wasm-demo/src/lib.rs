//! [zero] @claude A browser wallet demo over the wasm build of librustzcash.
//!
//! This is deliberately the honest subset: everything here runs today, in a browser, with
//! no server and no network. What it does *not* do is sync — that needs a lightwalletd or
//! Zaino speaking gRPC-Web, which is an infrastructure decision rather than a code one.
//! See `librustzcash/WASM.md`.
//!
//! ## Where the database lives
//!
//! In memory, for the lifetime of the page. `rusqlite` 0.39 links `sqlite-wasm-rs` on this
//! target, and as published that crate ships only the in-memory VFS — the OPFS and
//! IndexedDB backends its README describes were split out after 0.4 and are not reachable
//! from the version `rusqlite` depends on. A wallet that survives a reload needs that gap
//! closed first; the demo says so on screen rather than pretending otherwise.

use std::num::NonZeroU32;

use rand_chacha::{ChaChaRng, rand_core::SeedableRng};
use rusqlite::Connection;
use secrecy::SecretVec;
use serde::Serialize;
use wasm_bindgen::prelude::*;
use zcash_client_backend::data_api::{
    Account as _, AccountBirthday, WalletRead, WalletWrite, chain::ChainState,
};
use zcash_client_sqlite::{WalletDb, util::Clock, wallet::init::init_wallet_db};
use zcash_keys::keys::UnifiedAddressRequest;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Network};

/// A [`Clock`] backed by JavaScript's `Date.now()`.
///
/// `SystemTime::now` panics on `wasm32-unknown-unknown`, so the wallet's own `SystemClock`
/// cannot be used. Every browser host needs a clock of this shape.
#[derive(Clone, Copy)]
struct JsClock;

impl Clock for JsClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(js_sys::Date::now() as u64)
    }
}

/// Installs a panic hook that reports Rust panics to the browser console.
///
/// Without this a panic is an unhelpful `unreachable executed` in the JS console, which is
/// a bad first experience of a wasm build.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// What the page shows after an account is created.
#[derive(Serialize)]
pub struct AccountSummary {
    /// The account's UUID, as the wallet database assigned it.
    pub account_uuid: String,
    /// The account's default Unified Address.
    pub unified_address: String,
    /// The receiver types the Unified Address carries.
    pub receivers: Vec<String>,
    /// Total balance in zatoshis. Zero until the wallet syncs.
    pub total_zatoshis: u64,
    /// The birthday height the account was created with.
    pub birthday: u32,
}

/// What the page shows about the wallet as a whole.
#[derive(Serialize)]
pub struct WalletSummary {
    /// Number of accounts in the wallet.
    pub accounts: usize,
    /// The wallet's birthday height, if it has one.
    pub wallet_birthday: Option<u32>,
    /// The scanned chain tip, if any. `None` until the wallet syncs.
    pub chain_height: Option<u32>,
    /// The network the wallet is on.
    pub network: String,
}

/// A wallet database living in the browser.
///
/// The connection is held here rather than inside a long-lived `WalletDb`, and a `WalletDb`
/// is borrowed from it per operation. That is the shape `WalletDb::from_connection`
/// documents for connection pooling, and it leaves the raw connection reachable for the
/// schema query below — `WalletDb` deliberately does not expose it.
#[wasm_bindgen]
pub struct Wallet {
    conn: Connection,
    network: Network,
}

impl Wallet {
    fn db(&mut self) -> WalletDb<&mut Connection, Network, JsClock, ChaChaRng> {
        WalletDb::from_connection(
            &mut self.conn,
            self.network,
            JsClock,
            ChaChaRng::seed_from_u64(0),
        )
    }
}

#[wasm_bindgen]
impl Wallet {
    /// Creates a wallet database and applies every migration.
    ///
    /// `network` is `"main"` or `"test"`.
    #[wasm_bindgen(constructor)]
    pub fn new(network: &str) -> Result<Wallet, JsError> {
        let network = match network {
            "main" => Network::MainNetwork,
            "test" => Network::TestNetwork,
            other => return Err(JsError::new(&format!("unknown network {other:?}"))),
        };

        let conn = Connection::open_in_memory()
            .map_err(|e| JsError::new(&format!("could not open the database: {e}")))?;
        // `WalletDb::from_connection` requires this; only `for_path` does it for you, and
        // `for_path` needs a filesystem. Without it, any query binding a list parameter
        // fails at runtime rather than here.
        rusqlite::vtab::array::load_module(&conn)
            .map_err(|e| JsError::new(&format!("could not load the rarray module: {e}")))?;

        let mut wallet = Wallet { conn, network };
        init_wallet_db(&mut wallet.db(), None)
            .map_err(|e| JsError::new(&format!("migrations failed: {e:?}")))?;

        Ok(wallet)
    }

    /// Adds an account derived from `seed_hex`, with the given birthday height.
    ///
    /// The seed must be at least 32 bytes, hex-encoded.
    #[wasm_bindgen(js_name = createAccount)]
    pub fn create_account(
        &mut self,
        name: &str,
        seed_hex: &str,
        birthday_height: u32,
    ) -> Result<JsValue, JsError> {
        let seed = hex::decode(seed_hex.trim())
            .map_err(|e| JsError::new(&format!("the seed is not valid hex: {e}")))?;
        if seed.len() < 32 {
            return Err(JsError::new("the seed must be at least 32 bytes"));
        }
        let seed = SecretVec::new(seed);

        // A wallet that has not synced has no chain state, so the birthday is an empty
        // one at the chosen height: the wallet will scan forward from there. A real wallet
        // takes this from the server's tree state at that height.
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(birthday_height), BlockHash([0; 32])),
            None,
        );

        let mut db = self.db();
        let (account_id, usk) = db
            .create_account(name, &seed, &birthday, None)
            .map_err(|e| JsError::new(&format!("could not create the account: {e}")))?;

        let (address, _) = usk
            .to_unified_full_viewing_key()
            .default_address(UnifiedAddressRequest::AllAvailableKeys)
            .map_err(|e| JsError::new(&format!("could not derive an address: {e:?}")))?;

        let mut receivers = Vec::new();
        if address.orchard().is_some() {
            receivers.push("Orchard".to_owned());
        }
        if address.sapling().is_some() {
            receivers.push("Sapling".to_owned());
        }
        if address.transparent().is_some() {
            receivers.push("P2PKH".to_owned());
        }

        let account = db
            .get_account(account_id)
            .map_err(|e| JsError::new(&format!("could not read the account back: {e}")))?
            .ok_or_else(|| JsError::new("the account vanished after being created"))?;

        let summary = AccountSummary {
            account_uuid: account.id().expose_uuid().to_string(),
            unified_address: address.encode(&self.network),
            receivers,
            total_zatoshis: 0,
            birthday: birthday_height,
        };
        serde_wasm_bindgen::to_value(&summary).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Reads the wallet's current state back out of the database.
    #[wasm_bindgen]
    pub fn summary(&mut self) -> Result<JsValue, JsError> {
        let network = self.network;
        let db = self.db();
        let accounts = db
            .get_account_ids()
            .map_err(|e| JsError::new(&format!("could not list accounts: {e}")))?;
        let wallet_birthday = db
            .get_wallet_birthday()
            .map_err(|e| JsError::new(&format!("could not read the wallet birthday: {e}")))?;
        let chain_height = db
            .chain_height()
            .map_err(|e| JsError::new(&format!("could not read the chain height: {e}")))?;

        let summary = WalletSummary {
            accounts: accounts.len(),
            wallet_birthday: wallet_birthday.map(u32::from),
            chain_height: chain_height.map(u32::from),
            network: match network {
                Network::MainNetwork => "main".to_owned(),
                Network::TestNetwork => "test".to_owned(),
            },
        };
        serde_wasm_bindgen::to_value(&summary).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Counts the migrations the database has applied.
    ///
    /// Proof that the schema is real rather than an empty file: this is the same count a
    /// desktop wallet would report.
    #[wasm_bindgen(js_name = migrationCount)]
    pub fn migration_count(&self) -> Result<u32, JsError> {
        // The migrations table is `schemerz`'s, and is the only place the applied set is
        // recorded.
        self.conn
            .query_row("SELECT COUNT(*) FROM schemer_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|applied: i64| applied as u32)
            .map_err(|e| JsError::new(&format!("could not count migrations: {e}")))
    }
}

/// Derives a Unified Address without touching a database.
///
/// The cheapest thing the wasm build can do, and the one a wallet does before it has any
/// state: key derivation is pure computation.
#[wasm_bindgen(js_name = deriveAddress)]
pub fn derive_address(
    network: &str,
    seed_hex: &str,
    account_index: u32,
) -> Result<String, JsError> {
    let network = match network {
        "main" => Network::MainNetwork,
        "test" => Network::TestNetwork,
        other => return Err(JsError::new(&format!("unknown network {other:?}"))),
    };
    let seed = hex::decode(seed_hex.trim())
        .map_err(|e| JsError::new(&format!("the seed is not valid hex: {e}")))?;
    if seed.len() < 32 {
        return Err(JsError::new("the seed must be at least 32 bytes"));
    }
    let account_index = zip32::AccountId::try_from(account_index)
        .map_err(|_| JsError::new("the account index is out of range"))?;

    let usk = zcash_keys::keys::UnifiedSpendingKey::from_seed(&network, &seed, account_index)
        .map_err(|e| JsError::new(&format!("could not derive the spending key: {e:?}")))?;
    let (address, _) = usk
        .to_unified_full_viewing_key()
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .map_err(|e| JsError::new(&format!("could not derive an address: {e:?}")))?;
    Ok(address.encode(&network))
}

/// The demo's build-time facts, for the page to display.
#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info() -> Result<JsValue, JsError> {
    #[derive(Serialize)]
    struct BuildInfo {
        sqlite: String,
        persistent: bool,
        note: &'static str,
    }
    let _ = NonZeroU32::new(1);
    serde_wasm_bindgen::to_value(&BuildInfo {
        sqlite: rusqlite::version().to_owned(),
        persistent: false,
        note: "in-memory VFS: the wallet exists for the lifetime of this page",
    })
    .map_err(|e| JsError::new(&e.to_string()))
}
