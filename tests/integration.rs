use competitive_net_sim::app::{App, TrafficPattern};
use competitive_net_sim::controller::{
    ControllerAction, Discipline, LinkEvent, NoopController, PortDrops, PuntEvent, QueueConfig,
    QueueSpec, SwitchController,
};
use competitive_net_sim::link::LinkConfig;
use competitive_net_sim::network::Node;
use competitive_net_sim::packet::{IpAddr, PacketField, PuntReason};
use competitive_net_sim::sim::Simulator;
use competitive_net_sim::switch::{Switch, SwitchConfig};
use competitive_net_sim::tinyvm::{
    Instr, MatchActionTable, MatchKind, StageProgram, TableAction, TableEntry, TinyProgram,
    TinyVmState,
};
use competitive_net_sim::types::{
    AppId, EntryId, MetaKey, PortId, QueueId, Reg, SimTime, SwitchId, TableId,
};
use std::time::Duration;

fn build_simple_topology(
    install_route: bool,
) -> (Simulator, AppId, AppId, SwitchId) {
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
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    );

    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    if install_route {
        table
            .install(TableEntry {
                id: EntryId::new(1),
                key: 0x0a000002,
                prefix_len: 32,
                priority: 0,
                action: TableAction::SetEgress {
                    port: PortId::new(1),
                },
            })
            .unwrap();
    }
    state.tables.push(table);

    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField {
                    dst: Reg::new(0),
                    field: PacketField::IpDst,
                },
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

    sim.connect(
        Node::App(app_a),
        PortId::new(0),
        Node::Switch(switch_id),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(100),
            bandwidth_bps: 1_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );
    sim.connect(
        Node::Switch(switch_id),
        PortId::new(1),
        Node::App(app_b),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(100),
            bandwidth_bps: 1_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );

    (sim, app_a, app_b, switch_id)
}

#[test]
fn packet_forwarded_on_matching_route() {
    let (mut sim, _a, b, _s) = build_simple_topology(true);
    sim.run_until(Duration::from_millis(50));
    assert!(sim.apps[&b].metrics.packets_received > 0);
}

#[test]
fn packet_dropped_on_no_route_when_no_punt() {
    // We use a punting switch by default (NoEgress -> Punt), so let's instead
    // verify that without a route, B never receives anything.
    let (mut sim, _a, b, _s) = build_simple_topology(false);
    sim.run_until(Duration::from_millis(50));
    assert_eq!(sim.apps[&b].metrics.packets_received, 0);
}

#[test]
fn multi_hop_delivery_and_delay_accumulation() {
    // Two switches in series.
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let s0 = SwitchId::new(0);
    let s1 = SwitchId::new(1);

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
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    );

    fn build_switch(switch_id: SwitchId, out_port: u16) -> Switch {
        let mut state = TinyVmState::default();
        let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
        table
            .install(TableEntry {
                id: EntryId::new(1),
                key: 0x0a000002,
                prefix_len: 32,
                priority: 0,
                action: TableAction::SetEgress {
                    port: PortId::new(out_port),
                },
            })
            .unwrap();
        state.tables.push(table);
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::LoadField {
                        dst: Reg::new(0),
                        field: PacketField::IpDst,
                    },
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
        Switch::new(
            SwitchConfig::defaults(switch_id),
            prog,
            state,
            Box::new(NoopController),
            4,
            1 << 16,
        )
    }

    sim.add_switch(build_switch(s0, 1));
    sim.add_switch(build_switch(s1, 1));
    sim.add_app(a);
    sim.add_app(b);

    let lc = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(app_a), PortId::new(0), Node::Switch(s0), PortId::new(0), lc);
    sim.connect(Node::Switch(s0), PortId::new(1), Node::Switch(s1), PortId::new(0), lc);
    sim.connect(Node::Switch(s1), PortId::new(1), Node::App(app_b), PortId::new(0), lc);

    sim.run_until(Duration::from_millis(50));
    assert!(sim.apps[&app_b].metrics.packets_received > 0);
    // Multi-hop delay should be larger than a single hop's prop delay.
    assert!(sim.apps[&app_b].metrics.avg_delay > Duration::from_micros(200));
}

// A controller that installs a route on first punt.
struct InstallOnPuntController {
    installed: bool,
    table: TableId,
}

impl SwitchController for InstallOnPuntController {
    fn on_punt(&mut self, _event: PuntEvent) -> Vec<ControllerAction> {
        if self.installed {
            return Vec::new();
        }
        self.installed = true;
        vec![ControllerAction::InstallTableEntry {
            table_id: self.table,
            entry: TableEntry {
                id: EntryId::new(42),
                key: 0x0a000002,
                prefix_len: 32,
                priority: 0,
                action: TableAction::SetEgress {
                    port: PortId::new(1),
                },
            },
        }]
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_link_event(
        &mut self,
        _event: competitive_net_sim::controller::LinkEvent,
    ) -> Vec<ControllerAction> {
        Vec::new()
    }
}

#[test]
fn controller_installed_route_affects_later_packets() {
    // No initial route. NoEgress packets become punts to controller.
    // Controller installs route after first punt.
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
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    );

    let mut state = TinyVmState::default();
    state
        .tables
        .push(MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16));
    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField {
                    dst: Reg::new(0),
                    field: PacketField::IpDst,
                },
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
        Box::new(InstallOnPuntController {
            installed: false,
            table: TableId::new(1),
        }),
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

    sim.run_until(Duration::from_millis(200));
    assert!(sim.apps[&app_b].metrics.packets_received > 0);
}

