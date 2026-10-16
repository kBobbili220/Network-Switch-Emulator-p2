//! Real WASM regressions: machine limits, packet paths, and A1 recovery.
#![cfg(feature = "wasm")]

use competitive_net_sim::{
    packet::{IpAddr, Packet, PacketKind},
    sim::Simulator,
    sim_log::{self, LogEvent},
    tinyvm::{PipelineResult, StageDecision, run_stage},
    types::{OwnerId, SwitchId},
    wasm::WasmLimits,
    world::World,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
    sync::OnceLock,
    time::Duration,
};

fn wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(
            Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--release",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--manifest-path"
                ])
                .arg(root.join("examples/fair_switch/Cargo.toml"))
                .status()
                .unwrap()
                .success()
        );
        std::fs::read(
            root.join(
                "examples/fair_switch/target/wasm32-unknown-unknown/release/fair_switch.wasm",
            ),
        )
        .unwrap()
    })
}

fn world(n: u32, mixed: bool) -> World {
    let mut text = format!(
        r#"
[world]
name = "fair-switch-regression"
[params]
link_capacity_mbps = 10
link_latency_us = 500
queue_capacity_bytes = 40000
buffer_budget_packets = 40
packet_size_bytes = 1000
control_delay_us = 2000
duration_ms = 12000
warmup_ms = 3000
[workload]
kind = "{}"
size_bytes = 200
{}
"#,
        if mixed { "mixed" } else { "all_pairs_cbr" },
        if mixed { "" } else { "interval_ms = 20" }
    );
    for i in 0..n {
        text.push_str(&format!("\n[[switch]]\nid = {i}\n"));
    }
    let mut edges = BTreeSet::new();
    for i in 0..n {
        for offset in [1, 3] {
            let j = (i + offset) % n;
            if i != j {
                edges.insert((i.min(j), i.max(j)));
            }
        }
    }
    for (a, b) in edges {
        text.push_str(&format!("\n[[link]]\na = {a}\nb = {b}\n"));
    }
    for i in 0..n {
        text.push_str(&format!(
            "\n[[app]]\nid = {}\nswitch = {i}\nprefix = \"10.9.{i}.0/24\"\n",
            100 + i
        ));
        if mixed {
            text.push_str(&format!("preset = \"fortnite\"\nlaw = \"{}\"\nrate_mbps = 0.4\npeer = {}\nsize_bytes = 200\n", if i % 2 == 0 { "aimd" } else { "rate_match" }, 100 + (i + n / 2) % n));
        }
    }
    World::from_str(&text).unwrap()
}

fn install(
    world: &World,
    log: &std::path::Path,
) -> (Simulator, competitive_net_sim::world::WorldHandles) {
    let mut sim = Simulator::new();
    sim.start_logging(log).unwrap();
    let handles = world.build(&mut sim).unwrap();
    sim.install_program(OwnerId::new(0), wasm(), WasmLimits::default())
        .unwrap();
    for s in &world.switches {
        sim.schedule_controller_timer(SwitchId::new(s.id), Duration::ZERO);
    }
    (sim, handles)
}

fn check_trace(sim: &mut Simulator, path: &std::path::Path) -> Vec<(u64, u32, u16, bool)> {
    for (id, switch) in &sim.switches {
        assert!(
            switch.controller.failure().is_none(),
            "switch {id:?}: {:?}",
            switch.controller.failure()
        );
    }
    drop(sim.log.take());
    let mut visited = BTreeMap::<u64, BTreeSet<u32>>::new();
    let mut declarations = Vec::new();
    sim_log::writer::for_each_frame(path, |f| match f.event {
        LogEvent::PacketIngress { switch, packet, .. } if packet.ip_proto == 0 => {
            assert!(
                visited.entry(packet.id).or_default().insert(switch),
                "packet {} revisited {switch}",
                packet.id
            );
        }
        LogEvent::LinkDeclared { switch, port, up } => {
            declarations.push((f.at_ns, switch, port, up))
        }
        _ => {}
    })
    .unwrap();
    declarations
}

