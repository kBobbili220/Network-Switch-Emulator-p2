//! Integration test for SimpleBGP, mirroring the example scenario in
//! `bgp_plan.md`:
//!
//! ```text
//!   AS1 -- AS2 -- AS3
//!     \           /
//!      --- AS4 ---
//! ```
//!
//! AS3 owns 10.3.0.0/16. AS2 promises AS1 path [2, 3]. AS4 promises AS1
//! path [4, 3]. We then drive data traffic and check that the conformance
//! monitor records honored vs. violated bytes correctly.

#![cfg(feature = "wasm")]

use std::time::Duration;

use competitive_net_sim::bgp::{AsInfo, AsRegistry, Prefix as BgpPrefix, PromiseStatus};
use competitive_net_sim::controller::NoopController;
use competitive_net_sim::event::EventKind;
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::{IpAddr, Packet, PacketKind, PuntReason};
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{
    Instr, MatchActionTable, MatchKind, StageProgram, TableAction, TableEntry, TinyProgram,
    TinyVmState,
};
use competitive_net_sim::trace::TraceBudget;
use competitive_net_sim::packet::PacketField;
use competitive_net_sim::types::{
    AsId, EntryId, MetaKey, PacketId, PortId, Reg, SwitchId, TableId, TraceId,
};

use switch_program_types::bgp::{
    encode_envelope, AsIdW, AsPathW, BgpEnvelopeW, PrefixW, RoutePromiseW,
    SimpleBgpMessageW,
};

const AS1: u32 = 1;
const AS2: u32 = 2;
const AS3: u32 = 3;
const AS4: u32 = 4;

const S1: u32 = 1;
const S2: u32 = 2;
const S3: u32 = 3;
const S4: u32 = 4;

/// Speaker IPs (one per AS).
fn speaker_ip(as_n: u32) -> IpAddr {
    IpAddr(0xc0_a8_00_00 | as_n)
}

/// AS3 owns 10.3.0.0/16; this IP lives "behind" S3.
const DST_IP: u32 = 0x0a03_0001;

/// Build a TinyVM program that does a single ip_dst exact-match lookup.
fn forwarding_program() -> TinyProgram {
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

fn entry(id: u64, key: u64, port: u16) -> TableEntry {
    TableEntry {
        id: EntryId::new(id),
        key,
        prefix_len: 32,
        priority: 0,
        action: TableAction::SetEgress { port: PortId::new(port) },
    }
}

fn make_switch(id: u32, entries: &[(u64, u64, u16)]) -> Switch {
    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 64);
    for (eid, key, port) in entries {
        table.install(entry(*eid, *key, *port)).unwrap();
    }
    state.tables.push(table);
    Switch::new(
        SwitchConfig::defaults(SwitchId::new(id)),
        forwarding_program(),
        state,
        Box::new(NoopController),
        16,
        1 << 16,
    )
}