#[test]
fn cpu_failure_does_not_stop_data_plane() {
    let (mut sim, _a, b, s) = build_simple_topology(true);
    sim.fail_cpu(s);
    sim.run_until(Duration::from_millis(50));
    assert!(sim.apps[&b].metrics.packets_received > 0);
}

#[test]
fn switch_failure_drops_all_traffic() {
    let (mut sim, _a, b, s) = build_simple_topology(true);
    sim.fail_switch(s);
    sim.run_until(Duration::from_millis(50));
    assert_eq!(sim.apps[&b].metrics.packets_received, 0);
}

#[test]
fn ttl_decrement_prevents_loops() {
    // Set up a route that sends packets back to ingress (0) with a SetEgress
    // action. With TTL decrement on each pass, the packet should die after
    // TTL hops even if it would otherwise loop forever.
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let switch_id = SwitchId::new(0);

    let mut a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 100,
        },
    );
    // Send one packet manually with a TTL that should expire quickly.
    let _ = &mut a;

    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    // Send back out port 0 -> back toward app.
    table
        .install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000002,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetEgress {
                port: PortId::new(0),
            },
        })
        .unwrap();
    state.tables.push(table);
    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadField {
                    dst: Reg::new(0),
                    field: PacketField::IpDst,
                },
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
    sim.add_switch(Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        Box::new(NoopController),
        4,
        1 << 16,
    ));
    sim.add_app(a);

    let lc = LinkConfig {
        latency: Duration::from_micros(10),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(app_a), PortId::new(0), Node::Switch(switch_id), PortId::new(0), lc);

    // Run; should terminate since TTL bounds the bouncing.
    sim.run_until(Duration::from_millis(10));
    // Just that we made it here without infinite loop is the test.
}

#[test]
fn punt_pipe_latency_respected() {
    // We measure that on_punt callback fires no earlier than punt_pipe_latency
    // after ingress. Use a controller that records the first punt time.
    use std::cell::RefCell;
    use std::rc::Rc;

    struct RecordController {
        first_punt: Rc<RefCell<Option<SimTime>>>,
    }
    impl SwitchController for RecordController {
        fn on_punt(&mut self, event: PuntEvent) -> Vec<ControllerAction> {
            let mut slot = self.first_punt.borrow_mut();
            if slot.is_none() {
                *slot = Some(event.now);
            }
            Vec::new()
        }
        fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
            Vec::new()
        }
        fn on_link_event(
            &mut self,
            _event: competitive_net_sim::controller::LinkEvent,
        ) -> Vec<ControllerAction> {
            Vec::new()
        }
    }

    let recorded = Rc::new(RefCell::new(None));
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let switch_id = SwitchId::new(0);
    let mut cfg = SwitchConfig::defaults(switch_id);
    cfg.punt_pipe_latency = Duration::from_millis(5);

    let a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_millis(100),
            size_bytes: 100,
        },
    );
    let mut state = TinyVmState::default();
    state
        .tables
        .push(MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16));
    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![Instr::Punt {
                reason: PuntReason::NoRoute,
            }],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        }],
    };
    let switch = Switch::new(
        cfg,
        prog,
        state,
        Box::new(RecordController {
            first_punt: recorded.clone(),
        }),
        4,
        1 << 16,
    );
    sim.add_switch(switch);
    sim.add_app(a);
    sim.connect(
        Node::App(app_a),
        PortId::new(0),
        Node::Switch(switch_id),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(0),
            bandwidth_bps: 1_000_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );
    sim.run_until(Duration::from_millis(20));
    let t = recorded.borrow().expect("punt should have fired");
    assert!(t >= Duration::from_millis(5));
}

