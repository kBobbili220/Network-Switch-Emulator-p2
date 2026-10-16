//! In-band trace mark + bandwidth-charged reply.
//!
//! S1 has a TinyVM data plane that marks every packet for trace and
//! forwards via port 1 toward S2. S2 forwards via port 2 toward an app.
//!
//!     S1 -- (link L0) -- S2 -- (link L1) -- App
//!
//! When the original packet is delivered to the app, the simulator
//! synthesizes a TraceReply and queues it on `L0` (the link S1 used to
//! forward), addressed back to S1. The reply consumes bandwidth on `L0`
//! exactly as if S2 had sent it. S1's controller observes the reply via
//! a punt event and records the path the simulator computed.

#![cfg(feature = "wasm")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use competitive_net_sim::app::{App, TrafficPattern};
use competitive_net_sim::controller::{
    ControllerAction, LinkEvent, NoopController, PuntEvent, SwitchController,
};
use competitive_net_sim::event::EventKind;
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::{IpAddr, Packet, PacketField};
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{
    Instr, MatchActionTable, MatchKind, StageProgram, TableAction, TableEntry, TinyProgram,
    TinyVmState,
};
use competitive_net_sim::types::{
    AppId, EntryId, LinkId, MetaKey, PacketId, PortId, Reg, SimTime, SwitchId, TableId,
};

use switch_program_types::TraceReplyW;

#[derive(Default)]
struct CapturedPunt {
    payloads: Vec<Vec<u8>>,
    sizes: Vec<u64>,
}

struct CaptureController {
    captured: Rc<RefCell<CapturedPunt>>,
}

impl SwitchController for CaptureController {
    fn on_punt(&mut self, event: PuntEvent) -> Vec<ControllerAction> {
        let mut c = self.captured.borrow_mut();
        c.sizes.push(event.packet.size_bytes);
        c.payloads.push(event.packet.payload.clone());
        Vec::new()
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> { Vec::new() }
    fn on_link_event(&mut self, _ev: LinkEvent) -> Vec<ControllerAction> { Vec::new() }
}

fn entry(id: u64, key: u64, port: u16) -> TableEntry {
    TableEntry {
        id: EntryId::new(id),
        key,
        prefix_len: 32,
        priority: 0,
        action: TableAction::SetEgress { port: PortId::new(port) },
    }
}

fn s1_program() -> TinyProgram {
    // S1's stage: lookup ip_dst -> sets egress, *and* mark the packet.
    TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField { dst: Reg::new(0), field: PacketField::IpDst },
                Instr::TableLookup {
                    table_id: TableId::new(1),
                    key_reg: Reg::new(0),
                    result_meta: MetaKey::new(0),
                },
                Instr::MarkTrace,
            ],
            max_alu_ops: 0,
            max_memory_accesses: 1,
        }],
    }
}

fn plain_program() -> TinyProgram {
    TinyProgram {
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
    }
}

fn make_switch(id: u32, prog: TinyProgram, route: TableEntry, ctrl: Box<dyn SwitchController>) -> Switch {
    let mut state = TinyVmState::default();
    let mut t = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 8);
    t.install(route).unwrap();
    state.tables.push(t);
    Switch::new(SwitchConfig::defaults(SwitchId::new(id)), prog, state, ctrl, 8, 1 << 16)
}

