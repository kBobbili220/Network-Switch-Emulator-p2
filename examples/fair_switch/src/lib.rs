//! A2 switch. Customer identity, queue identity, and path identity are separate.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use switch_program_sdk::*;

const APP: u16 = 100; // Documented machine endpoint port, not a world-specific route.
const HELLO: u8 = 89;
const CONTROL: u8 = 90;
const TICK: u64 = 50_000_000;
const EVAL: u64 = 500_000_000;
const DEAD: u64 = 350_000_000;
const PROTO: u32 = 1;
const CUSTOMER: u32 = 2;
const INGRESS: u32 = 3;
const ACK: u32 = 4;
const PATH: u32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Edge {
    port: u16,
    to: u32,
    capacity: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    edges: Vec<Edge>,
    app_capacity: u64,
    host: Option<u32>,
    destinations: BTreeMap<u32, u32>,
    drops: BTreeMap<u16, Vec<u32>>,
    congested: BTreeSet<usize>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Route {
    id: u64,
    source: u32,
    key: u32,
    destination: u32,
    customer: Option<usize>,
    ack: bool,
    nodes: Vec<u32>,
}
impl Route {
    fn identity(&self) -> (u32, u32, bool) {
        (self.source, self.key, self.ack)
    }
    fn ingress_table(&self) -> u32 {
        if self.ack { ACK } else { INGRESS }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Message {
    State(Snapshot),
    Prepare { epoch: u64, routes: Vec<Route> },
    Ack { epoch: u64, phase: u8 },
    Advance { epoch: u64, phase: u8 },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Frame {
    origin: u32,
    seq: u64,
    part: u16,
    count: u16,
    bytes: Vec<u8>,
}
struct Assembly {
    count: u16,
    parts: BTreeMap<u16, Vec<u8>>,
    at: u64,
}
#[derive(Default)]
struct Transaction {
    epoch: u64,
    routes: Vec<Route>,
    participants: BTreeSet<u32>,
    acknowledged: BTreeSet<u32>,
    settled_at: u64,
    last_ack: u64,
    phase: u8, // 0: install paths, 1: activate ingress, 2: retire old paths.
}

#[derive(Default)]
pub struct FairSwitch {
    id: u32,
    ports: Vec<u16>,
    capacities: BTreeMap<u16, u64>,
    customers: Vec<Customer>,
    neighbours: BTreeMap<u16, (u32, u64)>,
    failed: BTreeSet<u16>,
    local: Snapshot,
    states: BTreeMap<u32, Snapshot>,
    state_seq: BTreeMap<u32, u64>,
    drops: BTreeMap<u16, Vec<u32>>,
    now: u64,
    last_eval: u64,
    seq: u64,
    epoch: u64,
    next_path: u64,
    assemblies: BTreeMap<(u32, u64), Assembly>,
    packets: VecDeque<Action>,
    configs: VecDeque<Action>,
    current: Vec<Route>,
    transaction: Option<Transaction>,
    pending_prepare: Option<(u64, Vec<Route>)>,
    last_retire: u64,
    caps: BTreeMap<(u16, usize), u32>,
    cooldown: BTreeMap<usize, u64>,
    optimization_cursor: usize,
    needs_plan: bool,
    cached_graph: Graph,
    path_cache: BTreeMap<(u32, u32), Vec<Vec<u32>>>,
    baseline_model: Vec<(u32, u32)>,
    greedy_work: Option<GreedyWork>,
    dirty: bool,
}
fn entry(id: u64, key: u64, action: TableAction) -> TableEntry {
    TableEntry { id: wire::EntryIdW(id), key, prefix_len: 64, priority: 0, action }
}
fn meta(id: u64, key: u64, value: u64) -> TableEntry {
    entry(id, key, TableAction::SetMeta { key: wire::MetaKeyW(20), value })
}

impl FairSwitch {
    fn customer(&self, ip: u32) -> Option<usize> {
        self.customers.iter().position(|c| {
            let mask = u32::MAX.checked_shl(32 - c.prefix_len as u32).unwrap_or(0);
            ip & mask == c.prefix_addr & mask
        })
    }
    fn leader(&self) -> u32 {
        self.states.keys().copied().chain([self.id]).min().unwrap()
    }
    fn send(&mut self, msg: Message) {
        let bytes = postcard::to_allocvec(&msg).expect("bounded control message");
        assert!(bytes.len() <= 32_000);
        self.seq += 1;
        let count = bytes.len().div_ceil(400) as u16;
        for (part, bytes) in bytes.chunks(400).enumerate() {
            let frame = Frame { origin: self.id, seq: self.seq, part: part as u16, count, bytes: bytes.to_vec() };
            self.flood(&frame, None);
        }
    }
    fn flood(&mut self, frame: &Frame, skip: Option<u16>) {
        let payload = postcard::to_allocvec(frame).unwrap();
        for (&port, _) in &self.neighbours {
            if Some(port) != skip && !self.failed.contains(&port) {
                self.packets.push_back(actions::inject_packet(port, self.id, 0xe0000005, CONTROL, 1, payload.clone()));
            }
        }
    }
    fn advertise(&mut self) {
        self.needs_plan |= self.dirty;
        self.local.edges = self
            .neighbours
            .iter()
            .filter(|(p, _)| !self.failed.contains(p))
            .map(|(&port, &(to, _))| Edge { port, to, capacity: self.capacities[&port] })
            .collect();
        self.states.insert(self.id, self.local.clone());
        self.send(Message::State(self.local.clone()));
        self.dirty = false;
    }
    fn port_for(&self, route: &Route) -> Option<u16> {
        let at = route.nodes.iter().position(|&n| n == self.id)?;
        if at + 1 == route.nodes.len() {
            Some(APP)
        } else {
            self.neighbours.iter().find(|(_, n)| n.0 == route.nodes[at + 1]).map(|(&p, _)| p)
        }
    }
    fn slots(&self, routes: &[Route]) -> BTreeSet<(u16, usize)> {
        routes.iter().filter_map(|r| Some((self.port_for(r)?, r.customer?))).collect()
    }
    fn can_transition(&self, routes: &[Route]) -> bool {
        self.states.iter().all(|(&node, state)| {
            let slots = |routes: &[Route]| -> BTreeSet<(u32, usize)> {
                routes
                    .iter()
                    .filter_map(|r| {
                        let customer = r.customer?;
                        let at = r.nodes.iter().position(|&id| id == node)?;
                        Some((r.nodes.get(at + 1).copied().unwrap_or(u32::MAX), customer))
                    })
                    .collect()
            };
            let old = slots(&self.current);
            let new = slots(routes);
            let reserved_ports = 3 * (state.edges.len() as u32 + 1);
            let budget = 40u32.saturating_sub(reserved_ports + 1);
            let old_targets: u32 = old.iter().map(|(_, i)| if self.customers[*i].cca == Cca::Aimd { 4 } else { 2 }).sum();
            let held = old_targets.min(budget).max(old.len() as u32);
            held + new.difference(&old).count() as u32 + reserved_ports <= 40
        })
    }
    fn budget(&self) -> u32 {
        40u32.saturating_sub(3 * self.ports.len() as u32 + 1)
    }
    fn allocate(&self, routes: &[Route]) -> BTreeMap<(u16, usize), u32> {
        let mut caps: BTreeMap<_, _> = self.slots(routes).into_iter().map(|s| (s, 1)).collect();
        let mut left = self.budget().saturating_sub(caps.len() as u32);
        for (cca, target) in [(Cca::Aimd, 4), (Cca::RateMatch, 2)] {
            for _ in 1..target {
                for (&(_, i), cap) in caps.iter_mut() {
                    if left > 0 && self.customers[i].cca == cca && *cap < target {
                        *cap += 1;
                        left -= 1;
                    }
                }
            }
        }
        caps
    }
    fn configure(&mut self, caps: BTreeMap<(u16, usize), u32>) {
        for &port in &self.ports {
            let mut specs = vec![QueueSpec::new(2).weight(4)];
            for i in 0..self.customers.len() {
                specs.push(QueueSpec::new(*caps.get(&(port, i)).unwrap_or(&0)));
            }
            if self.customers.is_empty() {
                specs.push(QueueSpec::new((self.budget() / self.ports.len().max(1) as u32).max(1)));
            }
            self.configs.push_back(actions::set_queues(port, Discipline::Drr, specs));
        }
        self.caps = caps;
    }
    fn prepare(&mut self, epoch: u64, routes: Vec<Route>) {
        if let Some(t) = &self.transaction {
            if epoch > t.epoch {
                self.pending_prepare = Some((epoch, routes));
            }
            return;
        }
        if routes.len() > 900 || routes.iter().any(|r| r.nodes.len() > 15 || r.nodes.iter().collect::<BTreeSet<_>>().len() != r.nodes.len())
        {
            return;
        }
        let mut caps = if self.current.is_empty() { self.allocate(&routes) } else { self.caps.clone() };
        let new_slots: Vec<_> = self.slots(&routes).into_iter().filter(|s| !caps.contains_key(s)).collect();
        for &slot in &new_slots {
            caps.insert(slot, 1);
        }
        if caps.values().sum::<u32>() + 3 * self.ports.len() as u32 > 40 {
            return;
        }
        // Grow new queues using unreserved space, preserving old reservations.
        for slot in new_slots {
            let target = if self.customers[slot.1].cca == Cca::Aimd { 4 } else { 2 };
            while caps[&slot] < target && caps.values().sum::<u32>() + 3 * (self.ports.len() as u32) < 40 {
                *caps.get_mut(&slot).unwrap() += 1;
            }
        }
        self.configure(caps);
        for r in &routes {
            if let Some(port) = self.port_for(r) {
                if !self.current.iter().any(|old| old.id == r.id) {
                    self.configs.push_back(actions::install_route(PATH, r.id, r.id, 64, port));
                }
            }
        }
        self.epoch = epoch;
        self.transaction = Some(Transaction {
            epoch,
            routes,
            participants: self.states.keys().copied().collect(),
            settled_at: self.now + 2 * TICK,
            last_ack: self.now,
            ..Transaction::default()
        });
    }
    fn activate(&mut self, epoch: u64) -> Option<()> {
        let t = self.transaction.as_mut().filter(|t| t.epoch == epoch && t.phase == 0)?;
        for r in &t.routes {
            if r.source == self.id && !self.current.iter().any(|old| old.id == r.id) {
                // Appended entries need higher priority until the old path drains.
                let mut e = meta(r.id, r.key as u64, r.id);
                e.priority = epoch.min(i32::MAX as u64) as i32;
                self.configs.push_back(actions::install_entry(r.ingress_table(), e));
            }
        }
        t.phase = 1;
        t.acknowledged.clear();
        t.settled_at = self.now + 2 * TICK;
        t.last_ack = self.now;
        Some(())
    }
    fn drain_bound(&self) -> u64 {
        if self.current.is_empty() {
            return 0;
        }
        // Full-queue DRR service bound, plus propagation and a drain margin.
        let quantum = 5 * (4 + self.customers.len() as u64) * 8_000_000_000_000;
        let mut longest = 0;
        for route in &self.current {
            let mut delay = 0;
            for w in route.nodes.windows(2) {
                let capacity = self.states.get(&w[0]).and_then(|s| s.edges.iter().find(|e| e.to == w[1])).map_or(1_000_000, |e| e.capacity);
                delay += quantum / capacity.max(1) + 500_000;
            }
            longest = longest.max(delay);
        }
        longest + 250_000_000
    }
    fn finish(&mut self) -> Option<()> {
        let done = self.transaction.as_ref()?.phase == 2 && self.configs.is_empty();
        let t = done.then(|| self.transaction.take().unwrap())?;
        let keep: BTreeSet<_> = t.routes.iter().map(|r| r.id).collect();
        for old in &self.current {
            if !keep.contains(&old.id) {
                self.configs.push_back(actions::delete_entry(PATH, old.id));
                if old.source == self.id {
                    self.configs.push_back(actions::delete_entry(old.ingress_table(), old.id));
                }
            }
        }
        self.current = t.routes;
        let caps = self.allocate(&self.current);
        self.configure(caps);
        // PREPARE may overtake RETIRE on another flooded path. Stage it
        // until the previous epoch finishes; its READY is the retirement barrier.
        if let Some((epoch, routes)) = self.pending_prepare.take() {
            self.prepare(epoch, routes);
        }
        Some(())
    }
    fn announce_phase(&mut self, retry: bool) {
        let leader = self.id == self.leader();
        if retry && leader && self.last_retire > 0 {
            self.send(Message::Advance { phase: 2, epoch: self.last_retire });
        }
        let Some(t) = &mut self.transaction else { return };
        let epoch = t.epoch;
        let mut messages = Vec::new();
        if retry && leader {
            messages.push(if t.phase == 0 {
                Message::Prepare { epoch, routes: t.routes.clone() }
            } else {
                Message::Advance { phase: t.phase, epoch }
            });
        }
        if !self.configs.is_empty() {
            t.settled_at = self.now + 2 * TICK;
        } else if self.now >= t.settled_at && t.phase < 2 && (retry || !t.acknowledged.contains(&self.id)) {
            if t.acknowledged.insert(self.id) {
                t.last_ack = self.now;
            }
            messages.push(Message::Ack { phase: t.phase, epoch });
        }
        for message in messages {
            self.send(message);
        }
    }
    fn receive(&mut self, origin: u32, seq: u64, msg: Message) {
        match msg {
            Message::State(s) => {
                if self.state_seq.get(&origin).is_none_or(|&old| seq > old) {
                    self.state_seq.insert(origin, seq);
                    if self
                        .states
                        .get(&origin)
                        .is_none_or(|old| old.edges != s.edges || old.host != s.host || old.destinations != s.destinations)
                    {
                        self.needs_plan = true;
                    }
                    self.states.insert(origin, s);
                }
            }
            Message::Prepare { epoch, routes } if origin == self.leader() && epoch > self.epoch => self.prepare(epoch, routes),
            Message::Ack { epoch, phase } => {
                if let Some(t) = self.transaction.as_mut().filter(|t| t.epoch == epoch && t.phase == phase) {
                    if t.acknowledged.insert(origin) {
                        t.last_ack = self.now;
                    }
                }
            }
            Message::Advance { epoch, phase } if origin == self.leader() => {
                if phase == 1 {
                    self.activate(epoch);
                } else if phase == 2 {
                    if let Some(t) = self.transaction.as_mut().filter(|t| t.epoch == epoch) {
                        t.phase = 2;
                    }
                }
            }
            _ => {}
        }
    }
    fn control(&mut self, payload: &[u8], port: u16) -> Option<()> {
        let f = postcard::from_bytes::<Frame>(payload).ok()?;
        let valid = f.origin != self.id && f.count > 0 && f.count <= 80 && f.part < f.count && f.bytes.len() <= 400;
        valid.then_some(())?;
        let key = (f.origin, f.seq);
        (self.assemblies.contains_key(&key) || self.assemblies.len() < 512).then_some(())?;
        let a = self.assemblies.entry(key).or_insert(Assembly { count: f.count, parts: BTreeMap::new(), at: self.now });
        (a.count == f.count && !a.parts.contains_key(&f.part)).then_some(())?;
        a.parts.insert(f.part, f.bytes.clone());
        let bytes: Vec<u8> = if a.parts.len() == a.count as usize { a.parts.values().flatten().copied().collect() } else { Vec::new() };
        self.flood(&f, Some(port));
        if let Ok(message) = postcard::from_bytes(&bytes) {
            self.receive(f.origin, f.seq, message);
        }
        Some(())
    }
    fn output(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        let mut bytes = 16; // postcard vector length and the timer action
        while let Some(a) = self.configs.front() {
            let size = postcard::to_allocvec(a).unwrap().len();
            if bytes + size > 2400 {
                break;
            }
            bytes += size;
            out.push(self.configs.pop_front().unwrap());
        }
        let mut sent = BTreeMap::<u16, usize>::new();
        let n = self.packets.len().min(128);
        for _ in 0..n {
            let a = self.packets.pop_front().unwrap();
            let port = match &a {
                Action::InjectPacket { port, .. } => port.0,
                _ => unreachable!(),
            };
            let size = postcard::to_allocvec(&a).unwrap().len();
            // Pace bytes, allowing small acknowledgements to progress promptly.
            if sent.get(&port).copied().unwrap_or(0) + size <= 1200 && bytes + size <= 3800 {
                bytes += size;
                *sent.entry(port).or_default() += size;
                out.push(a);
            } else {
                self.packets.push_back(a);
            }
        }
        out.push(actions::schedule_timer(if self.customers.is_empty() { TICK / 5 } else { TICK }));
        out
    }
}

impl SwitchProgram for FairSwitch {
    fn init(id: u32, ports: Vec<u16>) -> (Self, ProgramSetup) {
        Self::init_with(InitInput {
            switch_id: id,
            port_capacity_bps: vec![10_000_000; ports.len()],
            local_ports: ports,
            customers: Vec::new(),
        })
    }
    fn init_with(input: InitInput) -> (Self, ProgramSetup) {
        assert!(input.customers.len() <= 15, "at most fifteen customer queues");
        let mut setup = ProgramSetup::new();
        for t in [PROTO, CUSTOMER, INGRESS, ACK, PATH] {
            setup.declare_table(t, if t == CUSTOMER { MatchKind::Lpm } else { MatchKind::Exact }, 1024);
        }
        // TinyVM LPM measures prefixes within the low 32 IPv4 bits.
        for (i, customer) in input.customers.iter().enumerate() {
            let action = TableAction::SetQueue { queue: wire::QueueIdW(i as u16 + 1) };
            let mut row = entry(i as u64 + 1, customer.prefix_addr as u64, action);
            row.prefix_len = customer.prefix_len;
            setup.add_entry(CUSTOMER, row);
        }
        // Semicolons separate instructions; each line groups one pipeline operation.
        // Reset hop metadata; custom0 preserves the immutable path identifier.
        setup.set_program(
            text::parse_tiny_program(
                r#"
            stage alu=4 mem=1
                const r0, 0; storem m20, r0; storem m3, r0; queue 0
                load r0, ip_proto; table t1, r0 -> m1
            stage alu=4 mem=1
                load r0, kind; const r1, 4; eq r2, r0, r1; bif r2 -> 6
                load r0, ip_src; table t2, r0 -> m2; noop
            stage alu=12 mem=1
                load r0, custom0; bif r0 -> 11
                load r0, kind; const r1, 4; eq r2, r0, r1; bif r2 -> 11
                load r0, ip_src; table t3, r0 -> m3
                loadm r0, m3; bif r0 -> 11; punt 91; noop
            stage alu=8 mem=1
                load r0, custom0; bif r0 -> 10
                load r0, kind; const r1, 4; eq r2, r0, r1
                const r1, 1; xor r2, r2, r1; bif r2 -> 10
                load r0, ip_dst; table t4, r0 -> m4; noop
            stage alu=4 mem=0
                load r0, custom0; bif r0 -> 4
                loadm r0, m20; store custom0, r0; noop
            stage alu=0 mem=1
                load r0, custom0; table t5, r0 -> m5
        "#
                .replace(';', "\n")
                .as_str(),
            )
            .unwrap(),
        );
        if input.customers.is_empty() {
            let mut program = setup.clone().into_wire().program;
            program.stages[2].instrs[6] = Instr::LoadField { dst: wire::RegW(0), field: PacketField::IpDst };
            program.stages[1].instrs[4] = Instr::SetQueue { queue: wire::QueueIdW(1) };
            setup.set_program(program);
        }
        for p in [HELLO, CONTROL] {
            setup.add_entry(PROTO, entry(p as u64, p as u64, TableAction::Punt { reason: PuntReason::Custom(p as u32) }));
        }
        let mut me = Self {
            id: input.switch_id,
            ports: input.local_ports.clone(),
            capacities: input.local_ports.into_iter().zip(input.port_capacity_bps).collect(),
            customers: input.customers,
            next_path: 1,
            needs_plan: true,
            dirty: true,
            ..Self::default()
        };
        me.local.app_capacity = me.capacities.get(&APP).copied().unwrap_or(10_000_000);
        me.configure(BTreeMap::new());
        (me, setup)
    }
    fn on_punt(&mut self, ev: PuntEvent) -> Vec<Action> {
        self.now = ev.now_ns;
        if ev.ip_proto == HELLO && ev.payload.len() == 4 {
            let id = u32::from_le_bytes(ev.payload[..4].try_into().unwrap());
            if id != self.id {
                let discovered = self.neighbours.insert(ev.ingress_port, (id, self.now)).is_none();
                if self.failed.remove(&ev.ingress_port) || discovered {
                    self.dirty = true;
                    self.configs.push_back(actions::link_up(ev.ingress_port));
                }
            }
        } else if ev.ip_proto == CONTROL {
            self.control(&ev.payload, ev.ingress_port);
        } else if ev.ingress_port == APP {
            if self.local.host.replace(ev.ip_src) != Some(ev.ip_src) {
                self.dirty = true;
            }
            if !self.customers.is_empty()
                && matches!(ev.reason, PuntReason::Custom(91))
                && ev.ip_src != ev.ip_dst
                && self.local.destinations.insert(ev.ip_src, ev.ip_dst) != Some(ev.ip_dst)
            {
                self.dirty = true;
            }
        }
        Vec::new()
    }
    fn on_timer(&mut self, ev: TimerEvent) -> Vec<Action> {
        self.now = ev.now_ns;
        for d in ev.port_drops {
            let v = self.drops.entry(d.port).or_default();
            v.resize(v.len().max(d.queues.len()), 0);
            for (i, n) in d.queues.iter().enumerate() {
                v[i] = v[i].saturating_add(*n);
            }
        }
        let hello = self.id.to_le_bytes().to_vec();
        let hello_ticks = if self.customers.is_empty() { 2 } else { 5 };
        if self.now / TICK % hello_ticks == 0 || self.neighbours.is_empty() {
            for &port in &self.ports {
                if port != APP {
                    self.packets.push_front(actions::inject_packet(port, self.id, 0xe0000005, HELLO, 1, hello.clone()));
                }
            }
        }
        if self.customers.is_empty() {
            for (&port, &(_, last)) in &self.neighbours {
                if self.now > last + DEAD && self.failed.insert(port) {
                    self.dirty = true;
                    self.configs.push_back(actions::link_down(port));
                }
            }
        }
        self.assemblies.retain(|_, a| self.now < a.at + 10_000_000_000);
        if self.now >= self.last_eval + EVAL {
            self.last_eval = self.now;
            self.local.congested.clear();
            for (&port, counts) in &self.drops {
                for (q, &n) in counts.iter().enumerate().skip(1) {
                    if n > 0 && self.local.drops.get(&port).and_then(|v| v.get(q)).is_some_and(|&n| n > 0) {
                        self.local.congested.insert(q - 1);
                    }
                }
            }
            self.local.drops = std::mem::take(&mut self.drops);
            self.advertise();
            self.announce_phase(true);
            self.plan();
        } else if self.dirty {
            self.advertise();
        }
        if self.needs_plan && self.transaction.is_none() && self.now >= 150_000_000 {
            self.plan();
        }
        self.announce_phase(false);
        if self.id == self.leader() {
            let transition = self.transaction.as_ref().and_then(|t| {
                (t.phase < 2 && t.participants.is_subset(&t.acknowledged) && (t.phase == 0 || self.now >= t.last_ack + self.drain_bound()))
                    .then_some((t.epoch, t.phase + 1))
            });
            if let Some((epoch, phase)) = transition {
                if phase == 1 {
                    self.activate(epoch);
                } else {
                    self.transaction.as_mut().unwrap().phase = 2;
                    self.last_retire = epoch;
                }
                self.send(Message::Advance { epoch, phase });
            }
        }
        self.finish();
        self.output()
    }
}

switch_program!(FairSwitch);

type Link = (u32, u32);
type Graph = BTreeMap<u32, Vec<(u32, u64)>>;
fn shortest_path(graph: &Graph, source: u32, destination: u32) -> Option<Vec<u32>> {
    path_menu(graph, source, destination, 0).into_iter().next()
}

/// Enumerate simple paths with distance pruning and a bounded search.
fn path_menu(graph: &Graph, source: u32, destination: u32, slack: usize) -> Vec<Vec<u32>> {
    let mut distance = BTreeMap::from([(destination, 0usize)]);
    let mut todo = VecDeque::from([destination]);
    while let Some(node) = todo.pop_front() {
        let d = distance[&node];
        for (&other, edges) in graph {
            if edges.iter().any(|e| e.0 == node) && !distance.contains_key(&other) {
                distance.insert(other, d + 1);
                todo.push_back(other);
            }
        }
    }
    let Some(&shortest) = distance.get(&source) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut stack = vec![vec![source]];
    let mut work = 0;
    while let Some(path) = stack.pop() {
        work += 1;
        if work > 10_000 {
            break;
        }
        let node = *path.last().unwrap();
        if node == destination {
            result.push(path);
            if slack == 0 {
                return result;
            }
            continue;
        }
        for &(next, _) in graph.get(&node).into_iter().flatten().rev() {
            if !path.contains(&next) && distance.get(&next).is_some_and(|d| path.len() + d <= shortest + slack) {
                let mut extended = path.clone();
                extended.push(next);
                stack.push(extended);
            }
        }
    }
    result.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
    result
}
fn links(nodes: &[u32]) -> Vec<Link> {
    let mut result: Vec<_> = nodes.windows(2).map(|w| (w[0], w[1])).collect();
    if let (Some(&first), Some(&last)) = (nodes.first(), nodes.last()) {
        result.extend([(u32::MAX, first), (last, u32::MAX)]);
    }
    result
}

/// Progressive filling freezes only customers using the saturated resource.
fn shares(demands: &[f64], chosen: &[Vec<u32>], capacity: &BTreeMap<Link, f64>) -> Vec<f64> {
    let resources: Vec<_> = chosen.iter().map(|p| links(p)).collect();
    let mut allocation = vec![0.0; chosen.len()];
    let mut active: BTreeSet<_> = (0..chosen.len()).filter(|&i| !chosen[i].is_empty()).collect();
    let mut remaining = capacity.clone();
    while !active.is_empty() {
        let mut step = active.iter().map(|&i| demands[i] - allocation[i]).fold(f64::INFINITY, f64::min);
        let counts: BTreeMap<_, _> =
            remaining.keys().map(|&e| (e, active.iter().filter(|&&i| resources[i].contains(&e)).count())).collect();
        for (&edge, &count) in counts.iter().filter(|(_, n)| **n > 0) {
            step = step.min(remaining[&edge].max(0.0) / count as f64);
        }
        if !step.is_finite() {
            break;
        }
        for &i in &active {
            allocation[i] += step;
        }
        let tight: Vec<_> =
            counts.iter().filter(|(e, n)| **n > 0 && remaining[e].max(0.0) / **n as f64 <= step + 1e-6).map(|(&e, _)| e).collect();
        for (edge, count) in counts {
            *remaining.get_mut(&edge).unwrap() -= step * count as f64;
        }
        active.retain(|&i| allocation[i] + 1e-6 < demands[i] && !resources[i].iter().any(|e| tight.contains(e)));
    }
    allocation
}
struct GreedyWork {
    order: Vec<usize>,
    cursor: usize,
    chosen: Vec<Vec<u32>>,
    used: BTreeMap<Link, usize>,
}

impl GreedyWork {
    fn new<M: AsRef<[Vec<u32>]>>(demands: &[f64], menus: &[M]) -> Self {
        let mut order: Vec<_> = (0..menus.len()).collect();
        order.sort_by(|&a, &b| {
            menus[a].as_ref().len().cmp(&menus[b].as_ref().len()).then(demands[b].total_cmp(&demands[a])).then(a.cmp(&b))
        });
        Self { order, cursor: 0, chosen: vec![Vec::new(); menus.len()], used: BTreeMap::new() }
    }
    fn advance<M: AsRef<[Vec<u32>]>>(&mut self, menus: &[M], capacity: &BTreeMap<Link, f64>, count: usize) {
        let end = (self.cursor + count).min(self.order.len());
        for &i in &self.order[self.cursor..end] {
            let winner = menus[i]
                .as_ref()
                .iter()
                .map(|path| {
                    let room = links(path)
                        .iter()
                        .map(|l| capacity.get(l).copied().unwrap_or(0.0) / (self.used.get(l).copied().unwrap_or(0) + 1) as f64)
                        .fold(f64::INFINITY, f64::min);
                    (room, path)
                })
                .fold((f64::NEG_INFINITY, None), |best, (room, path)| if room > best.0 + 1e-6 { (room, Some(path)) } else { best })
                .1;
            if let Some(path) = winner {
                self.chosen[i] = path.clone();
            }
            for link in links(&self.chosen[i]) {
                *self.used.entry(link).or_default() += 1;
            }
        }
        self.cursor = end;
    }
}

impl FairSwitch {
    fn graph(&self) -> Graph {
        let mut graph = BTreeMap::new();
        for (&id, state) in &self.states {
            let reciprocal = |e: &&Edge| self.states.get(&e.to).is_some_and(|s| s.edges.iter().any(|back| back.to == id));
            graph.insert(id, state.edges.iter().filter(reciprocal).map(|e| (e.to, e.capacity)).collect());
        }
        graph
    }
    fn congestion(&self, nodes: &[u32], customer: usize) -> f64 {
        let mut pressure = 0.0;
        for w in nodes.windows(2) {
            let Some(state) = self.states.get(&w[0]) else { continue };
            let Some(edge) = state.edges.iter().find(|e| e.to == w[1]) else { continue };
            let Some(counts) = state.drops.get(&edge.port) else { continue };
            let other: u32 = counts.iter().enumerate().filter(|(q, _)| *q != 0 && *q != customer + 1).map(|(_, &n)| n).sum();
            pressure += other as f64 * 1_000_000.0 / edge.capacity.max(1) as f64;
        }
        pressure
    }
    fn rebalance(
        &self,
        i: usize,
        specs: &[Route],
        chosen: &[Vec<u32>],
        menus: &[&[Vec<u32>]],
        capacity: &BTreeMap<Link, f64>,
    ) -> Option<Vec<u32>> {
        let customer = specs[i].customer?;
        let congested = self.states.values().any(|s| s.congested.contains(&customer));
        if !congested || self.cooldown.get(&customer).is_some_and(|at| self.now < at + 2_000_000_000) {
            return None;
        }
        let demands: Vec<_> = specs.iter().map(|r| self.demand(r)).collect();
        let reference = shares(&demands, &self.greedy_work.as_ref()?.chosen, capacity);
        let allocation = shares(&demands, chosen, capacity);
        let ratio = |a: &[f64]| a.iter().zip(&reference).filter(|(_, r)| **r > 0.0).map(|(a, r)| a / r).fold(f64::INFINITY, f64::min);
        let old_ratio = ratio(&allocation);
        let old_sum: f64 = allocation.iter().sum();
        let pressure = self.congestion(&chosen[i], customer);
        let mut alternatives: Vec<_> = menus[i].iter().filter(|p| **p != chosen[i]).map(|p| (self.congestion(p, customer), p)).collect();
        alternatives.sort_by(|(a, p), (b, q)| a.total_cmp(b).then(p.len().cmp(&q.len())).then(p.cmp(q)));
        let (mut best, mut change) = (0.0, None);
        for (new_pressure, candidate) in alternatives.into_iter().take(2) {
            let mut trial = chosen.to_vec();
            trial[i] = candidate.clone();
            let proposed = shares(&demands, &trial, capacity);
            // Donors retain an 80% reference margin; minimum fairness cannot fall.
            if proposed.iter().zip(&allocation).zip(&reference).any(|((new, old), r)| new + 1e-6 < old.min(0.8 * r))
                || ratio(&proposed) + 1e-6 < old_ratio
            {
                continue;
            }
            let throughput = (proposed.iter().sum::<f64>() / old_sum.max(1.0) - 1.0).max(ratio(&proposed) / old_ratio.max(1e-9) - 1.0);
            let relief = if pressure > 0.0 { (pressure - new_pressure) / pressure } else { 0.0 };
            let gain = if throughput >= 0.10 {
                1.0 + throughput
            } else if relief >= 0.25 {
                relief
            } else {
                0.0
            };
            if gain > best {
                best = gain;
                change = Some(candidate.clone());
            }
        }
        change
    }
    fn demand(&self, route: &Route) -> f64 {
        route.customer.and_then(|i| self.customers[i].ask_bps).map_or(f64::INFINITY, |n| n as f64)
    }
    fn old_route(&self, route: &Route) -> Option<&Route> {
        self.current.iter().find(|r| r.identity() == route.identity())
    }

    fn plan(&mut self) {
        if self.id != self.leader() || self.transaction.is_some() {
            return;
        }
        // The advertised graph must be closed before choosing a coordinator's plan.
        if self.states.is_empty()
            || self.states.values().any(|s| s.host.is_none() || s.edges.iter().any(|e| !self.states.contains_key(&e.to)))
        {
            return;
        }
        let graph = self.graph();
        let hosts: BTreeMap<_, _> = self.states.iter().filter_map(|(&id, s)| Some((s.host?, id))).collect();
        let destinations: BTreeMap<_, _> = self.states.values().flat_map(|s| s.destinations.iter().map(|(&s, &d)| (s, d))).collect();
        let a1 = self.customers.is_empty();
        let eager = self.customers.len() <= 6 && self.states.len() <= 10;
        let mut specs = Vec::new();
        if a1 {
            for &source in hosts.values() {
                for (&destination, &target) in &hosts {
                    if let Some(nodes) = shortest_path(&graph, source, target) {
                        specs.push(Route { source, key: destination, destination, nodes, ..Route::default() });
                    }
                }
            }
        } else {
            for i in 0..self.customers.len() {
                let Some((&key, &source)) = hosts.iter().find(|(ip, _)| self.customer(**ip) == Some(i)) else {
                    continue;
                };
                let Some(&destination) = destinations.get(&key).filter(|d| hosts.contains_key(d)) else {
                    continue;
                };
                specs.push(Route { source, key, destination, customer: Some(i), ..Route::default() });
            }
            if self.cached_graph != graph {
                self.path_cache.clear();
                self.cached_graph = graph.clone();
                self.greedy_work = None;
            }
            let missing: BTreeSet<_> =
                specs.iter().map(|r| (r.source, hosts[&r.destination])).filter(|p| !self.path_cache.contains_key(p)).collect();
            if !missing.is_empty() {
                let limit = if eager { 6 } else { 2 };
                let remaining = missing.len() > limit;
                for (source, target) in missing.into_iter().take(limit) {
                    self.path_cache.insert((source, target), path_menu(&graph, source, target, 2));
                }
                if remaining || !eager {
                    self.needs_plan = true;
                    return;
                }
            }
            specs.retain(|r| !self.path_cache[&(r.source, hosts[&r.destination])].is_empty());
        }
        if specs.is_empty() {
            return;
        }
        let mut capacity = BTreeMap::new();
        for (&id, state) in &self.states {
            capacity.insert((u32::MAX, id), state.app_capacity as f64);
            capacity.insert((id, u32::MAX), state.app_capacity as f64);
            for &(to, c) in &graph[&id] {
                capacity.insert((id, to), c as f64);
            }
        }
        let menus: Vec<&[Vec<u32>]> = specs
            .iter()
            .map(|r| if a1 { std::slice::from_ref(&r.nodes) } else { self.path_cache[&(r.source, hosts[&r.destination])].as_slice() })
            .collect();
        let demands: Vec<_> = specs.iter().map(|r| self.demand(r)).collect();
        let model: Vec<_> = specs.iter().map(|r| (r.source, r.destination)).collect();
        if !a1 {
            if self.baseline_model != model {
                self.baseline_model = model;
                self.greedy_work = None;
            }
            let work = self.greedy_work.get_or_insert_with(|| GreedyWork::new(&demands, &menus));
            if work.cursor < specs.len() {
                work.advance(&menus, &capacity, if eager { specs.len() } else { 3 });
                if !eager {
                    self.needs_plan = true;
                    return;
                }
            }
        }
        self.needs_plan = false;
        let mut chosen: Vec<_> =
            if a1 { specs.iter().map(|r| r.nodes.clone()).collect() } else { self.greedy_work.as_ref().unwrap().chosen.clone() };
        let additions = specs.iter().any(|r| self.old_route(r).is_none());
        let baseline: Vec<_> = specs.iter().zip(&chosen).map(|(r, nodes)| Route { nodes: nodes.clone(), ..r.clone() }).collect();
        if !a1 && !(additions && self.can_transition(&baseline)) {
            for (i, r) in specs.iter().enumerate() {
                if let Some(old) = self.old_route(r).filter(|r| menus[i].contains(&r.nodes)) {
                    chosen[i] = old.nodes.clone();
                }
            }
        }
        if !a1 && !additions && !self.current.is_empty() {
            let i = self.optimization_cursor % specs.len();
            self.optimization_cursor += 1;
            if let Some(nodes) = self.rebalance(i, &specs, &chosen, &menus, &capacity) {
                chosen[i] = nodes;
                self.cooldown.insert(specs[i].customer.unwrap(), self.now);
            }
        }
        let mut desired = Vec::new();
        for (r, nodes) in specs.iter().zip(chosen) {
            desired.push(Route { nodes, ..r.clone() });
            if r.customer.is_some() {
                let source = hosts[&r.destination];
                if let Some(nodes) = shortest_path(&graph, source, r.source) {
                    desired.push(Route { source, key: r.key, destination: r.key, ack: true, nodes, ..Route::default() });
                }
            }
        }
        for r in &mut desired {
            r.id = match self.old_route(r).filter(|old| old.nodes == r.nodes) {
                Some(old) => old.id,
                None => {
                    let id = self.next_path;
                    self.next_path += 1;
                    id
                }
            };
        }
        if desired == self.current || !self.can_transition(&desired) {
            return;
        }
        let epoch = self.epoch + 1;
        self.prepare(epoch, desired.clone());
        if self.transaction.is_some() {
            self.send(Message::Prepare { epoch, routes: desired });
        }
    }
}
#[cfg(test)]
mod tests;