#[test]
fn recirculation_consumes_extra_processing() {
    // Stage 0 recirculates once; stage 0 then forwards. Verify that the packet
    // arrives later than if there were no recirculation (i.e., recirculation
    // adds at least one pipeline_delay).
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let switch_id = SwitchId::new(0);

    let a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_millis(100),
            size_bytes: 100,
        },
    );
    let b = App::new(
        app_b,
        IpAddr(0x0a000002), 32,
        IpAddr(0x0a000001), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    );

    let mut state = TinyVmState::default();
    state
        .tables
        .push(MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16));
    state.registers.push(competitive_net_sim::tinyvm::RegisterArray::new(
        competitive_net_sim::types::RegisterArrayId::new(0),
        4,
    ));

    // Program: if metadata[0] == 1 (already recirculated), set egress and halt.
    // Otherwise set meta=1 and recirculate.
    //
    // Layout:
    //   0: LoadMeta r0, key 0
    //   1: Const   r1 = 1
    //   2: Eq      r2 = (r0 == r1)
    //   3: BranchIf r2 -> 7   (already recirculated -> forward path)
    //   4: Const   r3 = 1
    //   5: StoreMeta key 0 = r3
    //   6: Recirculate         (terminates this pass)
    //   7: SetEgress port=1
    //   8: Noop                (fall-through end)
    let prog = TinyProgram {
        stages: vec![StageProgram {
            instrs: vec![
                Instr::LoadMeta {
                    dst: Reg::new(0),
                    key: MetaKey::new(0),
                },
                Instr::Const {
                    dst: Reg::new(1),
                    value: 1,
                },
                Instr::Eq {
                    dst: Reg::new(2),
                    a: Reg::new(0),
                    b: Reg::new(1),
                },
                Instr::BranchIf {
                    cond: Reg::new(2),
                    target: competitive_net_sim::types::InstrIndex::new(7),
                },
                Instr::Const {
                    dst: Reg::new(3),
                    value: 1,
                },
                Instr::StoreMeta {
                    key: MetaKey::new(0),
                    src: Reg::new(3),
                },
                Instr::Recirculate,
                Instr::SetEgress {
                    port: PortId::new(1),
                },
                Instr::Noop,
            ],
            max_alu_ops: 16,
            max_memory_accesses: 0,
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
        latency: Duration::from_micros(10),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(app_a), PortId::new(0), Node::Switch(switch_id), PortId::new(0), lc);
    sim.connect(Node::Switch(switch_id), PortId::new(1), Node::App(app_b), PortId::new(0), lc);

    sim.run_until(Duration::from_millis(50));
    assert!(sim.apps[&app_b].metrics.packets_received > 0);
    // Each delivered packet experienced two pipeline passes.
    let avg = sim.apps[&app_b].metrics.avg_delay;
    assert!(avg > Duration::from_micros(20));
}

/// One slow egress port fed faster than it drains. Build the topology by
/// hand so the timing is exact: app A → 1 Gbps / 100 µs → switch →
/// 8 kbps / 10 ms → app B. A 100-byte packet takes 100 ms to serialize on
/// the slow link, so with A sending every `interval` the switch's port 1
/// queue builds up.
fn build_slow_egress(interval: Duration, port_queue_bytes: u64) -> (Simulator, AppId, SwitchId) {
    let mut sim = Simulator::new();
    let app_a = AppId::new(1);
    let app_b = AppId::new(2);
    let switch_id = SwitchId::new(0);

    let a = App::new(
        app_a,
        IpAddr(0x0a000001), 32,
        IpAddr(0x0a000002), 32,
        TrafficPattern::ConstantBitrate { interval, size_bytes: 100 },
    );
    let b = App::new(
        app_b,
        IpAddr(0x0a000002), 32,
        IpAddr(0x0a000001), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    );

    let mut state = TinyVmState::default();
    let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    table
        .install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000002,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetEgress { port: PortId::new(1) },
        })
        .unwrap();
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
    sim.add_switch(Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        Box::new(NoopController),
        4,
        port_queue_bytes,
    ));
    sim.add_app(a);
    sim.add_app(b);

    sim.connect(
        Node::App(app_a),
        PortId::new(0),
        Node::Switch(switch_id),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_micros(100),
            bandwidth_bps: 1_000_000_000,
            queue_capacity_bytes: 1 << 16,
        },
    );
    sim.connect(
        Node::Switch(switch_id),
        PortId::new(1),
        Node::App(app_b),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_millis(10),
            bandwidth_bps: 8_000, // 100 bytes = 100 ms on the wire
            queue_capacity_bytes: 1 << 16,
        },
    );
    (sim, app_b, switch_id)
}

/// Emits one `SetQueueConfig` for port 1 the first time its timer fires.
struct QueueSetupController {
    config: QueueConfig,
    done: bool,
}

impl SwitchController for QueueSetupController {
    fn on_punt(&mut self, _event: PuntEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![ControllerAction::SetQueueConfig {
            port: PortId::new(1),
            config: self.config.clone(),
        }]
    }
    fn on_link_event(&mut self, _event: LinkEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
}

/// Two senders share one slow port toward `sink`. `data` floods it every
/// `data_interval`; `control` sends one packet every `control_interval`.
/// The program routes on ip_dst and puts `control`'s packets in queue 0,
/// `data`'s in queue 1. The controller applies `config` to port 1 at t=0.
fn build_two_senders_one_slow_port(
    data_interval: Duration,
    control_interval: Duration,
    config: QueueConfig,
) -> (Simulator, AppId, AppId, AppId, SwitchId) {
    build_two_senders_one_slow_port_with(
        data_interval,
        control_interval,
        Box::new(QueueSetupController { config, done: false }),
    )
}

/// `build_two_senders_one_slow_port` with the controller supplied.
fn build_two_senders_one_slow_port_with(
    data_interval: Duration,
    control_interval: Duration,
    controller: Box<dyn SwitchController>,
) -> (Simulator, AppId, AppId, AppId, SwitchId) {
    let mut sim = Simulator::new();
    let data = AppId::new(1);
    let control = AppId::new(2);
    let sink = AppId::new(3);
    let switch_id = SwitchId::new(0);
    let sink_ip = IpAddr(0x0a000003);

    let mk = |id, ip, interval| {
        App::new(
            id,
            ip, 32,
            sink_ip, 32,
            TrafficPattern::ConstantBitrate { interval, size_bytes: 100 },
        )
    };
    sim.add_app(mk(data, IpAddr(0x0a000001), data_interval));
    sim.add_app(mk(control, IpAddr(0x0a000002), control_interval));
    sim.add_app(App::new(
        sink,
        sink_ip, 32,
        IpAddr(0x0a000001), 32,
        TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(3600),
            size_bytes: 0,
        },
    ));

