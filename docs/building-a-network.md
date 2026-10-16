# Building a network

A network is a graph of `Node`s — apps and switches — connected by `Link`s.
The `Simulator` owns all of them and drives time forward by popping events
off the event queue.

```rust
pub enum Node {
    Switch(SwitchId),
    App(AppId),
}
```

## 1. Create a simulator

```rust
let mut sim = Simulator::new();
```

The simulator holds:

- the event queue
- a map of `AppId → App`
- a map of `SwitchId → Switch`
- a map of `LinkId → Link`
- the `Network` graph (which `(node, port)` pairs are connected to which
  link)

Time starts at `Duration::ZERO`.

## 2. Add apps

An app is an endpoint: it generates packets according to a
`TrafficPattern` and records metrics on what it receives.

```rust
let app_a = AppId::new(1);
let a = App::new(
    app_a,
    /* this app's IP    */ IpAddr(0x0a000001),
    /* destination IP   */ IpAddr(0x0a000002),
    TrafficPattern::ConstantBitrate {
        interval: Duration::from_millis(10),
        size_bytes: 100,
    },
);
sim.add_app(a);
```

Adding an app schedules its first `AppTick` at the current sim time.
Available patterns (see `src/app.rs`):

- `ConstantBitrate { interval, size_bytes }`
- `Poisson { lambda_pps, size_bytes, seed }` — deterministic xorshift RNG
- `BurstyOnOff { on_duration, off_duration, on_interval, size_bytes, .. }`
- `RequestResponse { gap, size_bytes, waiting }` — sends a request, waits
  for a reply (`App::deliver` flips `waiting = false`), then waits `gap`
  before the next request
- `BulkTransfer { total_bytes, sent_bytes, pacing, size_bytes }` — emits
  back-to-back packets paced by `pacing` until `sent_bytes >= total_bytes`

Every app exposes `App::metrics: AppMetrics` after the run with packets
sent/received, bytes sent/received, average + p50/p95/p99 delay, loss
rate, and throughput.

For an app that should be receive-only, hand it any pattern with a huge
interval and zero size, or a `BulkTransfer` with `total_bytes = 0`.

## 3. Add switches

See [programming-a-switch.md](programming-a-switch.md). One call:

```rust
sim.add_switch(switch);
```

## 4. Connect them with links

```rust
let lc = LinkConfig {
    latency:               Duration::from_micros(100),
    bandwidth_bps:         1_000_000_000,    // 1 Gbps
    queue_capacity_bytes:  1 << 16,
};

sim.connect(
    Node::App(app_a),         PortId::new(0),
    Node::Switch(switch_id),  PortId::new(0),
    lc,
);
sim.connect(
    Node::Switch(switch_id),  PortId::new(1),
    Node::App(app_b),         PortId::new(0),
    lc,
);
```

A link is full-duplex: each direction has its own wire, and traffic one
way never waits for, or takes buffer from, traffic the other way.

A `LinkConfig` models:

- **propagation latency** — fixed delay added after serialization
- **serialization delay** — `size_bytes * 8 / bandwidth_bps`, so larger
  packets back-to-back queue up behind each other
- **egress queue size** — `queue_capacity_bytes` sizes the drop-tail
  queue at an *app's* end of the link. Switch ports are sized at
  `Switch::new`.

The queue itself belongs to the sending node's **egress port**
(`egress.rs`), not to the link. A packet is admitted if the port's queue
and the node's shared buffer pool both have room, and dropped with reason
`QueueFull` otherwise; when the wire goes idle the port's scheduler picks
the next packet to serialize. With the default single FIFO queue packets
leave in arrival order, exactly like a drop-tail link.

A switch's egress port routes via the link registered for
`(Node::Switch(switch_id), port)`. If no link is attached to that port the
packet is dropped and counted in `Switch::packets_dropped_no_egress`.

## 5. Run the simulation

Two driver entry points:

```rust
sim.run_until(Duration::from_millis(100));   // run up to a deadline
sim.run();                                   // run until queue empties
```

Read out metrics afterwards:

```rust
let m = &sim.apps[&app_b].metrics;
println!("received {} pkts, avg delay {:?}, throughput {} bps",
    m.packets_received, m.avg_delay, m.throughput_bps);
```

## 6. Determinism

The event queue orders events first by simulated time, then by a
monotonically-increasing sequence number that's stamped at scheduling
time. So two events scheduled at the same time fire in the order they
were scheduled, regardless of how the heap shuffles them. Combined with
deterministic traffic generators (the Poisson generator uses a seeded
xorshift, not the OS RNG), runs are reproducible bit-for-bit.

The `App::deliver` -> recv-side metrics path uses the simulator's `now`
when the `AppDeliver` event fires, never wall-clock time.

## 7. Hooking in controller events

Two simulator helpers let tests / topology code drive controller behavior
directly:

```rust
sim.schedule_controller_timer(switch_id, /*at*/ Duration::from_millis(50));
sim.deliver_link_event(switch_id, LinkEvent::Down { port: PortId::new(2) });
```

The first schedules a `ControllerTimer` event in the queue; when it
fires, the controller's `on_timer(now)` is called and the resulting
actions are dispatched through the config pipe like any other.

`deliver_link_event` invokes `on_link_event` synchronously — useful for
modeling external link state changes (e.g., a peer marked the port down).

## 8. End-to-end walkthrough

The full `App A → Switch → App B` example with an exact-match route lives
in `src/main.rs`. Run with `cargo run`. To see fancier topologies, look
at `tests/integration.rs`:

- `multi_hop_delivery_and_delay_accumulation` — chained switches
- `controller_installed_route_affects_later_packets` — punt-driven
  learning controller
- `recirculation_consumes_extra_processing` — TinyVM recirculation cost
- `cpu_failure_does_not_stop_data_plane`, `switch_failure_drops_all_traffic`
  — failure model