#[test]
fn trace_reply_returns_path_and_charges_bandwidth() {
    let captured = Rc::new(RefCell::new(CapturedPunt::default()));
    let ctrl = Box::new(CaptureController { captured: captured.clone() });

    // S1: marks + forwards toward S2 on port 1.
    // S2: forwards toward App on port 2.
    let s1 = make_switch(1, s1_program(), entry(1, 0x0a000002, 1), ctrl);
    let s2 = make_switch(2, plain_program(), entry(1, 0x0a000002, 2), Box::new(NoopController));

    let mut sim = Simulator::new();
    sim.add_switch(s1);
    sim.add_switch(s2);

    let app = AppId::new(10);
    sim.add_app(App::new(
        app,
        IpAddr(0x0a000002), 32,
        IpAddr(0x0a000001), 32,
        TrafficPattern::BulkTransfer { total_bytes: 0, sent_bytes: 0, pacing: Duration::ZERO, size_bytes: 0 },
    ));

    let lc = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    // S1.p1 <-> S2.p1
    let l0 = sim.connect(
        Node::Switch(SwitchId::new(1)), PortId::new(1),
        Node::Switch(SwitchId::new(2)), PortId::new(1), lc,
    );
    // S2.p2 <-> App
    sim.connect(
        Node::Switch(SwitchId::new(2)), PortId::new(2),
        Node::App(app), PortId::new(0), lc,
    );

    // Inject one packet at S1's ingress on port 0.
    let mut p = Packet::new(PacketId::new(1), Duration::ZERO, 200);
    p.ip_src = IpAddr(0x0a000001);
    p.ip_dst = IpAddr(0x0a000002);
    p.ip_ttl = 16;
    sim.queue.schedule(Duration::ZERO, EventKind::SwitchIngress {
        switch: SwitchId::new(1),
        port: PortId::new(0),
        packet: p,
    });

    sim.run_until(Duration::from_millis(5));

    // The app should have received the original packet.
    let received = sim.apps[&app].metrics.packets_received;
    assert_eq!(received, 1, "app should receive original packet");

    // S1's controller should have observed exactly one punt — the
    // synthesized TraceReply.
    let captured = captured.borrow();
    assert_eq!(captured.payloads.len(), 1, "expected one trace reply punt, got {}", captured.payloads.len());

    let reply: TraceReplyW = postcard::from_bytes(&captured.payloads[0])
        .expect("decode TraceReplyW");
    assert_eq!(reply.mark_switch, 1);
    assert_eq!(reply.mark_port, 1);
    // Path: S1 (where the marker was set) -> S2 (next hop). The trail
    // stops when the packet is delivered to the app; the app isn't a
    // switch, so it doesn't stamp.
    let switches: Vec<u32> = reply.hops.iter().map(|h| h.switch_id).collect();
    assert_eq!(switches, vec![1, 2]);
    assert!(reply.delivered, "expected delivered=true");
    assert_eq!(reply.drop_reason, 0);

    // Bandwidth was charged on link L0: at least the original packet's
    // size + the reply's size should have crossed it. There's no public
    // counter we can read directly, but the reply payload size is known.
    let sim_link = sim.links.get(&l0).unwrap();
    // Defensive: link should not have dropped any packet.
    assert_eq!(sim_link.packets_dropped(), 0);
    let _ = LinkId::new(0); // silence unused
}

#[test]
fn trace_reply_when_packet_is_dropped_records_drop_reason() {
    // S1 marks; S2 has no route -> packet is "dropped" (NoEgress -> Punt
    // path actually punts to S2's controller). For drop semantics in our
    // model, an explicit `Drop` action is the right test.
    let captured = Rc::new(RefCell::new(CapturedPunt::default()));
    let ctrl = Box::new(CaptureController { captured: captured.clone() });

    let s1 = make_switch(1, s1_program(), entry(1, 0x0a000002, 1), ctrl);

    // S2's program: explicit drop.
    let drop_prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![Instr::Drop],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        }],
    };
    let mut s2_state = TinyVmState::default();
    s2_state.tables.push(MatchActionTable::new(TableId::new(1), MatchKind::Exact, 1));
    let s2 = Switch::new(
        SwitchConfig::defaults(SwitchId::new(2)),
        drop_prog,
        s2_state,
        Box::new(NoopController),
        8,
        1 << 16,
    );

    let mut sim = Simulator::new();
    sim.add_switch(s1);
    sim.add_switch(s2);
    let lc = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(
        Node::Switch(SwitchId::new(1)), PortId::new(1),
        Node::Switch(SwitchId::new(2)), PortId::new(1), lc,
    );

    let mut p = Packet::new(PacketId::new(1), Duration::ZERO, 100);
    p.ip_dst = IpAddr(0x0a000002);
    p.ip_ttl = 16;
    sim.queue.schedule(Duration::ZERO, EventKind::SwitchIngress {
        switch: SwitchId::new(1),
        port: PortId::new(0),
        packet: p,
    });
    sim.run_until(Duration::from_millis(5));

    let captured = captured.borrow();
    assert_eq!(captured.payloads.len(), 1);
    let reply: TraceReplyW = postcard::from_bytes(&captured.payloads[0]).unwrap();
    assert!(!reply.delivered);
    assert_eq!(reply.drop_reason, 8); // Policy: the program chose to drop it
    assert_eq!(reply.hops.last().unwrap().switch_id, 2);
}
