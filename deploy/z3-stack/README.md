# Run the Z3 stack: Zebra + Zaino + Zallet

The zcashd-free deployment. Zebra validates and faces the network, Zaino
serves the lightwalletd-compatible gRPC interface for light clients, and
Zallet is the zcashd-compatible wallet your systems talk to. This is the
long-term supported direction for Zcash infrastructure; if you are still
running zcashd, see the companion bundle
[Shield zcashd behind Zebra](../zcashd-behind-zebra/).

```
           public P2P (8233)             internal compose network
 Internet <----------------> zebra <--- JSON-RPC 8232 ---+--- zaino   (gRPC 8137 -> light clients)
                                                         |
                                                         +--- zallet  (wallet RPC 28232 -> your systems)
```

Zaino and Zallet both talk directly to Zebra's JSON-RPC. Zallet does not use
the zaino container (it embeds Zaino's indexer libraries); zaino is here for
light-client serving. If you do not need the lightwalletd interface, you can
delete the `zaino` service and nothing else changes.

Only Zebra's P2P port (8233) is published to the host. Zebra's RPC is
unauthenticated and stays on the compose network; the wallet RPC and the
gRPC interface have commented-out `ports:` mappings you can enable
deliberately.

## Files

| File | Purpose |
|------|---------|
| `docker-compose.yml` | Three services: `zebra` (public 8233), `zaino`, `zallet` (both internal). |
| `zebrad.toml` | Zebra config: P2P listener + JSON-RPC for zaino/zallet (cookie auth off; the compose network is the auth boundary). |
| `zainod.toml` | Zaino config: fetch backend pointed at `zebra:8232`, gRPC on 8137. |
| `zallet.toml` | Zallet config: validator `zebra:8232`, age keystore, wallet RPC on 28232. CHANGE the rpc password. |
| `.env.example` | Pin the image version (`ZERO_IMAGE_TAG`). |
| `docker-compose.build.yml` | Optional overlay: build the images from this repo's vendored source instead of pulling. |

Images are pulled from `ghcr.io/shieldedlabs/zero-zebra`, `-zaino`, and
`-zallet` (built from this repo's vendored source). To build locally instead,
add the build overlay:

```sh
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build
```

CI runs this stack end to end on every change to the bundle or the component
Dockerfiles (`.github/workflows/z3-smoke.yml`).

The images are `linux/amd64`; ARM64 hosts (e.g. Apple Silicon) run them under
emulation. Building `zallet` locally on ARM64 additionally requires
`platform: linux/amd64` on the service, because its reproducible-build (stagex)
base images are amd64-only.

## Quick start

```sh
cp .env.example .env          # optional: pin ZERO_IMAGE_TAG=v3
# edit zallet.toml: set a real [[rpc.auth]] password

# One-time wallet init (next section): identity file + wallet encryption
# + mnemonic. Do this BEFORE first `up` so zallet starts with a real wallet.

docker compose up -d zebra    # let the node come up first...
docker compose up -d          # ...then the rest (the healthcheck enforces
                              # this ordering even if you skip the first step)
docker compose logs -f
```

The first mainnet sync takes days and roughly 300 GB. Zaino and zallet start
as soon as Zebra's RPC answers and simply trail its sync; they do not need a
synced chain to start (see [Verify the stack](#verify-the-stack) for how to
watch progress).

## One-time wallet init

Zallet encrypts all key material with [age](https://age-encryption.org/).
You generate the identity file; zallet cannot do it for you (the container
image is a single static binary with no shell).

**1. Generate the age identity**, next to this README so the compose file can
mount it:

```sh
rage-keygen -o encryption-identity.txt      # or: age-keygen -o ...
# No rage/age on the host? Generate it in a throwaway container:
docker run --rm -v "$PWD:/out" alpine:3.20 \
  sh -c 'apk add --no-cache age >/dev/null && age-keygen -o /out/encryption-identity.txt'
# zallet runs as UID 1000; make the file readable by it:
sudo chown 1000:1000 encryption-identity.txt && sudo chmod 400 encryption-identity.txt
```

For a passphrase-protected identity (you will then need `walletpassphrase`
before spends), see the
[Zallet setup guide](../../zallet/book/src/guide/setup.md).

**2. Initialize wallet encryption, then generate the spending seed** (the
flags are repeated because `docker compose run` replaces the service command):

```sh
docker compose run --rm --no-deps zallet \
  --datadir /var/lib/zallet --config /etc/zallet/zallet.toml init-wallet-encryption

docker compose run --rm --no-deps zallet \
  --datadir /var/lib/zallet --config /etc/zallet/zallet.toml generate-mnemonic
```

`generate-mnemonic` prints a seed fingerprint (`zip32seedfp1...`); record it,
it names this spend authority in other zallet commands. Run it ONCE: every
run adds another independent root of spend authority to the wallet.

**3. Back up, now.** This bundle disables zallet's built-in backup gate
(`require_backup = false` in `zallet.toml`), so nothing will force you. Three
things belong together, and a wallet cannot be restored without all of them:

- the mnemonic: `docker compose run --rm --no-deps zallet --datadir
  /var/lib/zallet --config /etc/zallet/zallet.toml export-mnemonic`
- the age identity: `encryption-identity.txt`
- the wallet database: the `zallet-data` volume

## Verify the stack

The fastest full check is the smoke script, which runs the exact probe
sequence CI uses (liveness per layer plus the wallet-RPC contract checks)
and prints one `ok:`/`FAIL:` line per probe:

```sh
./smoke.sh                  # against a running stack, ~2 minutes
./smoke.sh --init --up      # first run: wallet init + compose up first
```

For iterating on locally built images, build with the overlay first:

```sh
docker compose -f docker-compose.yml -f docker-compose.build.yml build
ZERO_IMAGE_TAG=latest ./smoke.sh --init --up
```

The manual per-layer proofs below remain for debugging a specific layer,
from the bottom up.

**Zebra answers RPC** (this is also exactly what the compose healthcheck
does):

```sh
docker inspect --format '{{.State.Health.Status}}' zebra    # "healthy"

docker compose exec zebra curl -s \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":"v","method":"getblockchaininfo","params":[]}' \
  http://127.0.0.1:8232
#   -> "blocks": <rising number>
```

**Zaino serves gRPC.** `docker compose logs zaino` should show it following
Zebra, with no restart loop. For a positive gRPC proof (zainod does not
enable gRPC reflection, so grpcurl needs the proto from this repo):

```sh
# (zaino-proto/proto/ holds symlinks; mount the real directory so they
# resolve inside the container)
docker run --rm --network z3-stack_default \
  -v "$PWD/../../zaino/packages/zaino-proto/lightwallet-protocol/walletrpc:/protos:ro" \
  fullstorydev/grpcurl -plaintext -import-path /protos -proto service.proto \
  zaino:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
#   -> JSON with "chainName", "blockHeight", ...
```

**Zallet sees the chain and its wallet:**

```sh
docker compose exec zallet zallet \
  --datadir /var/lib/zallet --config /etc/zallet/zallet.toml rpc getwalletstatus
#   -> "node_tip" present = zallet <-> zebra works end to end;
#      "wallet_tip" / "fully_synced_height" show wallet-side sync.

docker compose exec zallet zallet \
  --datadir /var/lib/zallet --config /etc/zallet/zallet.toml rpc z_listaccounts
```

**Nothing is exposed that shouldn't be** (`docker port` lists a container's
published ports; empty means nothing reaches it from outside the compose
network):

```sh
docker port zebra    # only the P2P port (8233; 18233 on Testnet). RPC must NOT appear.
docker port zaino    # empty (unless you deliberately published gRPC)
docker port zallet   # empty (unless you deliberately published the wallet RPC)
```

## Watch-only transparent import (z_importaddress)

Migrating a zcashd `importaddress` / `importmulti` watch-only flow? Zallet's
equivalent is `z_importaddress`, and the `zero-zallet` images ship with it
enabled (the Dockerfile builds with `--features rpc-cli,zcashd-import`, which
includes the `transparent-key-import` feature). It is a compile-time feature;
there is no `zallet.toml` or environment switch.

Two differences from zcashd matter:

- **It takes a hex-encoded public key (P2PKH) or redeem script (P2SH), not a
  `t1...` address.** zcashd accepted the bare address and watched its
  scriptPubKey; zallet needs the actual key material. For a P2PKH address you
  must supply its 33-byte compressed (or 65-byte uncompressed) pubkey.
- **`rescan` (third parameter, default `true`) rescans from the target
  account's birthday, not from genesis.** The call returns immediately and
  the scan runs in the background sync tasks (watch `getwalletstatus`).
  History older than the account's birthday will not be discovered, and
  `z_getnewaccount` pins a new account's birthday at the current chain tip.
  To import an address that already has history, create the target account
  with `z_recoveraccounts` instead: it takes an explicit `birthday_height`,
  which must be set before the address's first use.

```sh
# The account UUID comes from z_getnewaccount / z_recoveraccounts (or
# z_listaccounts). The `zallet rpc` CLI parses each param as JSON, so
# string params need the extra quotes:
docker compose exec zallet zallet \
  --datadir /var/lib/zallet --config /etc/zallet/zallet.toml \
  rpc z_importaddress '"<account-uuid>"' '"<hex-pubkey-or-redeem-script>"'
#   -> {"type": "p2pkh", "address": "t1..."}
```

One caveat when auditing a binary: `help` and `rpc.discover` list every
method known at build time, whether or not its feature was compiled in, so
`help z_importaddress` proves nothing. A build without the feature answers
every call with "z_importaddress requires the transparent-key-import
feature"; these images answer with real parameter validation, and the smoke
workflow asserts exactly that.

## Security notes

- **Never publish Zebra's 8232.** It is unauthenticated by design here;
  compose-network isolation is the auth boundary (that is also why cookie
  auth is off, which is what replaces the cookie-permissions sidecar from the
  upstream z3 compose).
- **Change the zallet RPC password** in `zallet.toml` before first start.
  Every wallet RPC request must authenticate; prefer a hashed entry via
  `zallet add-rpc-user <user>` for production (note the `zallet rpc` CLI in
  the verification steps above only works with a bare `password` entry).
- **Zaino's gRPC is plaintext.** `zero-zaino` is built without TLS (the same
  trade-off as upstream's `-no-tls` images) so it can bind non-loopback on
  the private compose network. If you expose 8137 beyond this host, put a
  TLS-terminating proxy in front of it.
- **Wallet RPC to the host:** if your systems expect zcashd on 8232, map
  `"8232:28232"` on the zallet service rather than changing the internal
  port.

## Testnet

The ports are set explicitly in the config files, so switch them together
with the network names (18233/18232 are the testnet conventions):

- `zebrad.toml`: `network = "Testnet"`, `listen_addr = "[::]:18233"`, and
  `[rpc] listen_addr = "0.0.0.0:18232"`.
- `docker-compose.yml`: port mapping `"18233:18233"` and the healthcheck URL
  becomes `http://127.0.0.1:18232`.
- `zainod.toml`: `network = 'Testnet'` and
  `validator_jsonrpc_listen_address = 'zebra:18232'`.
- `zallet.toml`: `network = "test"` and `validator_address = "zebra:18232"`.