#[test]
fn ipv4_prefixes_isolate_data_and_preserve_control_queues() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sim, _) = install(&world(15, true), &dir.path().join("prefixes.simlog"));
    let customers = sim.customer_contract.clone();
    assert_eq!(customers.len(), 15);
    for switch in sim.switches.values_mut() {
        for (i, customer) in customers.iter().enumerate() {
            assert_eq!(customer.prefix_len, 24);
            // Probe two real host addresses, rather than the prefix's network address.
            for host in [1, 254] {
                for (kind, protocol, expected_queue) in [
                    (PacketKind::Data, 0, i as u16 + 1),
                    (PacketKind::Ack, 0, 0),
                    (PacketKind::Data, 89, 0),
                    (PacketKind::Data, 90, 0),
                ] {
                    let mut packet = Packet::new(Default::default(), Duration::ZERO, 200);
                    packet.ip_src = IpAddr(customer.prefix_addr | host);
                    packet.kind = kind;
                    packet.ip_proto = protocol;
                    packet.custom[0] = 1; // Already tagged transit packet; no route learning needed.
                    let mut result = PipelineResult::default();
                    for stage in &switch.program.stages {
                        if !matches!(
                            run_stage(stage, &mut packet, &mut switch.state, &mut result),
                            StageDecision::Continue
                        ) {
                            break;
                        }
                    }
                    assert_eq!(
                        result.queue.unwrap().raw(),
                        expected_queue,
                        "source={:?}, kind={kind:?}, protocol={protocol}",
                        packet.ip_src
                    );
                }
            }
        }
    }
}

#[test]
fn fifteen_customers_fit_fuel_buffer_and_queue_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.simlog");
    let (mut sim, _) = install(&world(15, true), &path);
    sim.run_until(Duration::from_secs(8));
    for switch in sim.switches.values() {
        assert!(
            switch.controller.failure().is_none(),
            "{:?}",
            switch.controller.failure()
        );
    }
    let before: BTreeMap<_, _> = sim
        .apps
        .iter()
        .map(|(&id, app)| (id, app.metrics.packets_received))
        .collect();
    sim.run_until(Duration::from_secs(10));
    for (&id, app) in &sim.apps {
        assert!(
            app.metrics.packets_received > before[&id],
            "no steady-state delivery to {id:?}"
        );
    }
    for switch in sim.switches.values() {
        assert!(switch.buffer_used <= 40_000);
    }
    assert!(
        check_trace(&mut sim, &path).iter().all(|d| d.3),
        "false link-down"
    );
}

#[test]
fn calm_a1_network_recovers_from_failure_and_restoration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.simlog");
    let (mut sim, handles) = install(&world(5, false), &path);
    sim.run_until(Duration::from_secs(3));
    let link = handles.links[&(0, 1)];
    sim.fail_link(link);
    sim.run_until(Duration::from_secs(5));
    let before: BTreeMap<_, _> = sim
        .apps
        .iter()
        .map(|(&id, app)| (id, app.metrics.packets_received))
        .collect();
    sim.run_until(Duration::from_secs(6));
    for (&id, app) in &sim.apps {
        assert!(
            app.metrics.packets_received > before[&id],
            "no delivery during failure to {id:?}"
        );
    }
    sim.restore_link(link);
    sim.run_until(Duration::from_secs(9));
    let declarations = check_trace(&mut sim, &path);
    assert!(declarations.iter().any(|d| !d.3), "failure never detected");
    for &(at, switch, port, up) in &declarations {
        if !up {
            assert!(
                declarations
                    .iter()
                    .any(|d| d.0 > at && d.1 == switch && d.2 == port && d.3),
                "missing restoration declaration"
            );
        }
    }
}
