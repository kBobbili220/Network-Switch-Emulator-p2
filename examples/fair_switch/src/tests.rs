fn paths(graph: &Graph, source: u32, destination: u32) -> Vec<Vec<u32>> {
    path_menu(graph, source, destination, 2)
}
fn greedy(demands: &[f64], menus: &[Vec<Vec<u32>>], capacity: &BTreeMap<Link, f64>) -> Vec<Vec<u32>> {
    let mut work = GreedyWork::new(demands, menus);
    work.advance(menus, capacity, menus.len());
    work.chosen
}

use super::*;
fn graph() -> Graph {
    BTreeMap::from([
        (0, vec![(1, 2_000_000), (2, 10_000_000)]),
        (1, vec![(0, 2_000_000), (3, 10_000_000)]),
        (2, vec![(0, 10_000_000), (3, 10_000_000)]),
        (3, vec![(1, 10_000_000), (2, 10_000_000)]),
    ])
}
#[test]
fn paths_are_simple_and_include_detours() {
    let p = paths(&graph(), 0, 1);
    assert_eq!(p, vec![vec![0, 1], vec![0, 2, 3, 1]]);
    assert!(p.iter().all(|p| p.iter().collect::<BTreeSet<_>>().len() == p.len()));
}
#[test]
fn progressive_filling_releases_small_demands() {
    let a = shares(&[100_000.0, f64::INFINITY, f64::INFINITY], &vec![vec![0, 1]; 3], &BTreeMap::from([((0, 1), 2_000_000.0)]));
    assert_eq!(a, vec![100_000.0, 950_000.0, 950_000.0]);
}
#[test]
fn progressive_filling_freezes_only_bottleneck_users() {
    let a = shares(
        &[f64::INFINITY; 3],
        &[vec![0, 1, 2], vec![0, 1], vec![3, 2]],
        &BTreeMap::from([((0, 1), 6.0), ((1, 2), 2.0), ((3, 2), 9.0)]),
    );
    assert_eq!(a, vec![2.0, 4.0, 9.0]);
}
#[test]
fn acknowledgements_cannot_overwrite_the_data_peer() {
    let mut me = switch(1);
    let packet = |reason, destination| PuntEvent {
        now_ns: 0,
        switch_id: 0,
        ingress_port: APP,
        reason,
        packet_size: 200,
        ip_src: 0,
        ip_dst: destination,
        ip_proto: 0,
        ip_ttl: 64,
        payload: Vec::new(),
    };
    me.on_punt(packet(PuntReason::Custom(91), 10));
    me.on_punt(packet(PuntReason::NoRoute, 20));
    assert_eq!(me.local.destinations[&0], 10);
}
#[test]
fn greedy_uses_the_wide_detour() {
    let p = paths(&graph(), 0, 1);
    let chosen = greedy(
        &[f64::INFINITY],
        &[p],
        &BTreeMap::from([((0, 1), 2e6), ((0, 2), 10e6), ((2, 3), 10e6), ((3, 1), 10e6), ((u32::MAX, 0), 10e6), ((1, u32::MAX), 10e6)]),
    );
    assert_eq!(chosen, vec![vec![0, 2, 3, 1]]);
}
fn switch(n: usize) -> FairSwitch {
    FairSwitch::init_with(InitInput {
        switch_id: 0,
        local_ports: vec![0, 1, 2, 3, APP],
        port_capacity_bps: vec![10_000_000; 5],
        customers: (0..n)
            .map(|i| Customer {
                prefix_addr: i as u32,
                prefix_len: 32,
                cca: if i % 2 == 0 { Cca::Aimd } else { Cca::RateMatch },
                ask_bps: None,
            })
            .collect(),
    })
    .0
}
#[test]
fn reservations_include_control_and_transmitting_packets() {
    let me = switch(15);
    let routes: Vec<_> = (0..15)
        .map(|i| Route { id: i as u64 + 1, source: 1, key: i as u32, destination: 0, customer: Some(i), ack: false, nodes: vec![1, 0] })
        .collect();
    let caps = me.allocate(&routes);
    assert_eq!(caps.len(), 15);
    assert!(caps.values().all(|n| *n >= 1));
    assert!(caps.values().sum::<u32>() + 3 * me.ports.len() as u32 + 1 <= 40);
}
#[test]
fn malformed_frames_and_stale_commits_are_ignored() {
    let mut me = switch(1);
    me.receive(9, 1, Message::Advance { epoch: 100, phase: 1 });
    assert!(me.transaction.is_none());
    me.prepare(1, vec![Route { id: 1, source: 0, key: 0, destination: 0, customer: Some(0), ack: false, nodes: vec![0, 1, 0] }]);
    assert!(me.transaction.is_none());
}
#[test]
fn prepare_overtaking_retire_waits_for_the_previous_epoch() {
    let mut me = switch(1);
    let route = Route { id: 1, source: 0, key: 0, nodes: vec![0], customer: Some(0), ..Route::default() };
    me.configs.clear();
    me.prepare(1, vec![route.clone()]);
    me.configs.clear();
    me.activate(1);
    let next = Route { id: 2, ..route };
    me.prepare(2, vec![next.clone()]);
    assert_eq!(me.transaction.as_ref().unwrap().epoch, 1);
    assert!(me.pending_prepare.is_some());
    me.receive(0, 1, Message::Advance { epoch: 1, phase: 2 });
    me.configs.clear();
    me.finish();
    assert_eq!(me.current[0].id, 1);
    assert_eq!(me.transaction.as_ref().unwrap().epoch, 2);
    assert!(!me.configs.is_empty(), "new preparation waits for queued retirement and reservation actions");
    me.receive(0, 2, Message::Advance { epoch: 1, phase: 2 });
    assert_eq!(me.transaction.as_ref().unwrap().phase, 0, "stale retirement cannot advance the new epoch");
    me.receive(1, 3, Message::Ack { epoch: 2, phase: 1 });
    assert!(me.transaction.as_ref().unwrap().acknowledged.is_empty(), "ACKs must match both epoch and phase");
}
#[test]
fn ingress_replacement_has_priority_and_waits_for_retirement() {
    let mut me = switch(1);
    me.neighbours.insert(0, (1, 0));
    me.neighbours.insert(1, (2, 0));
    let old = Route { id: 1, source: 0, key: 42, destination: 43, customer: Some(0), ack: false, nodes: vec![0, 1] };
    let new = Route { id: 2, nodes: vec![0, 2, 1], ..old.clone() };
    me.current = vec![old];
    me.caps = me.allocate(&me.current);
    me.configs.clear();
    me.prepare(2, vec![new.clone()]);
    me.configs.clear();
    me.activate(2);
    let installs: Vec<_> = me
        .configs
        .iter()
        .filter_map(|a| match a {
            Action::InstallTableEntry { table_id, entry } if table_id.0 == INGRESS => Some(entry),
            _ => None,
        })
        .collect();
    assert_eq!(installs.len(), 1);
    assert_eq!((installs[0].id.0, installs[0].key, installs[0].priority), (2, 42, 2));
    me.configs.clear();
    me.now = 100_000_000_000;
    me.finish();
    assert_eq!(me.current[0].id, 1, "elapsed time alone cannot retire a path");
    me.transaction.as_mut().unwrap().phase = 2;
    me.finish();
    assert_eq!(me.current, vec![new]);
    assert!(
        me.configs.iter().any(|a| matches!(a, Action::DeleteTableEntry { table_id, entry_id } if table_id.0 == INGRESS && entry_id.0 == 1))
    );
}
#[test]
fn output_is_bounded_under_control_backlog() {
    let mut me = switch(15);
    for _ in 0..100 {
        for port in [0, 1, 2, 3] {
            me.packets.push_back(actions::inject_packet(port, 0, 0, CONTROL, 1, vec![0; 420]));
        }
    }
    for _ in 0..80 {
        let out = me.output();
        assert!(postcard::to_allocvec(&out).unwrap().len() <= 4096);
        assert!(matches!(out.last(), Some(Action::ScheduleTimer { .. })));
    }
    assert!(me.packets.is_empty());
    assert!(me.configs.is_empty());
}
#[test]
fn pipeline_fits_machine_and_branch_targets_are_valid() {
    for n in [0, 15] {
        let (_, setup) = FairSwitch::init_with(InitInput {
            switch_id: 0,
            local_ports: vec![APP],
            port_capacity_bps: vec![10_000_000],
            customers: switch(n).customers,
        });
        let program = setup.into_wire().program;
        assert!(program.stages.len() <= 8);
        for stage in program.stages {
            for (i, instr) in stage.instrs.iter().enumerate() {
                if let Instr::BranchIf { target, .. } = instr {
                    assert!(*target as usize > i && (*target as usize) < stage.instrs.len());
                }
            }
        }
    }
}
