#!/usr/bin/env python3
"""Fat-tx bench: measure zebrad regtest latencies for ~1001-input transactions.

Reproduces the shape of Zcash mainnet block 3,382,603 (seven ~250KB shielding
consolidations with 1001 transparent inputs each) that busts Foundry's
5-second block-maker budget, using transparent-only spends so no proving is
needed. The hot path under test is identical: one sequential state-service
round-trip per transparent input during verification.

Phases:
  1. boot zebrad (Regtest, PoW disabled, unshielded coinbase spends allowed)
  2. generatetoaddress 110 blocks to the bench key
  3. fan out 7 mature coinbases into 7 x 1003 small P2PKH UTXOs; mine 1 block
  4. baseline: 2-input consolidation, timed sendrawtransaction
  5. seven 1001-input consolidations, timed sendrawtransaction each
  6. getblocktemplate (template mode), timed x3
  7. assemble the template into a block, getblocktemplate proposal mode, timed
  8. submitblock, timed
Prints a summary table; writes results JSON into the workdir.

Usage: bench.py [--keep] [--workdir DIR] [--zebrad PATH] [--port N]
"""

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

PRIVKEY = "11" * 32  # regtest-only throwaway key
FANOUTS = 7
CONS_INPUTS = 1001
FANOUT_OUTPUTS = 1003  # 1001 bench inputs + 2 spares (baseline tx)
FEE_CONS = 5_005_000  # ZIP-317 conventional fee for 1001 logical actions
FEE_BASE = 20_000

results = {"phases": {}, "consolidations": [], "notes": []}


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


