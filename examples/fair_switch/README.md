# Fair switch

Self-contained A2 switch using the supplied SDK. The simulator, senders, and
scoring code are unchanged. The machine's documented app port is 100; switch
IDs, customer addresses, peers, and network links are discovered at runtime.

## Build and run

Run these commands from the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release
cargo build --manifest-path examples/fair_switch/Cargo.toml --release --target wasm32-unknown-unknown

./target/release/competitive_net_sim run-world worlds/practice/a2p2-dumb-003.toml \
  --program examples/fair_switch/target/wasm32-unknown-unknown/release/fair_switch.wasm \
  --score
```

The program to submit is:

```text
examples/fair_switch/target/wasm32-unknown-unknown/release/fair_switch.wasm
```

## Tests and reports

```sh
cargo test --manifest-path examples/fair_switch/Cargo.toml
cargo test --release --test fair_switch_integration
python3 examples/fair_switch/tools/check_practice.py
python3 examples/fair_switch/tools/check_practice.py --loads 0.5 0.75 1.25 2
```

The Python runner requires Python 3 and the already-built release binaries.
It runs all five practice worlds at loads 1 and 1.5 by default. Every case
keeps its full text and JSON report in `examples/fair_switch/results/`.
Select worlds with repeated `--world NAME` arguments; change concurrency
with `--jobs` and the report directory with `--results`.

The recorded final results are in [VALIDATION.md](VALIDATION.md): all 30
practice runs passed for the 949-line condensed switch, along with 11 unit
tests and three WASM integration tests.

The WASM integration tests build the switch automatically and exercise a
15-customer network under normal fuel/output limits, IPv4 prefix queue
isolation, and a calm network with link failure and restoration. These are synthetic regression worlds;
the original A1 worlds are not included in this repository.

## Design card

### Customers and scheduling

Each customer has a stable queue number based on contract order. Data is
classified by prefix rules installed at startup. TinyVM LPM uses IPv4
prefix lengths directly: `/24` means 24, not 56. Endpoint addresses and
data peers are learned from traffic for routing.
ACKs are identified by packet kind and use queue 0 with control traffic.

Every port uses byte-based DRR: customer weights are 1, queue 0 has weight
4. This isolates AIMD and rate-matching customers and fairly serves both
1,000-byte and 200-byte packets. An empty queue reserves no wire time.
Queue 0 has preferential weighted service; the SDK cannot combine strict
control priority with a DRR customer scheduler.

### Buffer allocation

The switch reserves 2 full-size packets per port for control/ACK queues,
one per port for packets currently serializing, and one shared packet for
a route transition. At five ports, 24 packets remain for normal customer
queues. Unused customer queues have zero capacity.

Every required customer queue gets at least one packet. Remaining packets
increase AIMD queues toward four packets, then rate-matching queues toward
two, distributing shortages in rounds. Limits are in 1,000-byte units;
200-byte data and 40-byte ACKs consume only their actual bytes.

AIMD halves its rate after congestion loss and recovers additively. Its
larger queue absorbs short overloads and reduces repeated cuts.
Rate matching follows acknowledged service with gain 1.25 and a two-BDP
flight limit, with a four-packet minimum. Its smaller queue bounds the
standing backlog. Hard caps provide isolation; no extra early-drop line
is configured. Loss, delay, and unused reserved space are the costs.

Old queue reservations remain charged during route changes. A new queue
can use unreserved space immediately, but cannot borrow an old queue's
space before retirement. The coordinator checks the transition at every
switch, including old queue depths rather than only queue counts.

### Discovery and load information

HELLOs discover switch neighbors and their local ports. Flooded snapshots
advertise directed capacities, endpoint addresses, learned data peers, and
per-queue drop counts every 500 ms. Sequence numbers suppress duplicates;
large messages are fragmented and reassembled with bounded state.

Only a data-specific punt learns a customer's peer. ACKs can originate
from customers that send their own traffic to a different destination.
Treating these ACKs as data would corrupt the peer binding.

The controller does not measure sending rates, queue occupancy, or delay.
It plans using contractual ON demand even while a customer is quiet.
No drops does not establish that a customer stopped sending. Unused
capacity is still available through DRR's service of nonempty queues.

The A2 contract establishes that links do not fail, so congestion or
missing HELLOs do not trigger link-down declarations. With an empty
contract, the A1 mode uses HELLO timeouts and declares detected failures
and restorations. It recomputes shortest paths after those changes.

### Traffic engineering

The lowest discovered switch ID coordinates path placement. It enumerates
simple routes within shortest-plus-two hops. Initial placement gives
customers with fewer alternatives first choice, followed by larger demand
(including unlimited demand). Each chooses the path with the largest
minimum capacity per assigned customer, preferring shorter paths on ties.
Endpoint-link capacities are included.

Progressive filling computes predicted max-min allocations. Discovering a
new peer permits a coordinated baseline re-placement if its transition
fits the buffer; otherwise existing paths are preserved while the new
customer is added. Detours and placement are cached and processed across
callbacks to respect the four-million-instruction limit.

For ongoing adaptation, a customer must have drops in two consecutive
reports. Evaluations rotate through customers, comparing at most two
alternatives per callback. A move cannot lower the minimum predicted
reference ratio. Other customers retain their current prediction or at
least 80% of their reference, whichever is smaller. This permits a small
trade from a well-served customer to relieve a badly served one.

A move needs at least a 10% improvement in predicted throughput/minimum
share, or a 25% reduction in other customers' congestion with allocations
protected. The customer's own drops are excluded from that congestion
score. There is a two-second cooldown per moved customer and one active
route transaction at a time. These are heuristics, not a guarantee that
every hidden world passes.

### Route changes and control safety

`custom0` carries an immutable path identifier from ingress to egress.
Transit switches look up that identifier rather than independently
changing the next hop for the destination. Each route is simple, so old
and new packets can coexist without mixing path segments into a loop.

The coordinator prepares forwarding entries and queue space everywhere,
then waits for READY acknowledgements before
activating ingress. Sources acknowledge applied activation. Only after
every source has switched and the conservative old-path drain bound has
elapsed does the coordinator retire old routes. A switch stages an early
PREPARE for the next epoch until the previous retirement finishes. Its next
READY acknowledgement confirms that retirement and the new preparation
have applied. The coordinator repeats retirement messages to recover from
loss. Messages retry every 500 ms.

Table installation in this simulator **appends** entries. Ingress updates
therefore use distinct entry IDs and increasing epoch priorities; the old
entry is deleted only during retirement. Reinstalling an existing key at
equal priority would leave the old route selected.

Control output is paced by bytes per port, not packet count, so tiny
acknowledgements can progress promptly. Encoded callback output stays
below 4,096 bytes. The pipeline uses six stages, below the limit of eight.

## Limits and remaining uncertainty

The supported machine has up to 15 contractual customers and five ports.
Its queues, shared buffer, timers, and controller limits are the supplied
A2 interface. The forwarding protocol uses `custom0` and metadata keys
internally; it is not intended to compose with an unrelated encapsulation
using the same fields.

The full report matrix is the acceptance evidence. Unit tests alone do
not establish floor performance. Hidden topologies and customer mixes
remain untested, and the synthetic A1 regression does not substitute for
running the original A1 grading worlds.