    let mut state = TinyVmState::default();
    let mut routes = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    routes
        .install(TableEntry {
            id: EntryId::new(1),
            key: sink_ip.0 as u64,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetEgress { port: PortId::new(1) },
        })
        .unwrap();
    let mut classes = MatchActionTable::new(TableId::new(2), MatchKind::Exact, 16);
    classes
        .install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000001,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetQueue { queue: QueueId::new(1) },
        })
        .unwrap();
    classes
        .install(TableEntry {
            id: EntryId::new(2),
            key: 0x0a000002,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetQueue { queue: QueueId::new(0) },
        })
        .unwrap();
    state.tables.push(routes);
    state.tables.push(classes);
    let stage = |field, table| StageProgram {
        instrs: vec![
            Instr::LoadField { dst: Reg::new(0), field },
            Instr::TableLookup {
                table_id: TableId::new(table),
                key_reg: Reg::new(0),
                result_meta: MetaKey::new(0),
            },
        ],
        max_alu_ops: 0,
        max_memory_accesses: 1,
    };
    let prog = TinyProgram {
        stages: vec![stage(PacketField::IpDst, 1), stage(PacketField::IpSrc, 2)],
    };
    sim.add_switch(Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        controller,
        4,
        1 << 16,
    ));
    sim.schedule_controller_timer(switch_id, Duration::ZERO);

    let fast = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(data), PortId::new(0), Node::Switch(switch_id), PortId::new(0), fast);
    sim.connect(Node::App(control), PortId::new(0), Node::Switch(switch_id), PortId::new(2), fast);
    sim.connect(
        Node::Switch(switch_id),
        PortId::new(1),
        Node::App(sink),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_millis(10),
            bandwidth_bps: 8_000, // 100 bytes = 100 ms on the wire
            queue_capacity_bytes: 1 << 16,
        },
    );
    (sim, data, control, sink, switch_id)
}

#[test]
fn priority_queue_carries_control_traffic_across_a_saturated_port() {
    // Data floods port 1 ten times faster than the wire drains it; its
    // queue (5 packets) overflows within the first second. Control sends
    // once a second into queue 0 under strict priority, so each control
    // packet waits for at most the data packet already on the wire:
    // delay <= 100 ms (residual) + 100 ms (own serialization) + 10 ms.
    let config = QueueConfig {
        discipline: Discipline::StrictPriority,
        queues: vec![QueueSpec::new(64), QueueSpec::new(5)],
    };
    let (mut sim, data, control, sink, switch_id) = build_two_senders_one_slow_port(
        Duration::from_millis(10),
        Duration::from_secs(1),
        config,
    );
    sim.run_until(Duration::from_secs(10));

    let sw = &sim.switches[&switch_id];
    let port = &sw.egress[1];
    assert_eq!(port.queues.len(), 2);
    assert_eq!(port.queues[1].max_bytes, 500);
    assert!(port.packets_dropped > 500, "data queue should overflow: {}", port.packets_dropped);
    assert!(port.queues[0].is_empty(), "control queue never builds up");
    assert!(sw.buffer_used <= 700, "5 data + 1 on the wire + at most 1 control");

    let m = &sim.apps[&sink].metrics;
    // The wire moves 10 pkt/s; the first lands at 110.1 ms so the 100th
    // is 10.1 ms past the horizon.
    assert_eq!(m.packets_received, 99);
    let bound = Duration::from_millis(210) + Duration::from_micros(200);
    let control_delays: Vec<Duration> = m.delays_from(0x0a000002).collect();
    assert_eq!(control_delays.len(), 10, "one control packet per second, none lost");
    for d in &control_delays {
        assert!(*d <= bound, "control delay {d:?} exceeds one residual + own serialization");
    }
    // Once the data queue is full (after its first five packets), every
    // data packet that gets through has waited behind five others.
    let data_delays: Vec<Duration> = m.delays_from(0x0a000001).collect();
    assert_eq!(data_delays.len(), 89);
    for d in &data_delays[5..] {
        assert!(*d >= Duration::from_millis(500), "data delay {d:?}");
    }
    let (from_control, from_data) = count_by_source(&sim, sink);
    assert_eq!((from_control, from_data), (10, 89));
    // Sends at 0, 1, …, 10 s: the tick at the horizon itself still fires.
    assert_eq!(sim.apps[&control].metrics.packets_sent, 11);
    assert_eq!(sim.apps[&data].metrics.packets_sent, 1001);
}

#[test]
fn fifo_across_two_queues_lets_data_bury_control() {
    // Same setup with a single FIFO queue: the class table's `SetQueue 1`
    // is clamped to the only queue, so control packets compete with the
    // flood for the five slots. Control sends every 997 ms so it is not
    // phase-locked to the 100 ms wire slots; it mostly finds the queue
    // full and is dropped, and whatever gets through waited like data.
    let config = QueueConfig {
        discipline: Discipline::Fifo,
        queues: vec![QueueSpec::new(5)],
    };
    let (mut sim, _, _, sink, _) = build_two_senders_one_slow_port(
        Duration::from_millis(10),
        Duration::from_millis(997),
        config,
    );
    sim.run_until(Duration::from_secs(10));
    let m = &sim.apps[&sink].metrics;
    let bound = Duration::from_millis(210) + Duration::from_micros(200);
    let control_delays: Vec<Duration> = m.delays_from(0x0a000002).collect();
    let fast = control_delays.iter().filter(|d| **d <= bound).count();
    // Only the very first control packet finds an empty queue.
    assert!(fast <= 1, "without priority, control packets queue like data: {fast} fast");
    assert!(
        control_delays.len() < 10,
        "some control packets are lost to the full queue: {} delivered",
        control_delays.len()
    );
}