class Bench:
    def __init__(self, args):
        self.zebrad = args.zebrad
        self.fatcraft = os.path.join(HERE, "fatcraft/target/release/fatcraft")
        self.workdir = args.workdir
        self.port = args.port
        self.url = f"http://127.0.0.1:{self.port}"
        self.proc = None
        self.keep = args.keep

    def rpc(self, method, params=None, timeout=1200):
        body = json.dumps(
            {"jsonrpc": "2.0", "id": "b", "method": method, "params": params or []}
        ).encode()
        req = urllib.request.Request(
            self.url, data=body, headers={"content-type": "application/json"}
        )
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                resp = json.load(r)
        except urllib.error.HTTPError as e:
            # zebra returns JSON error bodies with non-200 codes
            try:
                resp = json.load(e)
            except Exception:
                raise RuntimeError(f"{method}: HTTP {e.code}") from e
        dt = time.monotonic() - t0
        if resp.get("error"):
            raise RuntimeError(f"{method}: {resp['error']}")
        return resp["result"], dt

    def craft(self, job):
        job.setdefault("privkey_hex", PRIVKEY)
        p = subprocess.run(
            [self.fatcraft], input=json.dumps(job).encode(), capture_output=True
        )
        if p.returncode != 0:
            raise RuntimeError(f"fatcraft: {p.stderr.decode()}")
        return json.loads(p.stdout)

    def start_zebrad(self, miner_address):
        os.makedirs(self.workdir, exist_ok=True)
        toml = os.path.join(self.workdir, "zebrad.toml")
        with open(toml, "w") as f:
            f.write(f"""\
[network]
network = "Regtest"
listen_addr = "127.0.0.1:0"

[network.testnet_parameters]
should_allow_unshielded_coinbase_spends = true

[network.testnet_parameters.activation_heights]
NU5 = 1

[mining]
miner_address = '{miner_address}'

[state]
cache_dir = "{self.workdir}/zebra-cache"

[rpc]
listen_addr = "127.0.0.1:{self.port}"
enable_cookie_auth = false
cookie_dir = "{self.workdir}"
""")
        env = {k: v for k, v in os.environ.items() if not k.startswith("ZEBRA")}
        env["RUST_LOG"] = os.environ.get("BENCH_RUST_LOG", "info")
        self.logfile = open(os.path.join(self.workdir, "zebrad.log"), "ab")
        self.proc = subprocess.Popen(
            [self.zebrad, "-c", toml, "start"],
            stdout=self.logfile,
            stderr=subprocess.STDOUT,
            env=env,
        )
        deadline = time.monotonic() + 90
        while True:
            try:
                self.rpc("getblockcount", timeout=5)
                return
            except Exception:
                if self.proc.poll() is not None:
                    raise RuntimeError(
                        f"zebrad exited early; see {self.workdir}/zebrad.log"
                    )
                if time.monotonic() > deadline:
                    raise RuntimeError("zebrad RPC never came up")
                time.sleep(0.5)

    def stop_zebrad(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.proc = None


def varint(n):
    if n < 0xFD:
        return bytes([n])
    if n <= 0xFFFF:
        return b"\xfd" + n.to_bytes(2, "little")
    return b"\xfe" + n.to_bytes(4, "little")


def assemble_block(template, nonce_cs_solution):
    roots = template["defaultroots"]
    hdr = (
        template["version"].to_bytes(4, "little")
        + bytes.fromhex(template["previousblockhash"])[::-1]
        + bytes.fromhex(roots["merkleroot"])[::-1]
        + bytes.fromhex(roots["blockcommitmentshash"])[::-1]
        + template["curtime"].to_bytes(4, "little")
        + bytes.fromhex(template["bits"])[::-1]
        + nonce_cs_solution
    )
    txs = [template["coinbasetxn"]["data"]] + [
        t["data"] for t in template["transactions"]
    ]
    body = varint(len(txs)) + b"".join(bytes.fromhex(t) for t in txs)
    return (hdr + body).hex(), len(txs)


def nonce_and_solution_from(raw_block_hex):
    """Reuse an existing regtest block's nonce + solution bytes (PoW is
    disabled, so contents are irrelevant, but serialized length must match)."""
    raw = bytes.fromhex(raw_block_hex)
    i = 140  # 4 version + 32 prev + 32 merkle + 32 commitments + 4 time + 4 bits + 32 nonce
    fb = raw[i]
    if fb < 0xFD:
        sol_len, off = fb, 1
    elif fb == 0xFD:
        sol_len, off = int.from_bytes(raw[i + 1 : i + 3], "little"), 3
    else:
        raise RuntimeError("unexpected solution compactsize")
    return raw[108 : i + off + sol_len]


def zats(vout):
    if "valueZat" in vout:
        return int(vout["valueZat"])
    return round(float(vout["value"]) * 1e8)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--workdir", default=None)
    ap.add_argument(
        "--zebrad", default=os.path.join(REPO, "zebra/target/release/zebrad")
    )
    ap.add_argument("--port", type=int, default=18239)
    ap.add_argument(
        "--finalize-gap",
        type=int,
        default=101,
        help="blocks mined after the fan-out block so its UTXOs finalize to "
        "RocksDB before the timed phases (0 = spend from non-finalized memory)",
    )
    ap.add_argument(
        "--load",
        type=int,
        default=0,
        help="concurrent background readers hammering the read state service "
        "during the timed phases (models a busy production node)",
    )
    ap.add_argument(
        "--abandoned-submit",
        action="store_true",
        help="submit the block with a tiny client timeout and disconnect, "
        "then watch whether zebrad finishes verification and commits anyway "
        "(models a block maker giving up on a slow submitblock)",
    )
    args = ap.parse_args()
    if args.workdir is None:
        args.workdir = os.path.join(
            os.environ.get("TMPDIR", "/tmp"), f"fat-tx-bench-{os.getpid()}"
        )

    b = Bench(args)
    ok = False
    try:
        run(b, args)
        ok = True
    finally:
        b.stop_zebrad()
        if ok and not b.keep:
            shutil.rmtree(b.workdir, ignore_errors=True)
        else:
            log(f"workdir preserved: {b.workdir}")
    return 0


def start_load(b, n, top_height, stop):
    """n threads issuing verbose getblock reads in a tight loop, to contend
    with UTXO lookups on the read state service like a busy production node."""
    counters = [0] * n

    def worker(i):
        while not stop.is_set():
            try:
                b.rpc("getblock", [str(random.randint(1, top_height)), 2], timeout=30)
                counters[i] += 1
            except Exception:
                time.sleep(0.05)

    threads = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(n)]
    for t in threads:
        t.start()
    return threads, counters


