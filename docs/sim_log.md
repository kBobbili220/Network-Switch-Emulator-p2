# Event log + browser viewer

The simulator can write a structured event log to disk, and a separate
browser-based viewer can replay it interactively.

## Recording a log

Logging is opt-in. The CLI exposes it on the `simulate` subcommand:

```sh
cargo run -- simulate --log /tmp/demo.simlog --until-ms 200
```

Programmatically:

```rust
let mut sim = Simulator::new();
sim.start_logging(std::path::Path::new("run.simlog"))?;
// ... add switches/apps/links, run as usual ...
```

For integration tests that take that pattern, the convention is an
environment variable:

```sh
ROUTING_SIMLOG=/tmp/routing.simlog \
    cargo test --features wasm --test routing_uptime
```

When set, the test calls `start_logging` with that path. Unset, the
test runs with no logging overhead.

`start_logging` is safe to call after the topology has been built — the
existing apps, switches, links, and (if present) the BGP registry are
backfilled into the log so the file is self-contained.

The hot-path cost when logging is **not** enabled is one
`Option::is_none()` check per hook site.

## File format

```
[u32 length LE][postcard(LogFrame)]
[u32 length LE][postcard(LogFrame)]
...
```

The first frame is always `LogEvent::Header { magic: 0x4E45_534D, version: 3 }`.
Subsequent frames are ordered by simulator time. Version 2 appended the
`QueueFull` / `Policy` drop reasons and the queue and declaration events;
version 3 added `flow_id` and `seq` to `PacketSnapshot`, which a reader
built for version 3 needs, so version 1 and 2 logs no longer decode.

`LogFrame { at_ns: u64, event: LogEvent }` and the `LogEvent` enum live
in [`src/sim_log/schema.rs`](../src/sim_log/schema.rs). Events:

| Event                  | When                                                        |
|------------------------|-------------------------------------------------------------|
| `Header`               | First frame in every file.                                  |
| `AppAdded`             | `Simulator::add_app`                                        |
| `SwitchAdded`          | `Simulator::add_switch`                                     |
| `LinkAdded`            | `Simulator::connect`                                        |
| `BgpConfigured`        | `Simulator::configure_bgp`                                  |
| `LinkFailed/Restored`  | `Simulator::fail_link` / `restore_link`                     |
| `SwitchFailed`         | `Simulator::fail_switch`                                    |
| `CpuFailed`            | `Simulator::fail_cpu`                                       |
| `PacketIngress`        | every switch ingress, after trail stamping                  |
| `PacketEgress`         | a packet starts onto the wire out of a switch port           |
| `PacketDropped`        | every drop, with `DropReason` (see below)                    |
| `PacketDelivered`      | when a packet reaches an app                                 |
| `PacketPunted`         | when a packet hits the controller                            |
| `ProgramInstalled`     | initial snapshot via `Simulator::install_program`            |
| `TableEntryInstalled`  | controller action arrives via the config pipe                |
| `TableEntryDeleted`    | ditto                                                        |
| `PortQueuesConfigured` | controller `SetQueueConfig` arrives: discipline + queues in packets, and the byte size of one packet |
| `LinkDeclared`         | controller `DeclareLink` arrives: what the switch now *believes* about the link on a port (`up`); compare with `LinkFailed/Restored`, the truth |
| `QueueConfigChanged`   | pre-version-2 form of the above; no longer written           |

`DropReason` says why a packet died: `Ttl`, `NoRoute` (punted with no
route and the controller had none either), `NoEgressLink` (forwarded out
a port with no cable), `LinkDrop` (the link was down), `SwitchFailed`,
`Recirculation` (hit the recirculation limit), `QueueFull` (the egress
queue or the switch's buffer pool had no room), and `Policy` (the switch
program executed `drop`). Logs before version 2 reported a full queue as
`LinkDrop` and a program drop as `NoRoute`.

`PacketSnapshot` carries id / kind / size / ip / ttl / label-stack
summary, but **not** the payload bytes — those would balloon the log
and would expose BGP/trace internals to a generic log consumer.

## Viewer

The viewer is a separate Cargo crate at `viewer/`, compiled to wasm and
loaded by a static HTML page.

### One-time setup

```sh
cargo install wasm-pack
```

### Build

```sh
cd viewer
./build.sh
```

This produces `viewer/web/pkg/` containing `viewer.js` and
`viewer_bg.wasm`. The wasm bundle itself is ~120 KB; gitignored.

### Run

```sh
cd viewer/web
python3 -m http.server 8080
open http://localhost:8080
```

Drag a `.simlog` onto the drop zone. The page lays out the topology on a
canvas: switches in a ring, apps near their attached switch. Press play
to animate packets. Use the speed selector (0.1× … 1000×) to scale time.

Above the per-link `summary @` threshold — packets-per-link-per-frame —
the viewer collapses individual packets into a heat-shaded summary
showing pkt/s and bps. Reduce playback speed (or raise the threshold) to
see individual packets again.

### Pause + inspect

While paused, click any switch to open the inspector pane. It shows:

- The TinyVM program currently installed at that switch, pretty-printed
  in the same text format the simulator's `text::parse_tiny_program`
  understands.
- Every match-action table with its current entries (key, prefix length,
  action), folded forward from the most recent `ProgramInstalled` plus
  every `TableEntryInstalled` / `TableEntryDeleted` up to the cursor.
- Register and counter array declarations.

The inspector reflects exactly what the data plane would see at the
cursor's timestamp.

### What's drawn

| State                  | Rendering                                  |
|------------------------|--------------------------------------------|
| Switch                 | Green disk, `S<id>` label                   |
| Switch (CPU failed)    | Yellow disk                                 |
| Switch (whole failed)  | Grey disk                                   |
| Link                   | Solid 2px line                              |
| Link (failed)          | Dashed 1px line                             |
| Data packet            | White dot moving along link                 |
| SimpleBGP packet       | Yellow dot                                  |
| TraceReply             | Blue dot                                    |
| Selected switch        | White outline                               |
| High-density link      | Heat-shaded badge with pkt/s + bps          |