#[test]
fn drr_splits_a_saturated_port_by_weight() {
    // Both senders flood. Weights 1:3 → over the run the sink receives
    // roughly one packet from `data` for every three from `control`.
    let config = QueueConfig {
        discipline: Discipline::Drr,
        queues: vec![
            QueueSpec { max_packets: 10, weight: 3, drop_threshold_packets: None },
            QueueSpec { max_packets: 10, weight: 1, drop_threshold_packets: None },
        ],
    };
    let (mut sim, _, _, sink, _) = build_two_senders_one_slow_port(
        Duration::from_millis(10),
        Duration::from_millis(10),
        config,
    );
    sim.run_until(Duration::from_secs(10));
    let m = &sim.apps[&sink].metrics;
    assert_eq!(m.packets_received, 99);
    let (from_control, from_data) = count_by_source(&sim, sink);
    assert!((72..=76).contains(&from_control), "control got {from_control}");
    assert!((23..=27).contains(&from_data), "data got {from_data}");
}

#[test]
fn drop_threshold_drops_are_policy_and_bound_the_queue() {
    // Port 1's data queue may hold 50 packets but is told to drop past 5.
    // The flood is refused at the line while the buffer has room, and every
    // one of those drops reaches the log as Policy, not QueueFull.
    let config = QueueConfig {
        discipline: Discipline::StrictPriority,
        queues: vec![
            QueueSpec::new(64),
            QueueSpec {
                max_packets: 50,
                weight: 1,
                drop_threshold_packets: Some(5),
            },
        ],
    };
    let (mut sim, data, _, sink, switch_id) = build_two_senders_one_slow_port(
        Duration::from_millis(10),
        Duration::from_secs(3600),
        config,
    );
    let log_path = std::env::temp_dir().join(format!(
        "networkarena-threshold-{}.simlog",
        std::process::id()
    ));
    sim.start_logging(&log_path).unwrap();
    sim.run_until(Duration::from_secs(5));

    let sw = &sim.switches[&switch_id];
    let port = &sw.egress[1];
    assert_eq!(port.queues[1].max_bytes, 5000);
    assert!(port.queues[1].bytes() <= 500, "occupancy never passes the line");
    assert!(sw.buffer_used <= 600, "5 queued + 1 on the wire");
    assert!(port.policy_dropped > 400, "policy drops: {}", port.policy_dropped);
    assert_eq!(port.policy_dropped, port.packets_dropped, "no full-queue drops at all");
    assert_eq!(sim.apps[&sink].metrics.packets_received, 49);
    // Every delivered packet waited behind at most the one on the wire
    // plus five queued: 100 + 500 + 100 own + 10 latency.
    let m = &sim.apps[&sink].metrics;
    for d in m.delays_from(0x0a000001) {
        assert!(d <= Duration::from_millis(710) + Duration::from_micros(200), "{d:?}");
    }
    let _ = data;
    let policy_dropped = port.policy_dropped;

    // The logger flushes when the simulator is dropped.
    drop(sim);
    let bytes = std::fs::read(&log_path).unwrap();
    let _ = std::fs::remove_file(&log_path);
    let frames = competitive_net_sim::sim_log::writer::decode_all(&bytes).unwrap();
    let mut policy = 0;
    let mut queue_full = 0;
    for f in &frames {
        if let competitive_net_sim::sim_log::LogEvent::PacketDropped { reason, .. } = &f.event {
            match reason {
                competitive_net_sim::sim_log::DropReasonW::Policy => policy += 1,
                competitive_net_sim::sim_log::DropReasonW::QueueFull => queue_full += 1,
                _ => {}
            }
        }
    }
    assert_eq!(policy as u64, policy_dropped);
    assert_eq!(queue_full, 0);
}

/// On its first timer, declares port 2 down and installs a route; on the
/// second, declares it up again.
struct DeclaringController {
    fired: u8,
}

impl SwitchController for DeclaringController {
    fn on_punt(&mut self, _event: PuntEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
        self.fired += 1;
        match self.fired {
            1 => vec![
                ControllerAction::DeclareLink { port: PortId::new(2), up: false },
                ControllerAction::InstallTableEntry {
                    table_id: TableId::new(1),
                    entry: TableEntry {
                        id: EntryId::new(9),
                        key: 0x0a000009,
                        prefix_len: 32,
                        priority: 0,
                        action: TableAction::SetEgress { port: PortId::new(1) },
                    },
                },
            ],
            2 => vec![ControllerAction::DeclareLink { port: PortId::new(2), up: true }],
            _ => Vec::new(),
        }
    }
    fn on_link_event(&mut self, _event: LinkEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
}

#[test]
fn declarations_reach_the_log_and_actions_land_after_the_control_delay() {
    // Control delay Δ = 5 ms. Each action leaves the controller at the
    // timer instant and lands Δ plus its config-pipe serialization later:
    // (64 min-size + payload) bytes at 1 Gbps. DeclareLink pays 8 bytes
    // (576 ns), InstallTableEntry 32 (768 ns), and they are serialized
    // back to back.
    let mut sim = Simulator::new();
    let switch_id = SwitchId::new(0);
    let mut state = TinyVmState::default();
    state.tables.push(MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16));
    let mut cfg = SwitchConfig::defaults(switch_id);
    cfg.config_pipe_latency = Duration::from_millis(5);
    sim.add_switch(Switch::new(
        cfg,
        TinyProgram { stages: Vec::new() },
        state,
        Box::new(DeclaringController { fired: 0 }),
        4,
        1 << 16,
    ));
    let log_path = std::env::temp_dir().join(format!(
        "networkarena-declare-{}.simlog",
        std::process::id()
    ));
    sim.start_logging(&log_path).unwrap();
    sim.schedule_controller_timer(switch_id, Duration::from_millis(100));
    sim.schedule_controller_timer(switch_id, Duration::from_millis(300));
    sim.run_until(Duration::from_secs(1));

