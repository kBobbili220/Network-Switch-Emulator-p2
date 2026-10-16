//! Integration test for the optional event log.
//!
//! Builds a small two-app/one-switch topology, runs it twice (once with
//! logging, once without), checks that:
//!
//! 1. The log file exists and decodes cleanly.
//! 2. The first frame is the magic Header, followed by topology
//!    declarations (AppAdded, SwitchAdded, LinkAdded).
//! 3. The number of `PacketIngress` events at the switch matches the
//!    number of packets the apps sent.
//! 4. App metrics are byte-identical between the logged and un-logged
//!    runs (logging does not change simulation outcomes).

use std::fs;
use std::time::Duration;

use competitive_net_sim::app::{App, TrafficPattern};
use competitive_net_sim::controller::NoopController;
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::{IpAddr, PacketField};
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::sim_log::{self, LogEvent};
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{
    Instr, MatchActionTable, MatchKind, StageProgram, TableAction, TableEntry, TinyProgram,
    TinyVmState,
};
use competitive_net_sim::types::{AppId, EntryId, MetaKey, PortId, Reg, SwitchId, TableId};

fn build_topology() -> (Simulator, AppId, AppId, SwitchId) {
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let switch_id = SwitchId::new(0);

    let a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_millis(10),
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
    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    table.install(TableEntry {
        id: EntryId::new(1),
        key: 0x0a000002,
        prefix_len: 32,
        priority: 0,
        action: TableAction::SetEgress { port: PortId::new(1) },
    }).unwrap();
    state.tables.push(table);
    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField { dst: Reg::new(0), field: PacketField::IpDst },
                Instr::TableLookup {
                    table_id: TableId::new(1),
                    key_reg: Reg::new(0),
                    result_meta: MetaKey::new(0),
                },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 1,
        }],
    };
    let switch = Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        Box::new(NoopController),
        4,
        1 << 16,
    );

    sim.add_switch(switch);
    sim.add_app(a);
    sim.add_app(b);

    let lc = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(app_a), PortId::new(0), Node::Switch(switch_id), PortId::new(0), lc);
    sim.connect(Node::Switch(switch_id), PortId::new(1), Node::App(app_b), PortId::new(0), lc);

    (sim, app_a, app_b, switch_id)
}

#[test]
fn logged_run_produces_decodable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.simlog");
    let (mut sim, _a, _b, _s) = build_topology();
    sim.start_logging(&path).unwrap();
    sim.run_until(Duration::from_millis(50));
    drop(sim); // forces the writer to flush via Drop

    let bytes = fs::read(&path).unwrap();
    assert!(!bytes.is_empty(), "log file should have content");
    let frames = sim_log::writer::decode_all(&bytes).unwrap();

    // First frame: header.
    assert!(matches!(
        frames[0].event,
        LogEvent::Header { magic: sim_log::LOG_MAGIC, version: sim_log::LOG_VERSION }
    ));
    // Topology declarations should follow.
    let mut saw_switch = false;
    let mut saw_app_a = false;
    let mut saw_link = false;
    for f in &frames[1..6] {
        match &f.event {
            LogEvent::SwitchAdded { id: 0, .. } => saw_switch = true,
            LogEvent::AppAdded { id: 1, .. } => saw_app_a = true,
            LogEvent::LinkAdded { .. } => saw_link = true,
            _ => {}
        }
    }
    assert!(saw_switch && saw_app_a && saw_link, "missing topology declarations");
}

#[test]
fn logged_packet_ingress_count_matches_send_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("count.simlog");
    let (mut sim, app_a, _b, sid) = build_topology();
    sim.start_logging(&path).unwrap();
    sim.run_until(Duration::from_millis(100));
    let sent = sim.apps[&app_a].metrics.packets_sent;
    drop(sim);

    let bytes = fs::read(&path).unwrap();
    let frames = sim_log::writer::decode_all(&bytes).unwrap();
    let ingress: u64 = frames
        .iter()
        .filter(|f| {
            matches!(&f.event, LogEvent::PacketIngress { switch, .. } if *switch == sid.raw())
        })
        .count() as u64;
    // Every sent packet (app A -> switch) should produce an ingress at S0,
    // possibly minus one packet still in flight at the deadline.
    assert!(
        ingress + 1 >= sent && ingress <= sent,
        "expected ~{sent} ingress events at S0, found {ingress}",
    );
}

#[test]
fn logging_does_not_change_simulation() {
    // Run twice — once with logging, once without — and verify the
    // app-side metrics agree exactly.
    let (mut sim_a, app_a, app_b, _) = build_topology();
    sim_a.run_until(Duration::from_millis(200));

    let dir = tempfile::tempdir().unwrap();
    let (mut sim_b, _, _, _) = build_topology();
    sim_b.start_logging(&dir.path().join("x.simlog")).unwrap();
    sim_b.run_until(Duration::from_millis(200));

    let m_a_unlogged = &sim_a.apps[&app_a].metrics;
    let m_a_logged = &sim_b.apps[&app_a].metrics;
    let m_b_unlogged = &sim_a.apps[&app_b].metrics;
    let m_b_logged = &sim_b.apps[&app_b].metrics;

    assert_eq!(m_a_unlogged.packets_sent, m_a_logged.packets_sent);
    assert_eq!(m_b_unlogged.packets_received, m_b_logged.packets_received);
    assert_eq!(m_b_unlogged.avg_delay, m_b_logged.avg_delay);
}