def run(b, args):
    results["config"] = {"finalize_gap": args.finalize_gap, "load": args.load}
    key = b.craft({"mode": "address"})
    addr, lock_hex = key["address"], key["lock_script_hex"]
    log(f"bench address: {addr}")

    log("starting zebrad (regtest, PoW disabled)")
    b.start_zebrad(addr)

    log("mining 110 blocks to the bench address")
    t0 = time.monotonic()
    mined = 0
    while mined < 110:
        n = min(25, 110 - mined)
        hashes, _ = b.rpc("generatetoaddress", [n, addr], timeout=600)
        mined += len(hashes)
    results["phases"]["mine_110_blocks_s"] = round(time.monotonic() - t0, 3)
    height, _ = b.rpc("getblockcount")
    assert height == 110, f"expected height 110, got {height}"

    # Collect the first 7 coinbases (mature: spendable at height >= cb_height+100).
    coinbases = []
    for h in range(1, FANOUTS + 1):
        blk, _ = b.rpc("getblock", [str(h), 2])
        cb = blk["tx"][0]
        vout = cb["vout"][0]
        spk = vout["scriptPubKey"]["hex"]
        assert spk == lock_hex, f"coinbase spk mismatch at {h}: {spk}"
        coinbases.append({"txid": cb["txid"], "vout": 0, "value_zat": zats(vout)})
    cb_val = coinbases[0]["value_zat"]
    log(f"coinbase value: {cb_val} zats")

    # Fan out each coinbase into FANOUT_OUTPUTS small UTXOs.
    fee_fanout = 5000 * (FANOUT_OUTPUTS + 1) + 100_000
    fanout_value = (cb_val - fee_fanout - 5_000_000) // FANOUT_OUTPUTS
    assert fanout_value >= 100_000, f"coinbase too small to fan out: {cb_val}"
    log(f"fanning out 7 coinbases into {FANOUT_OUTPUTS} x {fanout_value} zats each")
    fanouts = []
    for i, cb in enumerate(coinbases):
        r = b.craft(
            {
                "mode": "fanout",
                "inputs": [cb],
                "fanout_count": FANOUT_OUTPUTS,
                "fanout_value_zat": fanout_value,
                "fee_zat": fee_fanout,
            }
        )
        txid, dt = b.rpc("sendrawtransaction", [r["hex"]])
        assert txid == r["txid"], f"fanout {i}: txid mismatch"
        log(f"  fanout {i}: {r['size']}B, 1 input, accepted in {dt:.3f}s")
        fanouts.append(r)
    results["phases"]["fanout_size_bytes"] = fanouts[0]["size"]

    hashes, dt = b.rpc("generatetoaddress", [1, addr], timeout=600)
    results["phases"]["mine_fanout_block_s"] = round(dt, 3)
    log(f"mined fan-out block in {dt:.3f}s")
    for f in fanouts:
        info, _ = b.rpc("getrawtransaction", [f["txid"], 1])
        assert info.get("height", -1) == 111, f"fanout unconfirmed: {f['txid']}"

    # Push the fan-out block past the non-finalized window so the bench UTXOs
    # are served from RocksDB, matching mainnet consolidations of old UTXOs.
    if args.finalize_gap:
        log(f"mining {args.finalize_gap} blocks so the fan-out UTXOs finalize")
        mined = 0
        while mined < args.finalize_gap:
            n = min(25, args.finalize_gap - mined)
            hashes, _ = b.rpc("generatetoaddress", [n, addr], timeout=600)
            mined += len(hashes)
    tip, _ = b.rpc("getblockcount")
    assert tip == 111 + args.finalize_gap, f"unexpected tip {tip}"

    load_stop = threading.Event()
    load_counters = []
    if args.load:
        log(f"starting {args.load} background read-load threads")
        _, load_counters = start_load(b, args.load, tip, load_stop)
        time.sleep(1)  # let the load reach steady state

    # Baseline: 2-input consolidation from fanout 0's spare outputs.
    spare = [
        {"txid": fanouts[0]["txid"], "vout": v, "value_zat": fanout_value}
        for v in (CONS_INPUTS, CONS_INPUTS + 1)
    ]
    r = b.craft({"mode": "consolidate", "inputs": spare, "fee_zat": FEE_BASE})
    _, dt = b.rpc("sendrawtransaction", [r["hex"]])
    results["phases"]["baseline_2in_accept_s"] = round(dt, 3)
    log(f"baseline 2-input tx ({r['size']}B) accepted in {dt:.3f}s")

    # The main event: seven 1001-input consolidations.
    log(f"submitting {FANOUTS} consolidations of {CONS_INPUTS} inputs each")
    for i, f in enumerate(fanouts):
        inputs = [
            {"txid": f["txid"], "vout": v, "value_zat": fanout_value}
            for v in range(CONS_INPUTS)
        ]
        t0 = time.monotonic()
        r = b.craft({"mode": "consolidate", "inputs": inputs, "fee_zat": FEE_CONS})
        craft_s = time.monotonic() - t0
        _, dt = b.rpc("sendrawtransaction", [r["hex"]])
        log(
            f"  consolidation {i}: {r['size']}B, {CONS_INPUTS} inputs, "
            f"craft {craft_s:.2f}s, accepted in {dt:.3f}s"
        )
        results["consolidations"].append(
            {"size": r["size"], "craft_s": round(craft_s, 3), "accept_s": round(dt, 3)}
        )

    pool, _ = b.rpc("getrawmempool")
    log(f"mempool: {len(pool)} transactions")
    assert len(pool) == FANOUTS + 1, f"unexpected mempool: {pool}"

    # Template mode.
    gbt_times = []
    for i in range(3):
        tpl, dt = b.rpc("getblocktemplate")
        gbt_times.append(round(dt, 3))
    ntx = len(tpl["transactions"])
    log(f"getblocktemplate (template): {gbt_times} s, {ntx} txs in template")
    results["phases"]["gbt_template_s"] = gbt_times
    results["phases"]["gbt_template_txs"] = ntx
    if ntx != FANOUTS + 1:
        results["notes"].append(f"template selected {ntx} txs, expected {FANOUTS + 1}")

    # Proposal mode: validate the exact template as a block.
    raw_tip, _ = b.rpc("getblock", [str(tip), 0])
    block_hex, ntx_blk = assemble_block(tpl, nonce_and_solution_from(raw_tip))
    log(f"proposal block: {len(block_hex) // 2} bytes, {ntx_blk} txs")
    resp, dt = b.rpc(
        "getblocktemplate", [{"mode": "proposal", "data": block_hex}], timeout=1800
    )
    results["phases"]["gbt_proposal_s"] = round(dt, 3)
    results["phases"]["gbt_proposal_response"] = resp
    log(f"getblocktemplate (proposal): {dt:.3f}s, response: {json.dumps(resp)}")
    if isinstance(resp, dict) and resp.get("rejected") is not None:
        results["notes"].append(f"proposal rejected: {resp}")

    # Submit: full semantic verification + state commit.
    if args.abandoned_submit:
        t0 = time.monotonic()
        try:
            b.rpc("submitblock", [block_hex], timeout=0.05)
            log("abandoned-submit: call returned before the tiny timeout?")
        except Exception as e:
            log(f"abandoned-submit: client disconnected after 0.05s ({type(e).__name__})")
        landed = None
        while time.monotonic() - t0 < 30:
            height, _ = b.rpc("getblockcount", timeout=10)
            if height == tip + 1:
                landed = round(time.monotonic() - t0, 3)
                break
            time.sleep(0.05)
        results["phases"]["abandoned_submit_landed_s"] = landed
        log(f"abandoned-submit: block committed anyway after {landed}s"
            if landed else "abandoned-submit: block NEVER committed within 30s")
        height, _ = b.rpc("getblockcount")
        assert height == tip + 1, "abandoned submit: block lost"
    else:
        resp, dt = b.rpc("submitblock", [block_hex], timeout=1800)
        results["phases"]["submitblock_s"] = round(dt, 3)
        log(f"submitblock: {dt:.3f}s, response: {json.dumps(resp)}")
        height, _ = b.rpc("getblockcount")
        assert height == tip + 1, f"block not accepted, height {height}: {resp}"

    if args.load:
        load_stop.set()
        results["phases"]["load_threads"] = args.load
        results["phases"]["load_requests"] = sum(load_counters)
        log(f"load generator: {sum(load_counters)} verbose getblock reads served")

    # Report.
    cpu = subprocess.run(
        ["sysctl", "-n", "machdep.cpu.brand_string", "hw.ncpu"],
        capture_output=True,
        text=True,
    ).stdout.split("\n")
    results["host"] = {"cpu": cpu[0], "cores": cpu[1] if len(cpu) > 1 else "?"}

    out = os.path.join(b.workdir, "results.json")
    with open(out, "w") as f:
        json.dump(results, f, indent=2)

    accepts = [c["accept_s"] for c in results["consolidations"]]
    print("\n=== fat-tx bench results ===")
    print(f"config: finalize_gap={args.finalize_gap} load={args.load}")
    print(f"host: {results['host']['cpu']} ({results['host']['cores']} cores)")
    print(f"tx size: {results['consolidations'][0]['size']} bytes, {CONS_INPUTS} P2PKH inputs")
    print(f"baseline 2-input accept:        {results['phases']['baseline_2in_accept_s']:.3f}s")
    print(f"1001-input accept (7 txs):      min {min(accepts):.3f}s / max {max(accepts):.3f}s / total {sum(accepts):.3f}s")
    print(f"getblocktemplate template mode: {results['phases']['gbt_template_s']}s")
    print(f"getblocktemplate proposal mode: {results['phases']['gbt_proposal_s']:.3f}s  <-- Foundry's 5s budget")
    if "submitblock_s" in results["phases"]:
        print(f"submitblock:                    {results['phases']['submitblock_s']:.3f}s")
    if "abandoned_submit_landed_s" in results["phases"]:
        print(f"abandoned submit landed after:  {results['phases']['abandoned_submit_landed_s']}s")
    print(f"results: {out}")


if __name__ == "__main__":
    sys.exit(main())