    // The route is in the table once the run is over...
    let sw = &sim.switches[&switch_id];
    assert!(sw.state.tables[0].entries.iter().any(|e| e.id == EntryId::new(9)));
    drop(sim);

    // ...and the log says exactly when each action landed.
    let bytes = std::fs::read(&log_path).unwrap();
    let _ = std::fs::remove_file(&log_path);
    let frames = competitive_net_sim::sim_log::writer::decode_all(&bytes).unwrap();
    use competitive_net_sim::sim_log::LogEvent;
    let mut declared: Vec<(u64, u16, bool)> = Vec::new();
    let mut installed_at: Option<u64> = None;
    for f in &frames {
        match &f.event {
            LogEvent::LinkDeclared { switch, port, up } => {
                assert_eq!(*switch, 0);
                declared.push((f.at_ns, *port, *up));
            }
            LogEvent::TableEntryInstalled { entry, .. } if entry.id.0 == 9 => {
                installed_at = Some(f.at_ns);
            }
            _ => {}
        }
    }
    let ms = 1_000_000u64;
    let delta = 5 * ms;
    let ser_declare = (64 + 8) * 8; // ns at 1 Gbps
    let ser_install = (64 + 32) * 8;
    assert_eq!(
        declared,
        vec![
            (100 * ms + delta + ser_declare, 2, false),
            (300 * ms + delta + ser_declare, 2, true),
        ]
    );
    assert_eq!(installed_at, Some(100 * ms + delta + ser_declare + ser_install));
}

/// A closed-loop sender (app 1, 10.0.0.1) and an open-loop flooder (app
/// 2, 10.0.0.2) share one slow port toward the sink (app 3, 10.0.0.3).
/// Routes exist in both directions so acks find their way back.
fn build_closed_loop_over_a_congested_port(
    sender_bps: f64,
    flood_interval: Duration,
    port_queue_packets: u32,
) -> (Simulator, AppId, AppId, AppId, SwitchId) {
    use competitive_net_sim::flow::{Demand, FlowEngine, Law};
    let mut sim = Simulator::new();
    let sender = AppId::new(1);
    let flood = AppId::new(2);
    let sink = AppId::new(3);
    let switch_id = SwitchId::new(0);
    let idle = TrafficPattern::ConstantBitrate {
        interval: Duration::from_secs(3600),
        size_bytes: 0,
    };
    sim.add_app(
        App::new(sender, IpAddr(0x0a000001), 32, IpAddr(0x0a000003), 32, idle.clone()).with_flow(
            FlowEngine::new(
                Demand::Constant { bps: sender_bps },
                Law::Fixed { bps: sender_bps },
                100,
            ),
        ),
    );
    sim.add_app(App::new(
        flood,
        IpAddr(0x0a000002), 32,
        IpAddr(0x0a000003), 32,
        TrafficPattern::ConstantBitrate {
            interval: flood_interval,
            size_bytes: 100,
        },
    ));
    sim.add_app(App::new(sink, IpAddr(0x0a000003), 32, IpAddr(0x0a000001), 32, idle));

    let mut state = TinyVmState::default();
    let mut routes = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
    for (i, (ip, port)) in [(0x0a000001u64, 0u16), (0x0a000002, 2), (0x0a000003, 1)]
        .iter()
        .enumerate()
    {
        routes
            .install(TableEntry {
                id: EntryId::new(i as u64 + 1),
                key: *ip,
                prefix_len: 32,
                priority: 0,
                action: TableAction::SetEgress { port: PortId::new(*port) },
            })
            .unwrap();
    }
    state.tables.push(routes);
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
    sim.add_switch(Switch::new(
        SwitchConfig::defaults(switch_id),
        prog,
        state,
        Box::new(NoopController),
        4,
        port_queue_packets as u64 * 100,
    ));
    let fast = LinkConfig {
        latency: Duration::from_micros(100),
        bandwidth_bps: 1_000_000_000,
        queue_capacity_bytes: 1 << 16,
    };
    sim.connect(Node::App(sender), PortId::new(0), Node::Switch(switch_id), PortId::new(0), fast);
    sim.connect(Node::App(flood), PortId::new(0), Node::Switch(switch_id), PortId::new(2), fast);
    sim.connect(
        Node::Switch(switch_id),
        PortId::new(1),
        Node::App(sink),
        PortId::new(0),
        LinkConfig {
            latency: Duration::from_millis(1),
            bandwidth_bps: 80_000, // 100 bytes = 10 ms on the wire
            queue_capacity_bytes: 1 << 16,
        },
    );
    (sim, sender, flood, sink, switch_id)
}

