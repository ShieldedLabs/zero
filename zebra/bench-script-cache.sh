#!/usr/bin/env bash
# Run from anywhere with: bash path/to/zebra/bench-script-cache.sh
# Requires Python 3 and Zebra's build dependencies.
# Uses 10 ABBA blocks per input count, except 4 blocks for 7000 inputs.
set -euo pipefail

python3 - "$(dirname "${BASH_SOURCE[0]}")" <<'PY'
import csv
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

repo = Path(subprocess.check_output(
    ["git", "-C", sys.argv[1], "rev-parse", "--show-toplevel"],
    text=True,
).strip())

without45 = "968c7d681bc59498b04be8c04ea0bae2875fca5c"
with45 = subprocess.check_output(
    ["git", "rev-parse", without45 + "^"], cwd=repo, text=True,
).strip()
revisions = {"with45": with45, "without45": without45}

# Approximately comparable observation durations for the smaller fixtures.
batches = {1: 1000, 20: 200, 1001: 1, 7000: 1}
blocks = {1: 10, 20: 10, 1001: 10, 7000: 4}

base = repo / "zebra/target/script-cache-comparison"
base.mkdir(parents=True, exist_ok=True)
results = Path(tempfile.mkdtemp(prefix="paired.", dir=base))
print(f"Results: {results}", flush=True)

bench_path = Path("zebra/zebra-consensus/benches/script.rs")
source = (repo / bench_path).read_bytes()
(results / "script.rs").write_bytes(source)

env = os.environ.copy()
env.update(RAYON_NUM_THREADS="4", TOKIO_WORKER_THREADS="1")

executables = {}

for variant, revision in revisions.items():
    tree = base / variant
    if not tree.exists():
        subprocess.run(
            ["git", "worktree", "add", "--detach", str(tree), revision],
            cwd=repo, check=True,
        )

    actual = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=tree, text=True,
    ).strip()
    assert actual == revision, f"Wrong revision in {tree}"

    # Only the shared measurement source may differ from the fixed revision.
    subprocess.run(
        ["git", "diff", "--exit-code", revision, "--", ".",
         f":(exclude){bench_path.as_posix()}"],
        cwd=tree, check=True,
    )

    target = tree / bench_path
    if target.read_bytes() != source:
        target.write_bytes(source)

    print(f"Building {variant}...", flush=True)
    command = [
        "cargo", "rustc", "--locked", "--profile", "bench",
        "-p", "zebra-consensus", "--features", "bench-internals",
        "--bench", "script", "--message-format=json-render-diagnostics",
        "--", "--check-cfg", "cfg(cache_enabled)",
    ]
    if variant == "with45":
        command += ["--cfg", "cache_enabled"]

    build_log = results / f"build-{variant}.json"
    with build_log.open("w") as output:
        subprocess.run(
            command, cwd=tree / "zebra", env=env,
            stdout=output, check=True,
        )

    messages = [
        json.loads(line) for line in build_log.read_text().splitlines()
    ]
    executable, = [
        message["executable"]
        for message in messages
        if message.get("reason") == "compiler-artifact"
        and message.get("target", {}).get("name") == "script"
        and message.get("executable")
    ]
    executables[variant] = executable

metadata = {
    "revisions": revisions,
    "batches": batches,
    "abba_blocks": blocks,
    "rayon_workers": 4,
    "tokio_workers": 1,
    "platform": platform.platform(),
    "RUSTFLAGS": env.get("RUSTFLAGS", ""),
    "CARGO_ENCODED_RUSTFLAGS": env.get("CARGO_ENCODED_RUSTFLAGS", ""),
    "rustc": subprocess.check_output(
        ["rustc", "-Vv"], cwd=base / "with45/zebra", text=True,
    ),
}
(results / "environment.json").write_text(json.dumps(metadata, indent=2))

values = {}
block_ratios = {}

