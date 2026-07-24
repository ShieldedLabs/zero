# Local rehearsal: diskless zainod against a synced mainnet zebra

Pre-flight for the Caution deployment. Proves the exact enclave topology:
ephemeral zainod, external validator, zero writable disk.

Two validator options; either satisfies zainod's private-address rule:

- **Fast path: the k8s zebra.** `kubectl port-forward` the zebra RPC service
  to local port 8232; the rehearsal config's `host.docker.internal:8232` then
  reaches it via a loopback tunnel. No waiting for local catch-up.
- **Local container** (`zcash-zebrad-1`, RPC on host port 8232, cookie auth
  off): fully offline-capable, but must catch up first (step 1).

## 1. Start the validator and let it catch up

```sh
docker start zcash-zebrad-1
```

Poll until `blocks` reaches `estimatedheight` (a multi-week gap takes a few
hours):

```sh
curl -s -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
  http://127.0.0.1:8232/ | jq '.result.blocks, .result.estimatedheight'
```

## 2. Run zainod diskless

Uses the published glibc image (already built with the no-TLS feature) and the
rehearsal profile. `--read-only` is the zero-disk proof: any stray write
fails loudly instead of silently landing on disk. The tmpfs mounts cover the
paths the entrypoint seeds (home, /tmp, /run).

Starting zainod BEFORE zebra finishes catching up is a feature, not a mistake:
it exercises the `[zero]` unsynced-validator hardening this topology relies on.

```sh
docker run --rm --name zaino-rehearsal \
  --read-only \
  --tmpfs /home/container_user \
  --tmpfs /tmp \
  --tmpfs /run \
  -e XDG_RUNTIME_DIR=/run \
  -p 127.0.0.1:8137:8137 \
  -v "$PWD/deploy/caution-zaino/rehearsal/zainod-rehearsal.toml:/cfg/zainod.toml:ro" \
  ghcr.io/shieldedlabs/zero-zaino:latest \
  start --config /cfg/zainod.toml
```

## 3. Verify over gRPC

No reflection is compiled in; pass the protos from the vendored tree
(`brew install grpcurl` if missing):

```sh
P=zaino/packages/zaino-proto/proto
grpcurl -plaintext -import-path $P -proto service.proto \
  127.0.0.1:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
grpcurl -plaintext -import-path $P -proto service.proto \
  127.0.0.1:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock
```

Then a 1001-block range scan (the worst realistic single request in ephemeral
mode) while watching memory:

```sh
TIP=$(curl -s -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}' \
  http://127.0.0.1:8232/ | jq -r .result)
grpcurl -plaintext -import-path $P -proto service.proto \
  -d "{\"start\":{\"height\":$((TIP-1000))},\"end\":{\"height\":$TIP}}" \
  127.0.0.1:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange \
  | grep -c '"height"'
```

```sh
docker stats --no-stream zaino-rehearsal
```

## 4. Record results

Note peak RSS during the range scan and startup-to-serving time, then update
the sizing table in README.md (ephemeral row: estimated to measured) and the
numbers quoted to Caution.

## Troubleshooting

- Entrypoint mkdir failures under `--read-only`: add a `--tmpfs` for whatever
  path it complains about; do NOT drop `--read-only`.
- zainod rejects the validator address: `host.docker.internal` must resolve
  inside the container (Docker Desktop provides it); on Linux add
  `--add-host=host.docker.internal:host-gateway`.
- Port 8137 busy: another zaino instance is running; stop it first.
