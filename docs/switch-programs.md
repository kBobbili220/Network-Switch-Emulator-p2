# Writing switch programs

A **switch program** is the owner-supplied bundle of behavior for a switch.
Each switch in a topology has an owner (`OwnerId`); each owner provides a
single `.wasm` program; that program is loaded once per owned switch. The
program sees the switch's identity and its set of locally-attached ports
at `init` time, but nothing else — it has to discover the rest of the
network on its own.

A program supplies *both*:

1. The **data plane** — a TinyVM `TinyProgram` plus state declarations
   (tables, register arrays, counter arrays). Returned from `init` as a
   `ProgramSetup`. The simulator validates the TinyVM program against the
   normal budgets/limits before running it.
2. The **controller** — handlers for punts and timers. Implemented as
   `on_punt` and `on_timer`. Handlers return actions: install/delete table
   entries, inject control packets onto ports, reschedule timers, etc.
   Nothing notifies the controller that a link went down; a program that
   needs to know has to work it out for itself, the way a real protocol
   does.

There are two ways to write a program: the **Rust SDK** (recommended), or
the **raw ABI** (any language that compiles to wasm).

---

## 1. Building a `.wasm` with the Rust SDK

### 1.1 Set up the crate

A switch program is a stand-alone Cargo crate that compiles to a cdylib
for the `wasm32-unknown-unknown` target. Put it *outside* the
simulator's workspace so the wasm build doesn't fight with the host
build.

```toml
# my_program/Cargo.toml
[workspace]                                  # opt out of any parent workspace

[package]
name = "my_program"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
switch_program_sdk = { path = "../path/to/switch_program_sdk" }

[profile.release]
opt-level = "s"
lto = true
strip = true
```

```sh
rustup target add wasm32-unknown-unknown        # one-time
```

### 1.2 Write the program

```rust
// my_program/src/lib.rs
use switch_program_sdk::*;

pub struct MyProgram {
    switch_id: u32,
    local_ports: Vec<u16>,
}

impl SwitchProgram for MyProgram {
    // `local_ports` are the ports with a link; who is on the other end
    // you learn yourself. Override `init_with(input: InitInput)` instead
    // when you also want `input.port_capacity_bps`, each port's link
    // speed in bits per second, in the same order.
    fn init(switch_id: u32, local_ports: Vec<u16>) -> (Self, ProgramSetup) {
        let mut setup = ProgramSetup::new();

        // Declare data-plane state.
        setup.declare_table(1, MatchKind::Exact, 16);

        // Provide a TinyVM data-plane program. You can write it in
        // text and parse it, or build it programmatically.
        setup.set_program(text::parse_tiny_program("
            stage alu=4 mem=1
                load    r0, ip_dst
                table   t1, r0 -> m0
        ").unwrap());

        (Self { switch_id, local_ports }, setup)
    }

    fn on_punt(&mut self, ev: PuntEvent) -> Vec<Action> {
        // React to a packet that hit `Punt` in the data plane,
        // and/or to packets matching a punted ip_proto.
        // Emit actions through the config pipe:
        vec![actions::install_route(1, 42, ev.ip_dst as u64, 32, 1)]
    }

    fn on_timer(&mut self, ev: TimerEvent) -> Vec<Action> {
        // `ev.now_ns` is the time; `ev.port_drops` lists every port that
        // refused a packet since the previous timer, with how many it
        // refused (`dropped`), how many of those were its own drop
        // threshold at work (`policy`), and the split by queue
        // (`queues`, indexed like the port's `set_queues` list). Ports
        // that dropped nothing are absent.
        for d in &ev.port_drops {
            let _ = (d.port, d.dropped, d.policy, &d.queues);
        }
        // The first timer is scheduled by the host; subsequent timers
        // must be re-armed by the program itself:
        vec![actions::schedule_timer(50_000_000)]   // 50 ms
    }
}

switch_program!(MyProgram);
```

The `switch_program!` macro emits the `extern "C"` exports the host
requires and a static slot holding your program instance.

### 1.3 Build the `.wasm`

```sh
cd my_program
cargo build --release --target wasm32-unknown-unknown
```

The artifact lands at:

