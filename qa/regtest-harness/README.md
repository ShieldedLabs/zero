# Z3 regtest harness

Deterministic wallet-behavior regression tests for the Z3 stack (zebrad +
zallet with the zaino backend), run as local processes on a private regtest
chain. Complements `.github/workflows/z3-smoke.yml`: smoke proves the *shipped
containers* come up and serve on a fresh mainnet node; this harness proves the
*wallet logic* stays correct under the stateful conditions that have failed in
production, which a fresh empty wallet can never exercise.

## What it covers, and why

Every scenario is a regression guard for a specific production incident:

| Scenario | Guards against | Incident |
|---|---|---|
| `baseline` | listunspent contract, imported-address watch-only flag, response latency, clean errors | RPC hang class, `is_watch_only` account-level regression |
| `spend-poison` | an on-chain spend the wallet did not create is discovered and recorded (signer wallet shields a coinbase, fresh seed recovery must see the spend) | hot-wallet false-unspent accumulation: address-scoped spend-check watermark skipping unfetched spend heights |
| `dust` | sub-marginal-fee (≤5000 zat) UTXOs omitted from `z_listunspent` | zcash/zallet#594 |
| `filter` | `addresses` filter ignored for transparent outputs; cross-address leakage | zcash/zallet#595 |
| `union` | multi-address filters matching nothing (`all` vs `any`) | zcash/zallet#596 |
| `hang-guard` | a filtered listing sweeping *other* addresses' dust through per-outpoint checks (~10-minute RPC hang on an exchange wallet) | v11→v12 regression |
| `poison-heal` | a stored transaction row with no mined height and zero expiry crash-looping the wallet at startup; it must instead be skipped and self-heal via the status sweep | zcash/zallet#568 |

Scenarios run in order and are stateful by design (later scenarios build on
the chain and wallet mutations of earlier ones). `--only` skips scenarios but
never reorders them.

## Running locally

```sh
# Build the binaries it needs, then run everything (~10-15 minutes,
# dominated by mining 240 regtest blocks):
qa/regtest-harness/run.sh --build

# Re-run a subset against existing binaries:
qa/regtest-harness/run.sh --only dust,filter

# Keep the stack alive afterwards for interactive poking:
qa/regtest-harness/run.sh --keep
```

Requirements: `bash`, `curl`, `python3`, `sqlite3` (all present on macOS and
`ubuntu-latest`). Binaries default to `zebra/target/debug/zebrad` (must be
built with `--features internal-miner`) and `zallet/target/debug/zallet`
(must be built with `--no-default-features --features
zaino,rpc-cli,zcashd-import`); override with `ZEBRAD_BIN` / `ZALLET_BIN`.

On failure the workdir (configs, logs, wallet.db) is preserved and its path
printed.

## Design constraints

- **Deterministic**: regtest dials no peers; block production uses zebra's
  internal miner in a mine-then-freeze pattern (the chain is static while
  assertions run). Funding goes to two pinned, clearly-labeled public test
  keypairs, mirroring the imported-watch-address shape of the exchange
  deployments that hit the incidents above.
- **Hang-proof**: every RPC call carries a per-call timeout (`curl -m`),
  every wait is a bounded poll, and process shutdowns escalate after a bound.
  A hang anywhere becomes a red assertion, not a stuck job.
- **Wallet-state surgery is explicit**: scenarios that need pathological
  database states (dust values, poisoned rows) write SQL only while zallet is
  stopped, and every write is scoped to the throwaway workdir database.

## CI

`.github/workflows/z3-regtest.yml` builds the two binaries (cargo, cached)
and runs this script on pull requests touching the wallet-behavior surface
(`zallet/`, `zaino/packages/`, `librustzcash/`), on manual dispatch, and
weekly. Logs and the wallet database are uploaded as an artifact on failure.