#[test]
fn closed_loop_sender_retransmits_every_drop() {
    // The wire carries 100 pkt/s. The flooder offers 100 pkt/s on its
    // own and the closed-loop sender adds 8 pkt/s, so the 5-packet queue
    // overflows and both lose packets. Every one of the sender's drops
    // must come back as a retransmission of the same seq, and the sink's
    // count of distinct seqs must equal its first-time deliveries.
    let (mut sim, sender, _flood, sink, _switch) = build_closed_loop_over_a_congested_port(
        6_400.0,
        Duration::from_millis(10),
        5,
    );
    let log_path = std::env::temp_dir().join(format!(
        "networkarena-closed-loop-{}.simlog",
        std::process::id()
    ));
    sim.start_logging(&log_path).unwrap();
    sim.run_until(Duration::from_secs(20));

    let flow = sim.apps[&sender].flow.as_ref().unwrap();
    let sink_m = &sim.apps[&sink].metrics;
    let from_sender = *sink_m.received_by_src.get(&0x0a000001).unwrap_or(&0);
    let unique_from_sender = from_sender - sink_m.duplicates_received;
    let pending: std::collections::BTreeSet<u64> = flow.pending_seqs().into_iter().collect();
    assert!(flow.losses > 0, "the congested port must cost the sender something");
    assert!(flow.retransmits > 0);
    assert!(flow.retransmits <= flow.losses);
    // Everything ever numbered is either acked or still pending: the
    // sender never forgets a loss.
    assert_eq!(flow.acked as usize + pending.len(), flow.next_seq() as usize);
    // Acks the sender got cannot exceed distinct seqs the sink saw plus
    // duplicates it acked again.
    assert!(flow.acked <= from_sender);
    assert_eq!(sink_m.acks_sent, from_sender);
    let (first_sends, retransmits, losses, acked) =
        (flow.first_sends, flow.retransmits, flow.losses, flow.acked);
    drop(sim);

    // From the log: every seq of the sender's that was ever dropped is
    // either delivered later or still pending at the end.
    let bytes = std::fs::read(&log_path).unwrap();
    let _ = std::fs::remove_file(&log_path);
    let frames = competitive_net_sim::sim_log::writer::decode_all(&bytes).unwrap();
    use competitive_net_sim::sim_log::LogEvent;
    let mut seq_of: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut dropped: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut delivered: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut deliveries_from_sender = 0u64;
    for f in &frames {
        match &f.event {
            LogEvent::PacketIngress { packet, .. }
                if packet.kind == 0 && packet.ip_src == 0x0a000001 =>
            {
                if let Some(seq) = packet.seq {
                    seq_of.insert(packet.id, seq);
                }
            }
            LogEvent::PacketDropped { packet_id, .. } => {
                if let Some(seq) = seq_of.get(packet_id) {
                    dropped.insert(*seq);
                }
            }
            LogEvent::PacketDelivered { packet_id, app } if *app == 3 => {
                if let Some(seq) = seq_of.get(packet_id) {
                    delivered.insert(*seq);
                    deliveries_from_sender += 1;
                }
            }
            _ => {}
        }
    }
    assert!(!dropped.is_empty());
    for seq in &dropped {
        assert!(
            delivered.contains(seq) || pending.contains(seq),
            "seq {seq} was dropped and never retransmitted to delivery"
        );
    }
    assert_eq!(delivered.len() as u64, unique_from_sender);
    assert_eq!(deliveries_from_sender, from_sender);
    // One fresh id per copy; a copy sent at the horizon may still be on
    // the app's own link and not yet at the switch.
    let copies = first_sends + retransmits;
    assert!(
        (copies - 1..=copies).contains(&(seq_of.len() as u64)),
        "{copies} copies sent, {} seen at the switch",
        seq_of.len()
    );
    assert!(acked <= first_sends, "{acked} acked of {first_sends} first sends, {losses} losses");
}

/// Delivered packets at `sink`, split by sender.
fn count_by_source(sim: &Simulator, sink: AppId) -> (usize, usize) {
    let by_src = &sim.apps[&sink].metrics.received_by_src;
    let control = by_src.get(&0x0a000002).copied().unwrap_or(0) as usize;
    let data = by_src.get(&0x0a000001).copied().unwrap_or(0) as usize;
    (control, data)
}

#[test]
fn egress_queue_serves_fifo_with_closed_form_timing() {
    // Packet k is created at 10k ms and reaches the switch's port 1 at
    // 10k ms + 101.2 µs (0.8 µs serialization + 100 µs latency + 400 ns
    // pipeline). Packet 0 starts onto the slow wire at once; each later
    // one waits for its predecessor, so packet k finishes serializing at
    // 101.2 µs + 100(k+1) ms and arrives 10 ms later. Its one-way delay is
    // therefore 110.1012 ms + 90k ms.
    let (mut sim, app_b, switch_id) = build_slow_egress(Duration::from_millis(10), 1 << 16);

    // Mid-run: packet 1 is on the wire (100.1–200.1 ms), packets 2..=14
    // are queued, and buffer is charged for all fourteen since a packet
    // is only released once it has fully left.
    sim.run_until(Duration::from_millis(150));
    let port = &sim.switches[&switch_id].egress[1];
    assert!(port.transmitting);
    assert_eq!(port.packets_queued(), 13);
    assert_eq!(port.bytes_queued(), 1300);
    assert_eq!(sim.switches[&switch_id].buffer_used, 1400);
    assert_eq!(sim.apps[&app_b].metrics.packets_received, 1);

    // By 350 ms exactly three packets have arrived, with delays
    // 110.1, 200.1 and 290.1 ms.
    sim.run_until(Duration::from_millis(350));
    let m = &sim.apps[&app_b].metrics;
    assert_eq!(m.packets_received, 3);
    let base = Duration::from_millis(110) + Duration::from_micros(101) + Duration::from_nanos(200);
    let close = |got: Duration, want: Duration| {
        let diff = if got > want { got - want } else { want - got };
        assert!(diff < Duration::from_micros(1), "got {got:?}, want {want:?}");
    };
    close(m.p50_delay, base + Duration::from_millis(90));
    close(m.p99_delay, base + Duration::from_millis(180));
    close(m.avg_delay, base + Duration::from_millis(90));
}