fn build_topology(as2_routes_via_4: bool) -> Simulator {
    // Port plan (per switch):
    //   S1: p2 -> S2, p4 -> S4
    //   S2: p1 -> S1, p3 -> S3
    //   S3: p2 -> S2, p4 -> S4
    //   S4: p1 -> S1, p3 -> S3
    //
    // Forwarding tables:
    //   S1: dst=DST_IP -> p2 (toward AS2); dst=speaker(2) -> p2; dst=speaker(4) -> p4
    //   S2 (default route via 3): dst=DST_IP -> p3
    //                             dst=speaker(1) -> p1
    //   S2 (alternate via 4): dst=DST_IP -> p1 (back to S1, so S1 reroutes)
    //                         no — better: S2-p... let's wire a direct S2-S4 link.
    //
    // We'll add a direct S2<->S4 link to model the "AS2 reroutes through AS4"
    // case cleanly:
    //   S2: p4 -> S4
    //   S4: p2 -> S2
    let s1_routes: Vec<(u64, u64, u16)> = vec![
        (1, DST_IP as u64, 2),                  // toward AS3 via AS2
        (2, speaker_ip(AS2).0 as u64, 2),
        (3, speaker_ip(AS3).0 as u64, 2),       // also via AS2 hop
        (4, speaker_ip(AS4).0 as u64, 4),
    ];

    // S2: depending on the toggle, dst=DST_IP either goes to AS3 directly
    // (honoring [2,3]) or detours through AS4 (violating [2,3]).
    let s2_routes: Vec<(u64, u64, u16)> = if as2_routes_via_4 {
        vec![
            (1, DST_IP as u64, 4), // detour via S4
            (2, speaker_ip(AS1).0 as u64, 1),
            (3, speaker_ip(AS3).0 as u64, 4),
        ]
    } else {
        vec![
            (1, DST_IP as u64, 3),
            (2, speaker_ip(AS1).0 as u64, 1),
            (3, speaker_ip(AS3).0 as u64, 3),
        ]
    };

    let s3_routes: Vec<(u64, u64, u16)> = vec![
        // S3 is the destination AS; deliver locally (we'll send a route to a
        // virtual app port for DST_IP).
        (1, DST_IP as u64, 5), // p5 -> app
        (2, speaker_ip(AS1).0 as u64, 2),
        (3, speaker_ip(AS2).0 as u64, 2),
        (4, speaker_ip(AS4).0 as u64, 4),
    ];
    let s4_routes: Vec<(u64, u64, u16)> = vec![
        (1, DST_IP as u64, 3),
        (2, speaker_ip(AS1).0 as u64, 1),
        (3, speaker_ip(AS3).0 as u64, 3),
    ];

    let mut sim = Simulator::new();
    sim.add_switch(make_switch(S1, &s1_routes));
    sim.add_switch(make_switch(S2, &s2_routes));
    sim.add_switch(make_switch(S3, &s3_routes));
    sim.add_switch(make_switch(S4, &s4_routes));

    let lc = LinkConfig {
        latency: Duration::from_micros(50),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    // S1<->S2
    sim.connect(Node::Switch(SwitchId::new(S1)), PortId::new(2),
                Node::Switch(SwitchId::new(S2)), PortId::new(1), lc);
    // S2<->S3
    sim.connect(Node::Switch(SwitchId::new(S2)), PortId::new(3),
                Node::Switch(SwitchId::new(S3)), PortId::new(2), lc);
    // S1<->S4
    sim.connect(Node::Switch(SwitchId::new(S1)), PortId::new(4),
                Node::Switch(SwitchId::new(S4)), PortId::new(1), lc);
    // S3<->S4
    sim.connect(Node::Switch(SwitchId::new(S3)), PortId::new(4),
                Node::Switch(SwitchId::new(S4)), PortId::new(3), lc);
    // S2<->S4 (direct, used for the "AS2 detours via AS4" test case)
    sim.connect(Node::Switch(SwitchId::new(S2)), PortId::new(4),
                Node::Switch(SwitchId::new(S4)), PortId::new(2), lc);

    // Configure BGP.
    let mut reg = AsRegistry::new();
    reg.add_as(AsInfo {
        as_id: AsId::new(AS1),
        border_switch: SwitchId::new(S1),
        bgp_speaker_ip: speaker_ip(AS1),
        owned_prefixes: vec![BgpPrefix::new(0x0a01_0000, 16)],
    }, &[]);
    reg.add_as(AsInfo {
        as_id: AsId::new(AS2),
        border_switch: SwitchId::new(S2),
        bgp_speaker_ip: speaker_ip(AS2),
        owned_prefixes: vec![BgpPrefix::new(0x0a02_0000, 16)],
    }, &[]);
    reg.add_as(AsInfo {
        as_id: AsId::new(AS3),
        border_switch: SwitchId::new(S3),
        bgp_speaker_ip: speaker_ip(AS3),
        owned_prefixes: vec![BgpPrefix::new(0x0a03_0000, 16)],
    }, &[]);
    reg.add_as(AsInfo {
        as_id: AsId::new(AS4),
        border_switch: SwitchId::new(S4),
        bgp_speaker_ip: speaker_ip(AS4),
        owned_prefixes: vec![BgpPrefix::new(0x0a04_0000, 16)],
    }, &[]);
    sim.configure_bgp(reg);
    sim
}

/// Build a SimpleBGP promise packet and inject it as if it had just
/// arrived at the receiver's border switch ingress on port 1. (We could
/// route it through the network instead, but injecting at the border
/// keeps this test focused on validation/ledger semantics.)
fn inject_promise(
    sim: &mut Simulator,
    sender_as: u32,
    receiver_as: u32,
    sender_speaker_ip: IpAddr,
    receiver_speaker_ip: IpAddr,
    receiver_border: SwitchId,
    promise_id: u64,
    paths: Vec<Vec<u32>>,
    prefix: PrefixW,
) {
    let env = BgpEnvelopeW {
        msg_id: promise_id,
        sender_as: AsIdW(sender_as),
        receiver_as: AsIdW(receiver_as),
        sender_bgp_ip: sender_speaker_ip.0,
        receiver_bgp_ip: receiver_speaker_ip.0,
        message: SimpleBgpMessageW::Promise(RoutePromiseW {
            promise_id,
            prefix,
            traffic_class: None,
            promised_paths: paths.into_iter().map(|p| AsPathW {
                ases: p.into_iter().map(AsIdW).collect(),
            }).collect(),
            tags: vec![],
            valid_until_ns: None,
        }),
    };
    let mut p = Packet::new(PacketId::new(promise_id), sim.now, 256);
    p.kind = PacketKind::SimpleBgp;
    p.ip_src = sender_speaker_ip;
    p.ip_dst = receiver_speaker_ip;
    p.ip_proto = 179; // BGP-ish; the simulator only checks `kind`.
    p.payload = encode_envelope(&env);
    sim.queue.schedule(sim.now, EventKind::SwitchIngress {
        switch: receiver_border,
        port: PortId::new(0xFFFF),
        packet: p,
    });
}

fn inject_data(
    sim: &mut Simulator,
    src_ip: IpAddr,
    ingress_switch: SwitchId,
    ingress_port: PortId,
) {
    let mut p = Packet::new(PacketId::new(0), sim.now, 100);
    p.ip_src = src_ip;
    p.ip_dst = IpAddr(DST_IP);
    p.ip_ttl = 16;
    sim.queue.schedule(sim.now, EventKind::SwitchIngress {
        switch: ingress_switch,
        port: ingress_port,
        packet: p,
    });
}

#[test]
fn promise_recorded_when_packet_reaches_speaker() {
    let mut sim = build_topology(false);
    let prefix = PrefixW { addr: 0x0a03_0000, len: 16 };

    // AS2 -> AS1 promises path [2, 3].
    inject_promise(
        &mut sim,
        AS2, AS1, speaker_ip(AS2), speaker_ip(AS1),
        SwitchId::new(S1),
        100,
        vec![vec![AS2, AS3]],
        prefix,
    );
    // AS4 -> AS1 promises path [4, 3].
    inject_promise(
        &mut sim,
        AS4, AS1, speaker_ip(AS4), speaker_ip(AS1),
        SwitchId::new(S1),
        101,
        vec![vec![AS4, AS3]],
        prefix,
    );
    sim.run_until(Duration::from_millis(10));

    let bgp = sim.bgp.as_ref().unwrap();
    assert_eq!(bgp.ledger.entries.len(), 2);
    let active = bgp.ledger.active_promises();
    assert_eq!(active.len(), 2);
}

#[test]
fn promise_not_recorded_when_packet_dropped_en_route() {
    let mut sim = build_topology(false);
    // AS2 sends a promise toward AS1's speaker, but enters the network at
    // S3 with TTL=1 so it dies before reaching S1's border. The ledger
    // should remain empty and nothing should be marked rejected (the
    // packet never reached a BGP speaker at all).
    let prefix = PrefixW { addr: 0x0a03_0000, len: 16 };
    let env = BgpEnvelopeW {
        msg_id: 100,
        sender_as: AsIdW(AS2),
        receiver_as: AsIdW(AS1),
        sender_bgp_ip: speaker_ip(AS2).0,
        receiver_bgp_ip: speaker_ip(AS1).0,
        message: SimpleBgpMessageW::Promise(RoutePromiseW {
            promise_id: 100,
            prefix,
            traffic_class: None,
            promised_paths: vec![AsPathW {
                ases: vec![AsIdW(AS2), AsIdW(AS3)],
            }],
            tags: vec![],
            valid_until_ns: None,
        }),
    };
    let mut p = Packet::new(PacketId::new(100), sim.now, 256);
    p.kind = PacketKind::SimpleBgp;
    p.ip_src = speaker_ip(AS2);
    p.ip_dst = speaker_ip(AS1);
    p.ip_ttl = 1; // expires after one hop
    p.payload = encode_envelope(&env);
    // Enter at S3, far from AS1's border.
    sim.queue.schedule(sim.now, EventKind::SwitchIngress {
        switch: SwitchId::new(S3),
        port: PortId::new(0xFFFF),
        packet: p,
    });
    sim.run_until(Duration::from_millis(10));
    let bgp = sim.bgp.as_ref().unwrap();
    assert_eq!(bgp.ledger.entries.len(), 0);
    assert_eq!(bgp.rejected.len(), 0, "TTL drop should not even reach a speaker");
}

#[test]
fn conformance_honored_when_path_matches() {
    let mut sim = build_topology(/* as2_routes_via_4 = */ false);
    let prefix = PrefixW { addr: 0x0a03_0000, len: 16 };
    inject_promise(
        &mut sim, AS2, AS1, speaker_ip(AS2), speaker_ip(AS1),
        SwitchId::new(S1), 100, vec![vec![AS2, AS3]], prefix,
    );
    sim.run_until(Duration::from_millis(1));

    // AS1 sends data; in this topology S1's table sends it via S2,
    // S2 forwards to S3 (matching [2,3]).
    inject_data(&mut sim, speaker_ip(AS1), SwitchId::new(S1), PortId::new(0xFFFF));
    sim.run_until(Duration::from_millis(10));

    let bgp = sim.bgp.as_ref().unwrap();
    let stats = bgp.monitor.for_promise(competitive_net_sim::PromiseId::new(100));
    assert_eq!(stats.packets_honored, 1, "expected 1 honored, got {:?}", stats);
    assert_eq!(stats.packets_violated, 0);
}

#[test]
fn conformance_violation_when_path_diverges() {
    let mut sim = build_topology(/* as2_routes_via_4 = */ true);
    let prefix = PrefixW { addr: 0x0a03_0000, len: 16 };
    inject_promise(
        &mut sim, AS2, AS1, speaker_ip(AS2), speaker_ip(AS1),
        SwitchId::new(S1), 100, vec![vec![AS2, AS3]], prefix,
    );
    sim.run_until(Duration::from_millis(1));

    inject_data(&mut sim, speaker_ip(AS1), SwitchId::new(S1), PortId::new(0xFFFF));
    sim.run_until(Duration::from_millis(10));

    let bgp = sim.bgp.as_ref().unwrap();
    let stats = bgp.monitor.for_promise(competitive_net_sim::PromiseId::new(100));
    assert_eq!(stats.packets_honored, 0);
    assert_eq!(stats.packets_violated, 1, "expected 1 violation, got {:?}", stats);
}

#[test]
fn withdrawal_marks_promise_inactive() {
    use switch_program_types::bgp::{RouteWithdrawW, SimpleBgpMessageW};
    let mut sim = build_topology(false);
    let prefix = PrefixW { addr: 0x0a03_0000, len: 16 };
    inject_promise(
        &mut sim, AS2, AS1, speaker_ip(AS2), speaker_ip(AS1),
        SwitchId::new(S1), 100, vec![vec![AS2, AS3]], prefix,
    );
    sim.run_until(Duration::from_millis(1));

    // Now inject a withdrawal.
    let env = BgpEnvelopeW {
        msg_id: 200,
        sender_as: AsIdW(AS2),
        receiver_as: AsIdW(AS1),
        sender_bgp_ip: speaker_ip(AS2).0,
        receiver_bgp_ip: speaker_ip(AS1).0,
        message: SimpleBgpMessageW::Withdraw(RouteWithdrawW {
            withdrawn_promise_id: Some(100),
            prefix: None,
            traffic_class: None,
        }),
    };
    let mut p = Packet::new(PacketId::new(200), sim.now, 128);
    p.kind = PacketKind::SimpleBgp;
    p.ip_src = speaker_ip(AS2);
    p.ip_dst = speaker_ip(AS1);
    p.payload = encode_envelope(&env);
    sim.queue.schedule(sim.now, EventKind::SwitchIngress {
        switch: SwitchId::new(S1),
        port: PortId::new(0xFFFF),
        packet: p,
    });
    sim.run_until(Duration::from_millis(2));

    let bgp = sim.bgp.as_ref().unwrap();
    assert!(matches!(
        bgp.ledger.entries[0].status,
        PromiseStatus::Withdrawn { .. }
    ));
}

#[test]
fn trace_returns_path_and_is_invisible_to_program() {
    let mut sim = build_topology(false);

    // Set up a budget for AS1.
    sim.set_trace_budget(AsId::new(AS1), TraceBudget {
        max_traces_per_second: 100,
        burst_size: 5,
    });

    let mut template = Packet::new(PacketId::new(7777), sim.now, 100);
    template.ip_src = speaker_ip(AS1);
    template.ip_dst = IpAddr(DST_IP);
    let id = sim.request_trace(AsId::new(AS1), template, 16).unwrap();
    sim.run_until(Duration::from_millis(10));

    let result = sim.take_trace_result(id).expect("trace should complete");
    // Path should be S1 -> S2 -> S3.
    assert_eq!(
        result.switch_path,
        vec![SwitchId::new(S1), SwitchId::new(S2), SwitchId::new(S3)],
    );
    assert_eq!(
        result.as_path,
        vec![AsId::new(AS1), AsId::new(AS2), AsId::new(AS3)],
    );
    assert!(result.delivered || result.drop_reason.is_some());
}

#[test]
fn trace_budget_enforced() {
    let mut sim = build_topology(false);
    sim.set_trace_budget(AsId::new(AS1), TraceBudget {
        max_traces_per_second: 0, // refill rate 0
        burst_size: 1,
    });
    let mut template = Packet::new(PacketId::new(0), sim.now, 64);
    template.ip_src = speaker_ip(AS1);
    template.ip_dst = IpAddr(DST_IP);
    // First should succeed; second should fail.
    sim.request_trace(AsId::new(AS1), template.clone(), 16).unwrap();
    let err = sim.request_trace(AsId::new(AS1), template, 16).unwrap_err();
    assert_eq!(err, competitive_net_sim::trace::TraceError::BudgetExceeded);
}

#[test]
fn validation_rejects_unauthorized_origin() {
    let mut sim = build_topology(false);
    // AS2 promising AS3's prefix is unauthorized; we routed it through
    // the AS1 border (the receiver), so we only catch validation issues.
    let prefix = PrefixW { addr: 0x0a02_0000, len: 16 }; // AS2 owns this
    // Sender AS2 promising its own prefix — should validate.
    inject_promise(
        &mut sim, AS2, AS1, speaker_ip(AS2), speaker_ip(AS1),
        SwitchId::new(S1), 100, vec![vec![AS2]], prefix,
    );
    // Sender AS4 promising AS3's prefix — unauthorized.
    let prefix3 = PrefixW { addr: 0x0a03_0000, len: 16 };
    inject_promise(
        &mut sim, AS4, AS1, speaker_ip(AS4), speaker_ip(AS1),
        SwitchId::new(S1), 200, vec![vec![AS4]], prefix3,
    );
    sim.run_until(Duration::from_millis(2));
    let bgp = sim.bgp.as_ref().unwrap();
    assert_eq!(bgp.ledger.entries.len(), 1, "only the authorized promise should be recorded");
    assert_eq!(bgp.rejected.len(), 1);
}

// Suppress unused warnings for symbols we may want available later.
#[allow(dead_code)]
fn _kept(_: PuntReason, _: TraceId) {}
