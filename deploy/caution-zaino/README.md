# caution-zaino: zainod inside a Caution enclave

Runs zainod (the Zcash CompactTxStreamer indexer, vendored at `zaino/`) inside
[Caution](https://caution.co) (AWS Nitro Enclaves, EnclaveOS, reproducible EIF
builds) against a zebra validator running OUTSIDE the enclave: a zero-zebra
v21 node on an instance in the SAME VPC subnet as the enclave (Caution side),
with its state seeded from one of our synced caches to skip the multi-day
initial sync (see SEEDING.md). The same-subnet private address natively
satisfies both verified zainod link constraints below. The enclave has no
persistent disk; v0 uses zaino's upstream `ephemeral_finalised_state = true`
mode, which opens no LMDB and proxies finalised reads to the validator. Zero
zaino source patches required.

## Layout

```
assemble.sh                  builds the standalone push repo (see below)
overlay/                     copied verbatim onto the assembled repo root
  Containerfile              StageX static musl zainod (from zaino/Dockerfile.deterministic)
  caution.hcl                enclave resources, network rules, unit command
  config/                    baked zainod profiles, selected via unit.args
    testnet-ephemeral.toml   default: testnet, zero-disk
    mainnet-ephemeral.toml   mainnet, zero-disk
    mainnet-tmpfs.toml       roadmap stub: persistent DB in RAM, not for v0
rehearsal/                   local pre-flight config, see REHEARSAL.md
```

## How deployment works

Caution builds `docker build -f Containerfile .` from the ROOT of the repo you
push it, with no build args. The zero monorepo is the wrong shape for that, so
`assemble.sh` produces a small standalone repo: a `git subtree split` of the
`zaino/` prefix (upstream history plus our `[zero]` carries preserved) with the
overlay committed on top.

```sh
deploy/caution-zaino/assemble.sh            # assembles ../zaino-caution
deploy/caution-zaino/assemble.sh /tmp/ctx   # or anywhere else
```

Then `git push <caution-remote> main --force` from the assembled repo
(`caution init` / their onboarding provides the remote). The assembled main is
rebuilt each run, hence the force push; provenance is in the overlay commit
message (`caution build context from zero@<sha>`).

If subtree split ever becomes too slow, the flat fallback is
`git archive HEAD:zaino | tar -x -C <dest>` plus the overlay, at the cost of
history in the pushed repo.

## Environment flips

Testnet to mainnet is three edits in `overlay/caution.hcl`:

1. `unit.args` config path: `mainnet-ephemeral.toml`
2. `unit.env` `ZAINO_VALIDATOR_SETTINGS__VALIDATOR_JSONRPC_LISTEN_ADDRESS`
3. `network.egress` port 8232 (and tighten `cidr_ipv4` to the validator /32)

Constraints inherited from zainod (upstream behavior, verified in source; do
not fight them):

- The validator address must resolve to a private/loopback IP at config load;
  public IPs are rejected there (config.rs, `is_private_listen_addr`). A
  hostname that does not resolve at load passes and is only resolved again at
  connect time with no further IP-class check, but do not lean on that.
- The validator RPC hop is plaintext HTTP: the URL scheme is hardcoded
  (`connector.rs`, `format!("http://{}:{}")`) and no config or feature
  produces an https client, even though rustls is linked. Basic-auth and
  cookie credentials travel base64-in-cleartext. Consequence: the
  enclave-to-validator link must be private end to end (same VPC, peering, or
  a host-level tunnel); never the public internet.
- Secret-like config keys (passwords, cookies, tokens) are refused via env and
  must live in a config file. v0 sidesteps auth entirely: expose the k8s
  zebra RPC with `enable_cookie_auth = false` on an internal-only service,
  locked down by network policy to the enclave egress path.

## Sizing (answer to "how much RAM")

| Mode | Working set | memory_mb | Basis |
|---|---|---|---|
| ephemeral (v0) | 32 MiB idle, 77 MiB peak under a sustained 25k-block on-demand scan | 8192 (wildly conservative; 512-1024 would run) | MEASURED 2026-07-23, diskless read-only container vs local mainnet zebra |
| persistent tmpfs (roadmap) | full LMDB, est 50-80 GiB (45 GiB measured partial) + tuned heaps | 98304-131072 | r6i.4xlarge class parent |
| lightwalletd --nocache (fallback) | stateless proxy | 2048 | well under 1 GiB |
| lightwalletd cache tmpfs | 30.7 GiB cache (measured 2026-07) + margin | 49152 | rebuilds from genesis every boot |

Measured behavior (rehearsal, ephemeral mode): startup to serving is dominated
by building the ~1001-block non-finalised window (a couple of minutes when the
validator tip is also moving; near-instant against a settled tip). Recent
GetBlockRange (blocks already in the window) streams sub-second. Deep
historical GetBlockRange proxies every block from the validator at roughly
50 blocks/s and is bounded by zaino's streaming timeout (4x `service.timeout`,
so about 120 s per call by default); wallets chunk these ranges, but raise
`service.timeout` if a single long scan matters. RSS stays flat regardless,
so RAM is a non-issue for v0.

## Privacy posture, stated honestly

v0 gives an attested, reproducible build; it is an operator-blind endpoint only
if TLS terminates inside the enclave. In ephemeral mode nearly every query is
forwarded to zebra as plaintext JSON-RPC, so the validator host still sees
access patterns (GetTransaction txids, GetTaddress* addresses, block ranges).
Roadmap: persistent in-enclave block serving (mainnet-tmpfs profile or
Caution's disk-over-vsock when it lands), then an in-enclave address index so
queries stop leaving the enclave.

## Open questions for Caution (Anton)

1. The validator box: an instance in the enclave's VPC subnet with 500 GB+
   fast disk (gp3/NVMe) running our zero-zebra v21 image, RPC 8232 bound to
   its private address, cookie auth off, security-grouped to the enclave
   egress path only. We seed its state (SEEDING.md) to skip the initial sync.
2. Does the `http`/Caddy ingress carry gRPC (HTTP/2), and does TLS terminate
   inside the enclave (STEVE) or outside? Raw TCP 8137 is our fallback.
3. Builder: network during `docker build` (we `cargo fetch`, then build with
   `--network=none`)? BuildKit features OK (dockerfile:1, cache mounts)?
4. Does `unit.command` override the image ENTRYPOINT?
5. Max `memory_mb` / instance types available now; BYOC needed for the
   128 GiB+ persistent mode later?
6. Debug console for bring-up (our hcl enables it; disable for attested demo).
7. If the StageX build misbehaves under deadline, is a Debian-based image an
   acceptable temporary fallback?

## Fallback: lightwalletd --nocache

If the static zainod build burns more than a day, lightwalletd is a pure-Go
stateless alternative: `stagex/pallet-go` static build of the vendored
`lightwalletd/`, flags via env (`NOCACHE=true`, `RPCUSER`/`RPCHOST`/etc.,
`LOG_FILE=/dev/stdout`, `GEN_CERT_VERY_INSECURE=true` for in-memory TLS),
runs in well under 1 GiB. Not built unless needed.

## CI

`.github/workflows/caution-zaino.yml` assembles the context and docker-builds
the Containerfile on an x86 runner (dispatch, or PRs touching this dir,
`zaino/Cargo.lock`, or `zaino/Dockerfile.deterministic`). Keep it green before
any push to Caution: their builder must never be first contact with a change.
