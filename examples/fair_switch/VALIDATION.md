# Validation results

The **949-line condensed switch** passed all **30/30** practice world/load
combinations after fixing IPv4 prefix classification. These are fresh
results from the current source and its generated WASM, not the earlier
pre-refactoring binary. The incorrect `32 + prefix_len` made `/24` customer
rules behave as exact address matches and sent customer data into queue 0.
Using the contractual prefix length restores customer isolation.

Verified artifact: `target/wasm32-unknown-unknown/release/fair_switch.wasm`
within this example. SHA-256:

```text
12475a75c1d45a5031163287d782732ba231b9b6d977021cb8f13762bc4fa95f
```

Loads tested: **0.5, 0.75, 1, 1.25, 1.5, and 2** on each of the five
supplied worlds. Every run passed all floors, the no-loop gate, and the
no-starvation gate. There were zero false link-down declarations.

## Graded loads

These are rounded worst-window percentages from the report cards, not
whole-run averages. F1 requires 40%; F2 requires 55%.

| World | ×1 F1 | ×1 F2 | ×1.5 F1 | ×1.5 F2 |
|---|---:|---:|---:|---:|
| a2p1-dumb-004 | 87% | 90% | 87% | 91% |
| a2p1-grid-009 | 88% | 96% | 80% | 94% |
| a2p1-ring-020 | 93% | 96% | 68% | 85% |
| a2p2-dumb-003 | 74% | 84% | 73% | 90% |
| a2p2-grid-006 | 73% | 84% | 77% | 96% |

Across all six loads, the lowest rounded F1 was 45% and the lowest F2
was 71%. The fresh full text and JSON reports are in `results/prefix-fix/` after running
the practice runner; generated reports are ignored by Git.

## Automated regressions

- **11 unit tests passed:** path enumeration, wide-detour selection,
  max-min allocation with small demands and multiple bottlenecks,
  buffer reservations, stale/invalid transaction handling, ingress
  priority updates and retirement, ACK/data peer separation, output
  size bounds, pipeline branch targets, and a new preparation overtaking
  an old retirement without advancing the wrong epoch.
- **3 real WASM integration tests passed:** fifteen simultaneous
  customers on fifteen switches under the default machine limits, and
  a calm A1-style network with link failure and restoration, and real
  pipeline execution verifying `/24` customer data uses separate queues
  while ACKs and control packets use queue 0. The tests also check packet
  paths for loops and controller failures.

The original A1 grading worlds and hidden A2 worlds are not available
here. These results establish performance on the supplied practice
worlds and the synthetic regressions, rather than a hidden-world guarantee.

## Reproduce

From the repository root:

```sh
cargo build --release
cargo build --manifest-path examples/fair_switch/Cargo.toml --release --target wasm32-unknown-unknown
cargo test --manifest-path examples/fair_switch/Cargo.toml
cargo test --release --test fair_switch_integration
python3 examples/fair_switch/tools/check_practice.py --loads 0.5 0.75 1 1.25 1.5 2 --results examples/fair_switch/results/prefix-fix
```
