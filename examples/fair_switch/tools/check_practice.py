#!/usr/bin/env python3
"""Run independent scored worlds and retain report cards for every window."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--loads", nargs="+", type=float, default=[1, 1.5])
parser.add_argument("--world", action="append", help="World filename stem; repeat to select several")
parser.add_argument("--jobs", type=int, default=2)
parser.add_argument("--results", type=Path, default=ROOT / "examples/fair_switch/results")
args = parser.parse_args()
args.results.mkdir(parents=True, exist_ok=True)
worlds = sorted((ROOT / "worlds/practice").glob("a2p*.toml"))
if args.world:
    worlds = [w for w in worlds if w.stem in args.world]
if not worlds:
    parser.error("no matching practice worlds")
program = ROOT / "examples/fair_switch/target/wasm32-unknown-unknown/release/fair_switch.wasm"
simulator = ROOT / "target/release/competitive_net_sim"
if not program.exists() or not simulator.exists():
    parser.error("build the release simulator and WASM switch first (see README)")

def run(case):
    world, load = case
    name = f"{world.stem}-x{load:g}"
    command = [str(simulator), "run-world", str(world), "--program", str(program),
               "--score", "--load-scale", str(load), "--json", str(args.results / f"{name}.json")]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=180)
    report = result.stdout + result.stderr
    (args.results / f"{name}.txt").write_text(report)
    floors = [line.strip() for line in report.splitlines() if re.match(r"\s*F[123] ", line)]
    details = " | ".join(floors) if floors else report[-600:]
    print(f"{'PASS' if result.returncode == 0 else 'FAIL'} {name}: {details}", flush=True)
    return result.returncode == 0

with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as executor:
    results = list(executor.map(run, [(w, load) for w in worlds for load in args.loads]))
print(f"{sum(results)}/{len(results)} runs passed; reports: {args.results}")
sys.exit(0 if all(results) else 1)
