# nymnet: a local Nym mixnet harness

A complete Nym mixnet on localhost — three mixnodes and one entry gateway, no
nyxd chain, no nym-api, no credentials — plus a probe binary that drives real
`nym-sdk` clients through it. Its purpose is to answer the SDK-behaviour
questions in `../NYM_PLAN.md` by measurement instead of documentation-reading,
and to give the M4/M5 driver code a real mixnet to be tested against without
touching the public network.

Everything is pinned to one nym release (`PIN` in `localnet.sh`, the git tag in
`probe/Cargo.toml`; they must match): the nodes and the clients come from the
same code the shim and hub will embed. Post-2024.12 per NYM_PLAN D12.

## What it answers that channel-level tests cannot

* **SURB accounting (D3/D4).** One SURB carries one ~2 KiB reply packet, so a
  full-frame 64 KiB `LookupReplyV1` needs ~35+ of them, and a reply that
  outruns the attached count stalls on re-request round trips.
  `./localnet.sh lookup <n>` measures the elapsed time of a 64-byte request
  answered by a 64 KiB anonymous reply at attached-SURB count `n`; sweeping
  `n` locates the threshold M4's lookup constant must clear.
* **Sender-tag lifecycle (D11).** `./localnet.sh smoke` asserts one stable
  `AnonymousSenderTag` across a session's submits and a fresh tag after a
  client rebuild — the premise D11's rotation policy rests on.
* **Anonymous-send guard (D3).** The smoke test asserts every submit arrives
  with a sender tag and reassembles at exactly frame size, over the real SDK
  rather than a channel fake.
* **Client lifecycle (D12), later.** The gateway is a local process you can
  kill and restart deterministically — the reconnect/rebuild behaviour the
  M4/M5 driver must handle, untestable against the public mixnet.

What it can NOT answer: real-world round-trip latency, live gateway churn,
ticketbook/credential provisioning, and the StageX/musl build. Those stay with
NYM_PLAN's M0/M5/M6 against real infrastructure.

## Usage

```sh
./localnet.sh up          # clone+build at the pin (first run), start 4 nodes,
                          # assemble network.json, write harness.env
./localnet.sh smoke       # 64 KiB submits + 64-byte SURB acks + tag assertions
./localnet.sh lookup 13   # 64-byte request, 64 KiB anonymous reply, 13 SURBs
./localnet.sh lookup 60   # ...compare elapsed times across counts
./localnet.sh wire        # ship the crates' committed golden-vector frames
                          # through the mixnet; verify byte-identity both ways
                          # plus an independent offset-level decode
./localnet.sh status      # what's running
./localnet.sh env         # paths/ids for external consumers (gated tests)
./localnet.sh down        # stop the nodes
./localnet.sh clean       # also remove run dir and node configs
```

State lives outside the repo: the pinned nym checkout and build in
`~/.cache/zeronym-nymnet` (override with `NYMNET_HOME`), node configs in
`~/.nym/nym-nodes/zeronym-ln-*`, logs and pids and `network.json` in
`~/.cache/zeronym-nymnet/localnet`. The probe builds into `probe/target`
(gitignored). Ports used, all loopback: 10001-10004 (mixnet), 20001-20004
(verloc), 30001-30004 (node http), 9000 (gateway client websocket).

## How it works

Adapted from upstream `scripts/localnet_start.sh` at the pinned release, with
two deliberate changes:

1. **Background processes instead of tmux.** Each node runs under `nohup` with
   a pidfile and a log in the run dir, so the harness is scriptable and CI-able.
2. **The topology file is assembled by the probe, not by upstream's
   `build_topology.py`.** The Python script emits a format the current
   `NymTopology::new_from_file` no longer parses. `probe topology` builds the
   file with the SDK's own serde types — correct by construction — reading each
   node's `/api/v1/host-information` for its identity key and the sphinx key of
   the **current rotation** (sphinx keys rotate ~daily; the rotation id is
   stamped into the topology metadata, which is why assembly happens at `up`
   time from live nodes rather than from static init output).

Clients connect with `MixnetClientBuilder::new_ephemeral()` plus a
`HardcodedTopologyProvider` over `network.json`; gateway selection comes from
the same file (`available_gateways()` uses the custom provider), so no client
ever dials nym-api. Empty inbound messages are SDK SURB-replenishment
artifacts and are filtered, exactly as the hub listener does (D12).

## Measured results

Recorded in `../NYM_PLAN.md` (the feature's master document): the harness
subsection under "Status and handoff", and the corrections folded into D3, D4,
the lookup wire rules, M0, and M4.

## Extending

The probe is the seed of the M4/M5 test surface. Natural next probes, in the
order the plan needs them:

* sweep `lookup` over SURB counts in CI and assert the chosen constant clears
  the re-request threshold;
* a malicious-hub probe that spams `AdditionalSurbsRequest` to verify the
  shim-side `maximum_allowed_reply_surb_request_size` clamp bounds hoarding
  (the D3 rewrite);
* kill/restart the gateway around a submit to exercise the driver's
  reconnect-and-rebuild path (D12);
* packet-count capture at the gateway to pin D4's constant-count property.
