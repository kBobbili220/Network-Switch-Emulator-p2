# NetworkArena — Assignment 2 kit: Make It Fair

Same simulator, same one program on every switch you own. The networks
are no longer empty: they carry customers with demands and reactions of
their own, and at least one link in every world cannot carry what is
asked of it. Your program decides who gets the capacity: what waits,
what goes first, what is dropped, and which path each customer takes.

The assignment text and the grading rules are in the handout your instructor
gave you. They are not in this kit. `docs/a2-floors.md` says exactly what
each line of the report card measures.

## Layout

```
src/  switch_program_types/  switch_program_sdk/   the simulator and the SDK
examples/hello_switch/       the smallest program: runs, forwards nothing, every customer starves
examples/learning_switch/    55 lines: one static route on the first punt. A shape to copy, not a solution
examples/load_report/        measures the load on each link and shares it with every switch. Moves nothing
examples/fair_switch/        complete A2 switch: isolated DRR queues and coordinated detour routing
worlds/practice/             a2p1-*: one bottleneck (3 worlds)   a2p2-*: several, with detours (2 worlds)
viewer/                      the Assignment 1 browser replay: packets moving and dying, not queues
docs/                        simulator reference, and the floors
```

Your starting point is your own Assignment 1 switch: it delivers, and it
melts under load.

## Setup

```sh
rustup target add wasm32-unknown-unknown
cargo build --release
```

## Build your program

The implemented A2 switch is in [`examples/fair_switch`](examples/fair_switch/README.md).
Its README includes build commands, the design card, and the practice-world test runner.

Your program is its own crate, like `examples/hello_switch/`. Build it to
wasm and hand the simulator the file:

```sh
cd examples/hello_switch          # or wherever your crate lives
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/hello_switch.wasm
```

## What is new in your program

Everything from Assignment 1 still works. The switch now also has:

- **Queues, a schedule, a drop line** per port: `actions::set_queues(port,
  discipline, specs)` with `Discipline::{Fifo, StrictPriority, Drr}` and
  `QueueSpec::new(cap).weight(w).drop_at(n)`. Your TinyVM program picks a
  queue per packet with `queue N` (or a table's `SetQueue`). Every switch
  has one buffer pool, `buffer_budget_packets` full-size packets in the
  world file (40 x 1000 bytes); the queues you ask for draw from it. Caps
  and drop lines are in full-size packets, and the pool is charged in
  bytes: Jane Street and Fortnite send 200-byte packets, acks are 40.
- **What it dropped:** every `on_timer` carries `ev.port_drops`, per port,
  since your previous timer -- in total, by your own drop line, and by
  queue. Nothing else about queue state is reported.
- **How fast its cables are:** implement `init_with(input: InitInput)`
  instead of `init` to read `input.port_capacity_bps`, one per
  `local_ports` entry. Who is on the other end you still learn yourself.
- **The customer contract:** `input.customers`, the same list at every
  switch, one `Customer` per customer: its source prefix
  (`prefix_addr`, `prefix_len`), how its sender slows down (`cca`:
  `Cca::Aimd` or `Cca::RateMatch`) and the rate it asks for (`ask_bps`,
  `None` for no limit; already multiplied by `--load-scale`). Every
  customer keeps its contract in this assignment. One row looks like
  `Customer { prefix_addr: 0x0A008E00, prefix_len: 24, cca: Cca::Aimd,
  ask_bps: Some(4_000_000) }`: the customer in 10.0.142.0/24 is an AIMD
  sender that asks for 4 Mbit/s. Four things to know:
  - A row is a sender. Packets carry the prefix as their source address;
    a receiver has no row. Two Fortnite players send to each other, so
    they are two rows.
  - For Amazon, `ask_bps` is its rate while a burst is on. The contract
    does not say when a burst begins or ends.
  - The contract has no topology: not where a customer connects, and not
    which links its traffic uses. You learn that as in Assignment 1.
  - `src/flow.rs` is the sender. Both rules are there, with every number.
- **A voice:** `actions::link_down(port)` and `link_up(port)` put on record
  what your switch believes about a link. No link fails in this
  assignment; the card holds every declaration against the truth.

`docs/switch-programs.md` has the details and `docs/LIMITS.md` the ceilings.

## What you know about the load

You do not design the measurement in this assignment. `examples/load_report`
is all of it, and you may copy any part into your switch:

1. it reads the customer contract and each link's capacity at init, and
   each port's drop counts on every timer;
2. every half second it gives each link a load level (0 to 3) and counts
   the customers that want it;
3. it floods a small report, so every switch holds the same picture:
   `(from, to) -> capacity, load level, customers`.

It installs no routes and no queues. Which queue each kind of customer
gets, which customer goes on which path, and when to move it, is yours.

## Run it

```sh
./target/release/competitive_net_sim run-world \
  worlds/practice/a2p1-ring-020.toml \
  --program YOUR.wasm \
  --score
```

The card gives the result first, then one row for each floor. Below that
it lists every customer (goodput, worst share, delay and loss; delay and
loss are not graded), each customer's share of its fair share second by
second, the load on every busy link, where packets stopped and why, and
what went wrong. `docs/a2-floors.md` explains each part. It exits
non-zero when a floor fails. Add `--json card.json` for the same card as
data, with every customer, link and switch buffer per one-second window.

`--load-scale 1.5` asks every customer for half again as much. We grade
every world at `--load-scale 1` and at `1.5`.

Part 2 worlds (`a2p2-*`) have at least two thin links, and a way around each. A
program that only ever takes the shortest path fails them: the fair
share there is computed over every route within two hops of the
shortest, not just the one you chose. Demand shifts every 8 to 10
seconds, when a burst starts or stops.

Sweep everything:

```sh
for w in worlds/practice/a2p*.toml; do
  ./target/release/competitive_net_sim run-world "$w" --program YOUR.wasm --score
done
```

Draw a load-goodput curve: the same world at rising load, one JSON card
each. `customers[].goodput_mbps` in each file is the y axis.

```sh
for x in 0.5 0.75 1 1.25 1.5 2; do
  ./target/release/competitive_net_sim run-world worlds/practice/a2p1-ring-020.toml \
    --program YOUR.wasm --score --load-scale $x --json load-x$x.json \
    | grep -E '^ (F1|F2|F3|OVERALL)'
done
```

## Watch a run

The viewer is the one from Assignment 1: it replays packets, drops and
table installs. It does not draw queues or link load; for those, read the
card and its JSON.

Add `--log run.simlog` to any command above, then serve the viewer over HTTP
(opening the file directly will not work — it loads wasm as a module):

```sh
python3 -m http.server -d viewer/web 8080
```

Open <http://localhost:8080> and drag `run.simlog` onto the page.

## Reference

| File | What it covers |
|------|----------------|
| `docs/a2-floors.md` | Every line of the report card: windows, demand, the three floors, delay and loss, the collapse note. |
| `docs/switch-programs.md` | Your API: the SDK, the TinyVM text format, the raw wasm ABI. |
| `docs/LIMITS.md` | Fuel, output size, buffer, queues, control time. |
| `docs/tinyvm.md` | The data-plane instruction set. |
| `docs/sim_log.md` | The `.simlog` format and the viewer. |
| `docs/programming-a-switch.md`, `docs/building-a-network.md` | How the simulator is built. Not your API. |