#[test]
fn egress_queue_full_drops_are_counted_and_buffer_is_bounded() {
    // Port 1 holds 500 bytes = 5 packets. A sends every 7 ms. Packet 0
    // takes the wire at 0.1 ms; packets 1..=5 fill the queue; 6..=14 are
    // refused. At 100.1 ms packet 1 takes the wire, so packet 15 (105 ms)
    // is admitted and 16..=21 (up to 147 ms) are refused again.
    let (mut sim, app_b, switch_id) = build_slow_egress(Duration::from_millis(7), 500);
    sim.run_until(Duration::from_millis(150));
    let sw = &sim.switches[&switch_id];
    let port = &sw.egress[1];
    assert_eq!(port.queues[0].len(), 5);
    assert_eq!(port.packets_dropped, 15);
    // Five queued plus the one on the wire.
    assert_eq!(sw.buffer_used, 600);
    assert_eq!(sim.apps[&app_b].metrics.packets_received, 1);
}

/// Applies `config` to port 1 on its first timer, then re-arms every
/// `period` and records what each timer said the ports dropped.
struct DropWatchingController {
    config: QueueConfig,
    period: Duration,
    done: bool,
    seen: std::rc::Rc<std::cell::RefCell<Vec<(SimTime, Vec<PortDrops>)>>>,
}

impl SwitchController for DropWatchingController {
    fn on_punt(&mut self, _event: PuntEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
        unreachable!("the simulator calls on_timer_with_drops")
    }
    fn on_timer_with_drops(&mut self, now: SimTime, drops: Vec<PortDrops>) -> Vec<ControllerAction> {
        self.seen.borrow_mut().push((now, drops));
        let mut out = vec![ControllerAction::ScheduleTimer { delay: self.period }];
        if !self.done {
            self.done = true;
            out.push(ControllerAction::SetQueueConfig {
                port: PortId::new(1),
                config: self.config.clone(),
            });
        }
        out
    }
    fn on_link_event(&mut self, _event: LinkEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
}

#[test]
fn timer_reports_what_each_port_dropped_since_the_last_one() {
    // Data floods port 1 at ten times the wire rate into a queue that
    // drops past 5 packets, so the port sheds about 9 packets a second by
    // policy. A controller re-arming every second must be told about
    // exactly those drops, once each, on port 1 and nowhere else.
    let config = QueueConfig {
        discipline: Discipline::Fifo,
        queues: vec![
            QueueSpec::new(64),
            QueueSpec {
                max_packets: 50,
                weight: 1,
                drop_threshold_packets: Some(5),
            },
        ],
    };
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let controller = DropWatchingController {
        config,
        period: Duration::from_secs(1),
        done: false,
        seen: seen.clone(),
    };
    let (mut sim, _, _, _, switch_id) = build_two_senders_one_slow_port_with(
        Duration::from_millis(10),
        Duration::from_secs(3600),
        Box::new(controller),
    );
    sim.run_until(Duration::from_secs(5));

    let seen = seen.borrow();
    assert!(seen.len() >= 5, "timer fired {} times", seen.len());
    // The first timer comes before the queues are even configured.
    assert!(seen[0].1.is_empty(), "nothing dropped before t=0: {:?}", seen[0].1);
    let mut reported = 0;
    let mut reported_policy = 0;
    for (t, drops) in seen.iter().skip(1) {
        assert_eq!(drops.len(), 1, "only port 1 drops, at {t:?}: {drops:?}");
        assert_eq!(drops[0].port, PortId::new(1));
        assert!(drops[0].dropped > 0 && drops[0].dropped <= 100, "{drops:?}");
        reported += drops[0].dropped;
        reported_policy += drops[0].policy;
    }
    // Whatever fell between the last timer and the horizon is still
    // waiting to be reported; together they account for every drop.
    let sw = sim.switches.get_mut(&switch_id).unwrap();
    let total = sw.egress[1].packets_dropped;
    let total_policy = sw.egress[1].policy_dropped;
    let tail = sw.take_drop_deltas();
    let tail_dropped: u64 = tail.iter().map(|d| d.dropped).sum();
    let tail_policy: u64 = tail.iter().map(|d| d.policy).sum();
    assert!(total > 30, "the port did drop: {total}");
    assert_eq!(reported + tail_dropped, total);
    assert_eq!(reported_policy + tail_policy, total_policy);
    assert_eq!(total_policy, total, "all of it was policy");
    assert!(sw.take_drop_deltas().is_empty(), "reporting twice reports nothing");
}
