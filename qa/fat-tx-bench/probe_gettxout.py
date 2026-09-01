#!/usr/bin/env python3
"""UTXO lookup latency probe for zebrad. Stdlib only; safe and read-only.

Context: blocks that consolidate ~1000-input transactions (e.g. Zcash mainnet
block 3,382,603 with seven 1001-input transactions) make zebrad's submitblock
perform ~7,000 UTXO lookups, one sequential awaited round-trip per input.
The wall time of that phase is roughly (per-lookup latency) x (input count).
This script measures per-lookup latency on a live node via `gettxout`, which
exercises the same read path, and extrapolates.

The in-block verification path adds one extra internal hop per lookup, so the
real submitblock cost is somewhat HIGHER than this estimate. Treat the output
as a lower bound.

Usage:
    python3 probe_gettxout.py [--url http://127.0.0.1:8232] [--samples 1000]
                              [--inputs 7007]

Prints latency percentiles and the extrapolated serial lookup time. Run it on
the same host as zebrad (or wherever your block maker's RPC client runs, to
include the network hop it actually pays).
"""

import argparse
import http.client
import json
import random
import statistics
import sys
import time
import urllib.parse


class Rpc:
    def __init__(self, url):
        p = urllib.parse.urlparse(url)
        self.host, self.port = p.hostname, p.port or 8232
        self.conn = None

    def call(self, method, params=None, timeout=30):
        body = json.dumps(
            {"jsonrpc": "2.0", "id": "p", "method": method, "params": params or []}
        )
        if self.conn is None:
            self.conn = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        try:
            self.conn.request(
                "POST", "/", body, {"Content-Type": "application/json"}
            )
            resp = json.loads(self.conn.getresponse().read())
        except Exception:
            self.conn = None
            raise
        if resp.get("error"):
            raise RuntimeError(f"{method}: {resp['error']}")
        return resp["result"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:8232")
    ap.add_argument("--samples", type=int, default=1000)
    ap.add_argument(
        "--inputs",
        type=int,
        default=7007,
        help="transparent input count to extrapolate to (block 3382603 had 7x1001)",
    )
    args = ap.parse_args()

    rpc = Rpc(args.url)
    tip = rpc.call("getblockcount")
    try:
        version = rpc.call("getinfo").get("version")
    except Exception:
        version = "unknown"
    print(f"node: {args.url}  version: {version}  tip: {tip}")

    # Sample random (txid, vout) pairs from across the whole chain. Spent
    # outpoints still exercise the same lookup work, so no need to hunt for
    # unspent ones.
    print(f"collecting {args.samples} samples (sequential, like block verification)...")
    lat = []
    errors = 0
    t_start = time.monotonic()
    while len(lat) < args.samples:
        h = random.randint(1, tip)
        try:
            blk = rpc.call("getblock", [str(h), 1])
            txids = blk.get("tx", [])
            txid = random.choice(txids)
            t0 = time.monotonic()
            rpc.call("gettxout", [txid, random.randint(0, 1)])
            lat.append(time.monotonic() - t0)
        except Exception:
            errors += 1
            if errors > args.samples:
                print("too many errors; is the node reachable?", file=sys.stderr)
                return 1
            continue
        if len(lat) % 200 == 0:
            print(f"  {len(lat)}/{args.samples}")

    lat.sort()
    mean = statistics.fmean(lat)
    pct = lambda p: lat[min(len(lat) - 1, int(p / 100 * len(lat)))]
    print(f"\ngettxout latency over {len(lat)} samples "
          f"({time.monotonic() - t_start:.0f}s total, {errors} errors):")
    print(f"  min {lat[0]*1000:.2f}ms  p50 {pct(50)*1000:.2f}ms  "
          f"p90 {pct(90)*1000:.2f}ms  p99 {pct(99)*1000:.2f}ms  "
          f"mean {mean*1000:.2f}ms")
    print(f"\nextrapolated serial UTXO-lookup phase for a {args.inputs}-input block:")
    print(f"  mean x {args.inputs} = {mean * args.inputs:.1f}s   "
          f"(p90 x {args.inputs} = {pct(90) * args.inputs:.1f}s)")
    print("plus signature verification and state commit on top; treat as a lower bound.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