```
target/wasm32-unknown-unknown/release/my_program.wasm
```

That is the file the simulator consumes.

### 1.4 What the program can and cannot see at `init`

`init(switch_id, local_ports)` receives:

- `switch_id: u32` — the unique id of the switch this instance is
  running on. The same `.wasm` may be instantiated on many switches; the
  program differentiates per switch by branching on `switch_id`.
- `local_ports: Vec<u16>` — the set of port ids that have a link
  attached on this switch. The program does **not** know who is on the
  other side of each port; it has to learn that (e.g., by sending HELLO
  packets and watching what comes back). It also doesn't know the
  global topology; it has to reconstruct that itself if it needs it.

### 1.5 Actions the program can emit

Every handler returns `Vec<Action>`. Useful constructors live under the
`actions::` module:

```rust
actions::install_route(table_id, entry_id, key, prefix_len, egress_port)
actions::install_entry(table_id, entry)              // arbitrary TableEntry
actions::delete_entry(table_id, entry_id)
actions::inject_packet(port, ip_src, ip_dst, ip_proto, ip_ttl, payload)
actions::schedule_timer(delay_ns)
```

Actions travel through the **config pipe** (`config_pipe_latency`,
`config_pipe_bandwidth_bps`) before they take effect on the data plane,
just like any controller action. `inject_packet` rides the regular
egress link out of the chosen port — it is not free.

---

## 2. Setting up a multi-owner topology

The general flow is:

1. Create a `Simulator`.
2. Add switches, *tagging each one with its owner*.
3. Add apps.
4. Connect everything with links.
5. Read each owner's `.wasm` from disk and call
   `Simulator::install_program(owner, &bytes, limits)`. That call iterates
   over every switch tagged with that owner, instantiates a fresh wasm
   instance per switch, calls `init(switch_id, local_ports)`, and replaces
   the switch's data-plane program, state, and controller with what the
   program returned.
6. Schedule any initial controller timers and run the simulator.

### 2.1 Skeleton

```rust
use std::time::Duration;
use competitive_net_sim::{
    App, Simulator, Switch, SwitchConfig, TinyProgram, TinyVmState,
    TrafficPattern, IpAddr, LinkConfig, AppId, OwnerId, PortId, SwitchId,
};
use competitive_net_sim::controller::NoopController;
use competitive_net_sim::network::Node;
use competitive_net_sim::wasm::WasmLimits;

let mut sim = Simulator::new();

// Owners.
let alice = OwnerId::new(1);
let bob   = OwnerId::new(2);

// --- 1. Switches, each tagged with an owner -----------------------------
//
// `Switch::new` takes a placeholder `TinyProgram` / `TinyVmState` /
// controller — `install_program` will replace all three. `num_ports`
// must be at least one greater than the highest port id you'll ever use
// (including app ports).
let make = |id: u32, owner: OwnerId| {
    Switch::new(
        SwitchConfig::defaults(SwitchId::new(id)),
        TinyProgram { stages: Vec::new() },
        TinyVmState::default(),
        Box::new(NoopController),
        128,        // num_ports
        1 << 16,    // per-port queue bytes
    )
    .with_owner(owner)
};

sim.add_switch(make(0, alice));
sim.add_switch(make(1, alice));
sim.add_switch(make(2, alice));
sim.add_switch(make(3, bob));
sim.add_switch(make(4, bob));

// --- 2. Apps ------------------------------------------------------------
let app_a = AppId::new(100);
let app_b = AppId::new(101);
sim.add_app(App::new(
    app_a,
    IpAddr(0x0a000001),
    IpAddr(0x0a000002),
    TrafficPattern::ConstantBitrate {
        interval: Duration::from_millis(5),
        size_bytes: 64,
    },
));
sim.add_app(App::new(
    app_b,
    IpAddr(0x0a000002),
    IpAddr(0x0a000001),
    TrafficPattern::ConstantBitrate {
        interval: Duration::from_millis(5),
        size_bytes: 64,
    },
));

// --- 3. Links -----------------------------------------------------------
let lc = LinkConfig {
    latency:              Duration::from_micros(100),
    bandwidth_bps:        1_000_000_000,
    queue_capacity_bytes: 1 << 16,
};
// switch <-> switch (within Alice's island)
sim.connect(Node::Switch(SwitchId::new(0)), PortId::new(1),
            Node::Switch(SwitchId::new(1)), PortId::new(1), lc);
sim.connect(Node::Switch(SwitchId::new(1)), PortId::new(2),
            Node::Switch(SwitchId::new(2)), PortId::new(2), lc);
// Alice <-> Bob border
sim.connect(Node::Switch(SwitchId::new(2)), PortId::new(3),
            Node::Switch(SwitchId::new(3)), PortId::new(3), lc);
// Bob's island
sim.connect(Node::Switch(SwitchId::new(3)), PortId::new(4),
            Node::Switch(SwitchId::new(4)), PortId::new(4), lc);
// Apps attach to switches via the conventional "app port".
sim.connect(Node::App(app_a), PortId::new(0),
            Node::Switch(SwitchId::new(0)), PortId::new(100), lc);
sim.connect(Node::App(app_b), PortId::new(0),
            Node::Switch(SwitchId::new(4)), PortId::new(100), lc);

// --- 4. Install each owner's program -----------------------------------
let alice_wasm = std::fs::read("alice_program/target/wasm32-unknown-unknown/release/alice_program.wasm")?;
let bob_wasm   = std::fs::read("bob_program/target/wasm32-unknown-unknown/release/bob_program.wasm")?;

sim.install_program(alice, &alice_wasm, WasmLimits::default())?;
sim.install_program(bob,   &bob_wasm,   WasmLimits::default())?;
```

