# zebra-differential — tx-level zcashd↔Zebra consensus parity harness

Feeds raw transactions built by **zcashd** through the **vendored Zebra's own
`zebra_consensus::transaction::Verifier`** — full parse, version/upgrade gating
at the real mined height, and every Groth16/Halo2 proof and
RedJubjub/RedPallas signature verified under Zebra's independent sighash
implementation. Any bit of divergence in the v6 sighash, ZIP 229
serialization, flag grammar, or circuit-version selection between the two
implementations fails signature/proof verification here.

First used 2026-07-17 to validate the H-P2-1 wallet fix and the S4
`z_shieldtoironwood` output against Zebra (all PARITY; see
`ironwood_mainnet_review.md` at the repo parent for the run record).

## Why transaction-level (and not a P2P chain sync)

A full-chain regtest sync between the two nodes is **impossible below height
299,188 by construction**: zcashd regtest never retargets
(`fPowNoRetargeting`, every block carries the powLimit compact `0x200f0f0f`)
while Zebra applies spec DigiShield on regtest, whose floor-division loss
yields `0x200f0f0e` even at perfect 75-second spacing — and the ZIP 208
testnet minimum-difficulty override that would reconcile them only activates
at `TESTNET_MINIMUM_DIFFICULTY_START_HEIGHT` (299,188). This is a
regtest-only ecosystem gap, not a mainnet risk (both implement identical
DigiShield on mainnet); it makes the transaction level the right place for a
local differential.

## Usage

1. Build a regtest chain on zcashd (any `-nuparams` schedule) containing the
   transactions to check, e.g. via `z_shieldcoinbase` / `z_sendmany` /
   `z_shieldtoironwood`.
2. Extract the cases into JSON — one object per named case:

   ```json
   {
     "caseName": {
       "txid": "…", "hex": "…raw tx hex…",
       "height": 212, "time": 1296704427, "version": 6,
       "prevouts": [
         { "txid": "…", "vout": 0, "value_zat": 312497400,
           "script": "…scriptPubKey hex…", "creating_height": 5,
           "is_coinbase": true }
       ]
     }
   }
   ```

   `prevouts` lists the transparent inputs' funding outputs (empty for pure
   shielded transactions); on a wallet without `-txindex` use
   `gettransaction` + `decoderawtransaction` to gather them. The current
   `main.rs` expects the four case names `shieldcoinbase` / `classA` /
   `classB` / `ironwood` and runs a pre-activation negative control on
   `classA`; adjust `order` in `main()` for other case sets.

3. Adjust `regtest_network()` if your `-nuparams` schedule differs from
   "everything at 1, NU6.3 at 210".

4. Build and run (a shared `CARGO_TARGET_DIR` with other Zebra builds saves
   most of the compile):

   ```sh
   cargo run --release -- /path/to/zdiff_txs.json
   ```

   The 2026-07-17 review run's extracted cases are committed as
   `testdata/ironwood-mainnet-review-2026-07-17.json`, so
   `cargo run --release -- testdata/ironwood-mainnet-review-2026-07-17.json`
   reproduces that PARITY result without rebuilding the chain.

Exit status 0 = parity (all cases accepted, negative control rejected);
nonzero = at least one divergence, printed per case.

## Notes

- The state service is a `PanicAssertion` mock: the harness supplies every
  transparent prevout via `known_utxos`, so any state lookup the verifier
  attempts is itself a failure signal.
- Verification needs no proving parameters (Zebra embeds/derives the
  verifying keys; the Halo2 VK build costs a few seconds on first use).
- Chain-level rules (anchors-in-state, cross-transaction nullifier dedup,
  turnstile, difficulty) are outside this harness's scope — it is a
  *transaction* differential.