with (results / "measurements.csv").open("w", newline="") as raw:
    writer = csv.writer(raw)
    writer.writerow([
        "inputs", "block", "position", "variant", "batch",
        "first_ns", "hit_ns",
    ])

    for inputs, batch in batches.items():
        block_count = blocks[inputs]
        processes = {}
        logs = {}
        samples = {"cache_disabled": [], "cache_miss": [], "cache_hit": []}
        ratios = {"cache_miss": [], "cache_hit": []}

        try:
            for variant in revisions:
                log = (results / f"{inputs}-{variant}.stderr").open("w")
                logs[variant] = log
                process = subprocess.Popen(
                    [executables[variant]],
                    cwd=base / variant / "zebra",
                    env={**env, "BENCH_INPUTS": str(inputs)},
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=log,
                    text=True,
                    bufsize=1,
                )
                processes[variant] = process
                assert process.stdout.readline().strip() == "READY", (
                    f"{variant} failed to initialize; see {log.name}"
                )

            for block in range(1, block_count + 1):
                print(f"{inputs} inputs: ABBA block {block}/{block_count}", flush=True)
                observations = []

                for position, variant in enumerate(
                    ("with45", "without45", "without45", "with45"), start=1
                ):
                    process = processes[variant]
                    process.stdin.write(f"{batch}\n")
                    process.stdin.flush()

                    fields = process.stdout.readline().split()
                    assert len(fields) == 2, (
                        f"{variant}: expected two timings; see {logs[variant].name}"
                    )
                    first, hit = map(float, fields)
                    assert first > 0 and (
                        hit > 0 if variant == "with45" else hit == 0
                    )

                    observations.append((first, hit))
                    writer.writerow([
                        inputs, block, position, variant, batch, first, hit,
                    ])
                    raw.flush()

                    if variant == "with45":
                        samples["cache_miss"].append(first)
                        samples["cache_hit"].append(hit)
                    else:
                        samples["cache_disabled"].append(first)

                a1, b1, b2, a2 = observations
                for kind, index in (("cache_miss", 0), ("cache_hit", 1)):
                    # Pair adjacent A/B observations, then average within ABBA.
                    ratios[kind].append((
                        math.log(a1[index] / b1[0])
                        + math.log(a2[index] / b2[0])
                    ) / 2)

        finally:
            for process in processes.values():
                process.stdin.close()
            for variant, process in processes.items():
                assert process.wait() == 0, (
                    f"{variant} failed; see {logs[variant].name}"
                )
            for log in logs.values():
                log.close()

        values[inputs] = samples
        block_ratios[inputs] = ratios

lines = [
    "",
    "Verification times: arithmetic means; 20 observations per case, or 8 for 7000 inputs.",
    "Changes: paired geometric means relative to no caching.",
    "",
    f"{'Inputs':>6}  {'No cache':>10}  {'Miss':>10} {'':>7}  {'Hit':>10} {'':>7}",
]

for inputs, samples in values.items():
    scale, unit = (1000, "µs") if inputs <= 20 else (1_000_000, "ms")
    means = {kind: statistics.mean(samples[kind]) / scale for kind in samples}
    changes = {
        kind: 100 * math.expm1(statistics.mean(block_ratios[inputs][kind]))
        for kind in ("cache_miss", "cache_hit")
    }
    disabled = f"{means['cache_disabled']:.0f} {unit}"
    miss = f"{means['cache_miss']:.0f} {unit}"
    miss_change = f"({changes['cache_miss']:+.0f}%)"
    hit = f"{means['cache_hit']:.0f} {unit}"
    hit_change = f"({changes['cache_hit']:+.0f}%)"
    lines.append(
        f"{inputs:>6}  {disabled:>10}  {miss:>10} {miss_change:>7}  {hit:>10} {hit_change:>7}"
    )

lines.append("")
lines.append("Positive change: slower with caching. Negative change: faster with caching.")

summary = "\n".join(lines)
print(summary)
(results / "summary.txt").write_text(summary + "\n")
print(f"\nResults saved in: {results}")
PY