`install_program` is atomic per call: every targeted switch is loaded
first; only if every load succeeds are any of them swapped in. If any
owner's program fails to compile or any switch's `init` returns an
error, the call returns `Err` and that owner's switches are left
untouched. (Other owners' switches that have already been programmed
stay programmed — `install_program` calls are independent.)

### 2.2 Bootstrap and run

If your programs use timers, schedule the first one. The host fires
exactly the timers you ask for; programs reschedule themselves via
`actions::schedule_timer`.

```rust
for sid in 0..5 {
    sim.schedule_controller_timer(SwitchId::new(sid), Duration::ZERO);
}

sim.run_until(Duration::from_secs(2));

// Read out per-app metrics.
for id in [app_a, app_b] {
    let m = &sim.apps[&id].metrics;
    println!(
        "app {}: sent {} recv {} avg-delay {:?}",
        id.raw(), m.packets_sent, m.packets_received, m.avg_delay,
    );
}
```

`run_until(deadline)` drains the event queue until either the deadline
passes or the queue empties. `run()` runs to queue exhaustion.

### 2.3 Inducing failures during the run

```rust
let link = sim.connect(/* ... */);
sim.run_until(Duration::from_millis(500));
sim.fail_link(link);                            // drops new enqueues
sim.run_until(Duration::from_millis(1500));
sim.restore_link(link);
sim.run_until(Duration::from_secs(3));
```

`fail_link` flips a per-link flag (already-queued packets still drain)
and fires `LinkEvent::Down` on each switch endpoint. `restore_link` is
the reverse. Programs that don't subscribe to link events still detect
failures via timeout-based mechanisms (e.g., HELLO loss).

You can also fail the controller alone (`fail_cpu`) or the whole switch
(`fail_switch`).

---

## 3. Working examples

Two complete, working programs you can copy:

- `examples/hello_switch/` — the starting point. It compiles, declares a
  table, runs, forwards nothing, and scores 0%. Read it for the shape of a
  program, then make it actually route.

- `examples/learning_switch/` — minimal "install one route on first punt"
  program. Driven by `tests/wasm_integration.rs`, which builds the wasm
  on demand and exercises a 2-switch topology under one owner.

That test shows the full pipeline end-to-end: build the wasm, wire the
topology, install the program, run, assert.

---

## 4. TinyVM text format

The SDK exposes `text::parse_tiny_program(&str) -> Result<TinyProgram, _>`.
One instruction per line, blank lines and `# ...` comments ignored. Stages
are introduced by a `stage` line with optional `alu=N mem=M` attributes.

