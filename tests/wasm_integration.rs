//! End-to-end test: compile the `examples/learning_switch` SDK program to
//! wasm, load it via `Simulator::install_program`, and run a topology where
//! the switches' behavior is differentiated by `switch_id` inside one
//! owner-supplied program.

#![cfg(feature = "wasm")]

use std::process::Command;
use std::time::Duration;

use competitive_net_sim::app::{App, TrafficPattern};
use competitive_net_sim::controller::NoopController;
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::IpAddr;
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{TinyProgram, TinyVmState};
use competitive_net_sim::types::{AppId, OwnerId, PortId, SwitchId};
use competitive_net_sim::wasm::WasmLimits;

fn build_example_wasm() -> std::path::PathBuf {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/learning_switch/Cargo.toml"
    );
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--manifest-path",
            manifest,
        ])
        .status()
        .expect("cargo build of example program");
    assert!(status.success(), "example wasm build failed");

    std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/learning_switch/target/wasm32-unknown-unknown/release/learning_switch.wasm"
    ))
}

fn make_blank_switch(switch_id: SwitchId, owner: OwnerId) -> Switch {
    // Placeholder program/state — install_program replaces both.
    Switch::new(
        SwitchConfig::defaults(switch_id),
        TinyProgram { stages: Vec::new() },
        TinyVmState::default(),
        Box::new(NoopController),
        4,
        1 << 16,
    )
    .with_owner(owner)
}

#[test]
fn owner_program_drives_two_switches_with_per_switch_behavior() {
    let wasm = std::fs::read(build_example_wasm()).expect("read built wasm");

    // Topology: A --- s0 --- s1 --- B
    // Both switches owned by owner 1; s0 must learn to forward on port 1,
    // s1 must learn to forward on port 2 (per the program's switch_id branch).
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let s0 = SwitchId::new(0);
    let s1 = SwitchId::new(1);
    let owner = OwnerId::new(1);

    sim.add_switch(make_blank_switch(s0, owner));
    sim.add_switch(make_blank_switch(s1, owner));

    let a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_millis(5),
            size_bytes: 100,
        },
    );
    let b = App::new(
        app_b,
        IpAddr(0x0a000002), 32,
        IpAddr(0x0a000001), 32,
        TrafficPattern::BulkTransfer {
            total_bytes: 0,
            sent_bytes: 0,
            pacing: Duration::ZERO,
            size_bytes: 0,
        },
    );
    sim.add_app(a);
    sim.add_app(b);

    let lc = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(app_a), PortId::new(0), Node::Switch(s0), PortId::new(0), lc);
    // s0 port 1 -> s1 port 1 (so s0 should learn egress=1)
    sim.connect(Node::Switch(s0), PortId::new(1), Node::Switch(s1), PortId::new(1), lc);
    // s1 port 2 -> B (so s1 should learn egress=2)
    sim.connect(Node::Switch(s1), PortId::new(2), Node::App(app_b), PortId::new(0), lc);

    let installed = sim
        .install_program(owner, &wasm, WasmLimits::default())
        .expect("install_program");
    assert_eq!(installed.len(), 2);

    // Sanity: init populated each switch's program + state.
    for sid in [s0, s1] {
        let sw = sim.switches.get(&sid).unwrap();
        assert_eq!(sw.program.stages.len(), 1);
        assert_eq!(sw.state.tables.len(), 1);
    }

    sim.run_until(Duration::from_millis(500));

    // Each switch should have learned a route differentiated by switch_id.
    let s0_entry = &sim.switches[&s0].state.tables[0].entries[0];
    let s1_entry = &sim.switches[&s1].state.tables[0].entries[0];
    assert!(matches!(
        s0_entry.action,
        competitive_net_sim::TableAction::SetEgress { port } if port.raw() == 1
    ));
    assert!(matches!(
        s1_entry.action,
        competitive_net_sim::TableAction::SetEgress { port } if port.raw() == 2
    ));

    let recv = sim.apps[&app_b].metrics.packets_received;
    assert!(
        recv > 0,
        "App B should have received packets after both switches learned routes (got {recv})"
    );
}
