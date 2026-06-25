# Shield zcashd behind Zebra

Put Zebra in front of zcashd so that zcashd never touches the public network
directly. Zebra faces the Zcash P2P network; zcashd connects only to Zebra,
parsing and validating nothing from the internet itself. This removes zcashd's
network-facing message parser and peer-management code from your attack surface,
while your wallet or mining pool keeps talking to zcashd's RPC as before.

Background: [Reduce your zcashd attack surface by shielding it behind Zebra](https://shieldedlabs.net/advisory-reduce-your-zcashd-attack-surface-by-shielding-it-behind-zebra/).

```
            public P2P (8233)            internal only
  Internet <----------------> Zebra <----------------> zcashd
                              (front)   connect=         (RPC 8232 -> your
                                        zebra:8233        wallet / pool)
```

The whole mitigation is two lines in `zcash.conf`:

```
connect=zebra:8233   # one outbound peer: Zebra. (127.0.0.1:8233 if same host)
listen=0             # no inbound P2P
```

## Files

| File | Purpose |
|------|---------|
| `docker-compose.yml` | Two services: `zebra` (public 8233) and `zcashd` (internal). |
| `zebrad.toml` | Zebra config. Just listens on 8233; no special setup. |
| `zcash.conf` | zcashd config. `connect=zebra:8233` + `listen=0` + your RPC block. |
| `.env.example` | Pin the image version (`ZERO_IMAGE_TAG`). |
| `systemd/` | `zebrad.service` + `zcashd.service` for non-Docker hosts. |

## Option A: Docker

```sh
cp .env.example .env          # optional: pin ZERO_IMAGE_TAG=v3
# edit zcash.conf: set a real rpcpassword
docker compose up -d
docker compose logs -f
```

Images are pulled from `ghcr.io/shieldedlabs/zero-zebra` and `-zcashd` (built
from this repo's vendored source). To build locally instead, uncomment the
`build:` block in `docker-compose.yml`.

## Option B: No Docker (binaries or systemd)

Run the same two config files directly. Use the prebuilt Zero `zebrad` binary
(e.g. `zebrad-v3-linux-x86_64` from a [Zero release](https://github.com/ShieldedLabs/zero/releases))
and a Zero `zcashd` binary:

```sh
zebrad --config ./zebrad.toml
# in zcash.conf, set connect=127.0.0.1:8233 for a single host
zcashd -conf=$PWD/zcash.conf -datadir=/var/lib/zcash -printtoconsole
```

For production, install the binaries to `/usr/local/bin`, put the configs under
`/etc/zebra` and `/etc/zcash`, set `cache_dir`/`-datadir` to `/var/lib/...`, and
use the units in `systemd/` (`zcashd.service` starts after `zebrad.service`).

### Already running zcashd?

You do not need this whole bundle. Point your existing zcashd at a Zebra node by
adding two lines to its `zcash.conf` and restarting:

```
connect=<zebra-host>:8233
listen=0
```

## Verify the shield

Confirm zcashd is talking only to Zebra and exposes no P2P surface.

```sh
# Docker (prefix with: docker compose exec zcashd ...)
zcash-cli -conf=/etc/zcash/zcash.conf getpeerinfo
#   -> exactly ONE peer, the Zebra node.

zcash-cli -conf=/etc/zcash/zcash.conf getnetworkinfo
#   -> "connections": 1  and  "listen": false
```

On the zcashd host, confirm nothing is listening on the P2P port (only Zebra
should be):

```sh
ss -tlnp | grep 8233     # zcashd host: no output. Zebra host: zebra listening.
```

In Docker, confirm zcashd publishes no host port while Zebra is reachable:

```sh
docker compose port zcashd 8233   # empty
docker compose port zebra  8233   # 0.0.0.0:8233
```

If `getpeerinfo` shows peers other than your Zebra, or `"listen": true`, the
`connect=`/`listen=0` settings are not in effect: check that zcashd loaded this
`zcash.conf`.

> [!NOTE]
> The first sync takes time. The shield (single peer, no listener) is in effect
> immediately on startup; you do not need a fully synced chain to verify it.