```
stage alu=8 mem=1
    load    r0, ip_dst       # field reads
    const   r1, 0x0a000002
    eq      r2, r0, r1
    bif     r2 -> 6           # forward branch only
    drop
    noop
    egress  1
```

Supported fields: `ip_src`, `ip_dst`, `ip_proto`, `ip_ttl` (alias `ttl`),
`ip_dscp` (alias `dscp`), `src_port`,
`dst_port`, `app_id`, `flow_id`, `size`, `kind`, `custom0..custom3`,
`label_top`, `label_depth`.

Opcodes: `load`, `store`, `loadm`, `storem`, `const`, `add`, `sub`, `and`,
`or`, `xor`, `eq`, `lt`, `table`, `rread`, `rwrite`, `counter`, `bif`,
`drop`, `punt`, `egress`, `queue`, `recirc`, `noop`,
`push_label <imm>`, `pop_label`, `swap_label <imm>`, `mark_trace`.

### MPLS labels

Each packet carries a label stack (a `Vec<u32>`, top of stack is the most
recently pushed). The simulator imposes no MPLS semantics — it just gives
you primitives. `push_label` and `swap_label` take an immediate label
value; `pop_label` is a no-op on an empty stack. The data plane can read
the top label via `label_top` and the depth via `label_depth`.

### `mark_trace`

Setting `mark_trace` on a packet asks the simulator to deliver a
`TraceReply` back to *this* switch when the packet eventually
terminates. The reply contains the perfect switch-by-switch path the
packet took. The reply consumes bandwidth on the link the packet was
actually forwarded on (i.e. on the marking switch's chosen egress port);
when it arrives at the marking switch it is delivered directly to the
controller as a punt event with `PuntReason::Custom(0xFFFF_FFFE)` and
`packet.kind == PacketKind::TraceReply`. The payload is a postcard-encoded
`TraceReplyW`. Marking is free at the data plane — no extra bytes on the
original packet.

The TinyVM validator runs after `init`: programs that loop, exceed
budgets, or reference undeclared resources are rejected.

---

## 5. Raw ABI (any language)

If you don't want to use the Rust SDK, implement these wasm exports
against an exported linear `memory`:

```text
init(in_ptr, in_len, out_ptr, out_cap) -> i32
on_punt(in_ptr, in_len, out_ptr, out_cap) -> i32
on_timer(in_ptr, in_len, out_ptr, out_cap) -> i32
on_link_event(in_ptr, in_len, out_ptr, out_cap) -> i32
```

`on_link_event` must exist — the host refuses to load a module without it —
but the simulator never calls it. Return 0.

Each export reads its postcard-encoded input from `[in_ptr, in_ptr+in_len)`,
writes its postcard-encoded output into `[out_ptr, out_ptr+out_cap)`, and
returns the number of bytes written, or a negative value on error.

| Export          | Input          | Output                    |
|-----------------|----------------|---------------------------|
| `init`          | `InitInputW` (`switch_id`, `local_ports`, `port_capacity_bps`) | `ProgramSetupW` |
| `on_punt`       | `PuntEventW`   | `Vec<ActionW>`            |
| `on_timer`      | `TimerEventW` (`now_ns`, `port_drops: Vec<PortDropsW>`) | `Vec<ActionW>` |
| `on_link_event` | `LinkEventW`   | `Vec<ActionW>`            |

The wire types are defined in `switch_program_types/src/lib.rs`. Postcard
is a stable, compact binary format; non-Rust implementations can target it
directly or generate bindings from the Serde-derive structs.

---

## 6. Failure and resource model

The host meters fuel per call (`WasmLimits.fuel_per_call`), bounds output
size (`max_output_bytes`), and catches traps. Any of:

- a wasm trap
- fuel exhaustion
- a negative return value
- a postcard decode failure on the output

…marks the program as *failed*. The data plane keeps running with
whatever state it had at the moment of failure; controller events stop
being dispatched. This matches the existing `Switch::cpu.failed`
semantic, and is independent of `Simulator::fail_switch` (which stops
the data plane too).

Each owner's program runs in its own sandboxed wasm instance per switch.
One owner cannot read or corrupt another owner's state — they only
interact through packets on the wire.
