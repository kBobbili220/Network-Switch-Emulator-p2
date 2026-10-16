//! # load_report — what Assignment 2 gives you to see with
//!
//! Traffic engineering needs one input: how loaded is each link? In this
//! assignment you do not have to design that measurement. This program
//! shows the whole of it, and you may copy any part into your own switch.
//!
//! A switch is told three things:
//!
//! - **the customer contract**, once, in `init_with` (`input.customers`):
//!   every customer's source prefix, how its sender slows down
//!   ([`Cca::Aimd`] or [`Cca::RateMatch`]) and the rate it asks for
//!   (`None` means no limit). Every switch gets the same list;
//! - **the capacity of each of its links**, once, in `init_with`;
//! - **how many packets each port dropped** since the last timer, in
//!   every `on_timer` (`ev.port_drops`), split by queue.
//!
//! This program keeps the contract (`customer()` looks a source address
//! up in it) and does nothing more with it. Which queue a customer gets,
//! and where that queue drops, is yours to decide.
//!
//! From these it gives each of its links a **load level**: 0 (no drops),
//! 1, 2 or 3 (many drops), and counts how many customer queues dropped
//! (how many customers want the link). It then tells every other switch,
//! in a small report that is flooded through the network. After a few
//! hundred milliseconds every switch holds the same picture:
//!
//! ```text
//! (from switch, to switch) -> capacity, load level, customers
//! ```
//!
//! ## What this program does not do
//!
//! It forwards nothing and it moves nothing. It installs no routes and
//! no queues. What to do with the picture -- which customer goes on
//! which path, and when to move it -- is the assignment.

use std::collections::HashMap;
use switch_program_sdk::*;

const T_PROTO: u32 = 1;
const T_ROUTE: u32 = 2;

/// Greeting: "I am switch N", to the neighbour only.
const PROTO_GREETING: u8 = 89;
/// Load report: flooded to every switch.
const PROTO_REPORT: u8 = 90;
const CTRL_IP: u32 = 0xe000_0005;

const TICK_NS: u64 = 50_000_000; // 50 ms
/// How often the drop counts are turned into a load level.
const EVAL_NS: u64 = 500_000_000; // 500 ms
/// A link stays at its worst recent level for this many evaluations, so a
/// link that is busy on and off does not look idle between bursts.
const MEMORY: usize = 10; // 5 s

/// Drops in one evaluation period -> load level.
fn level_of(drops: u64) -> u8 {
    match drops {
        0 => 0,
        1..=4 => 1,
        5..=24 => 2,
        _ => 3,
    }
}

/// What one switch says about one of its links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLoad {
    pub neighbour: u32,
    pub capacity_kbps: u32,
    pub level: u8,
    pub customers: u8,
}

pub struct LoadReport {
    switch_id: u32,
    local_ports: Vec<u16>,
    /// port -> capacity of its link, bits per second. Told at init.
    port_capacity: HashMap<u16, u64>,
    /// The customer contract, as told at init: one row per customer.
    customers: Vec<Customer>,
    /// port -> the switch on the other end, learned from greetings.
    neighbour: HashMap<u16, u32>,

    /// Drops since the last evaluation: port -> (total, per queue).
    acc: HashMap<u16, (u64, Vec<u64>)>,
    /// port -> (drops, customer queues that dropped) in each of the last
    /// MEMORY evaluations.
    history: HashMap<u16, [(u64, u8); MEMORY]>,
    cursor: usize,
    last_eval_ns: u64,

    /// Our own report's sequence number, and the newest one seen from
    /// every other switch (so a flooded report is forwarded only once).
    my_seq: u64,
    seen_seq: HashMap<u32, u64>,

    /// The shared picture: (from, to) -> that link's load.
    pub picture: HashMap<(u32, u32), LinkLoad>,
}

impl LoadReport {
    /// Step 1: count. Every timer says what each port dropped.
    /// Step 2: every EVAL_NS, file the counts. Returns true when it did.
    fn measure(&mut self, ev: &TimerEvent) -> bool {
        for d in &ev.port_drops {
            let a = self.acc.entry(d.port).or_default();
            a.0 += d.dropped as u64;
            if a.1.len() < d.queues.len() {
                a.1.resize(d.queues.len(), 0);
            }
            for (k, n) in d.queues.iter().enumerate() {
                a.1[k] += *n as u64;
            }
        }
        if ev.now_ns.saturating_sub(self.last_eval_ns) < EVAL_NS {
            return false;
        }
        self.last_eval_ns = ev.now_ns;
        self.cursor = (self.cursor + 1) % MEMORY;
        let cursor = self.cursor;
        for slots in self.history.values_mut() {
            slots[cursor] = (0, 0);
        }
        for (port, (drops, queues)) in std::mem::take(&mut self.acc) {
            // If you keep queue 0 for your own control packets, skip it
            // here: its drops are not a customer wanting the link.
            let customers = queues.iter().filter(|q| **q > 0).count().min(255) as u8;
            self.history.entry(port).or_insert([(0, 0); MEMORY])[cursor] = (drops, customers);
        }
        true
    }

    /// The load of each of our links to another switch: the worst
    /// evaluation we remember.
    fn my_links(&self) -> Vec<LinkLoad> {
        let mut v: Vec<LinkLoad> = self
            .neighbour
            .iter()
            .map(|(port, n)| {
                let slots = self.history.get(port).copied().unwrap_or([(0, 0); MEMORY]);
                LinkLoad {
                    neighbour: *n,
                    capacity_kbps: (self.port_capacity.get(port).copied().unwrap_or(0) / 1000) as u32,
                    level: level_of(slots.iter().map(|s| s.0).max().unwrap_or(0)),
                    customers: slots.iter().map(|s| s.1).max().unwrap_or(0),
                }
            })
            .collect();
        v.sort_by_key(|l| l.neighbour);
        v
    }

    /// Step 3: the report. `[origin u32 | seq u64 | n u32 | (neighbour u32,
    /// capacity_kbps u32, level u8, customers u8) * n]`, little-endian.
    fn encode(origin: u32, seq: u64, links: &[LinkLoad]) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 10 * links.len());
        out.extend_from_slice(&origin.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(links.len() as u32).to_le_bytes());
        for l in links {
            out.extend_from_slice(&l.neighbour.to_le_bytes());
            out.extend_from_slice(&l.capacity_kbps.to_le_bytes());
            out.push(l.level);
            out.push(l.customers);
        }
        out
    }

    fn decode(p: &[u8]) -> Option<(u32, u64, Vec<LinkLoad>)> {
        let u32_at = |i: usize| Some(u32::from_le_bytes(p.get(i..i + 4)?.try_into().ok()?));
        let origin = u32_at(0)?;
        let seq = u64::from_le_bytes(p.get(4..12)?.try_into().ok()?);
        let n = u32_at(12)? as usize;
        let mut links = Vec::new();
        for k in 0..n {
            let at = 16 + 10 * k;
            links.push(LinkLoad {
                neighbour: u32_at(at)?,
                capacity_kbps: u32_at(at + 4)?,
                level: *p.get(at + 8)?,
                customers: *p.get(at + 9)?,
            });
        }
        (p.len() == 16 + 10 * n).then_some((origin, seq, links))
    }

    /// Put one switch's report into the picture, replacing its old one.
    fn remember(&mut self, origin: u32, links: &[LinkLoad]) {
        self.picture.retain(|(from, _), _| *from != origin);
        for l in links {
            self.picture.insert((origin, l.neighbour), *l);
        }
    }

    /// Send a report out of every port except the one it came in on.
    fn flood(&self, payload: &[u8], skip: Option<u16>, out: &mut Vec<Action>) {
        for port in &self.local_ports {
            if Some(*port) != skip {
                out.push(actions::inject_packet(
                    *port,
                    0xa9fe_0000 | (self.switch_id & 0xffff),
                    CTRL_IP,
                    PROTO_REPORT,
                    1, // one hop: the neighbour's own flood carries it on
                    payload.to_vec(),
                ));
            }
        }
    }
}

impl LoadReport {
    /// The contract row of the customer a source address belongs to, if
    /// it is a customer's address.
    pub fn customer(&self, ip_src: u32) -> Option<&Customer> {
        self.customers.iter().find(|c| {
            let mask = if c.prefix_len == 0 { 0 } else { u32::MAX << (32 - c.prefix_len) };
            ip_src & mask == c.prefix_addr & mask
        })
    }
}

impl SwitchProgram for LoadReport {
    /// `init_with` is `init` plus what the world tells a switch: the
    /// capacity of each port's link (in the same order as the ports) and
    /// the customer contract.
    fn init_with(input: InitInput) -> (Self, ProgramSetup) {
        let port_capacity = input
            .local_ports
            .iter()
            .copied()
            .zip(input.port_capacity_bps.iter().copied())
            .collect();
        let (mut me, setup) = Self::init(input.switch_id, input.local_ports);
        me.port_capacity = port_capacity;
        me.customers = input.customers;
        (me, setup)
    }

    fn init(switch_id: u32, local_ports: Vec<u16>) -> (Self, ProgramSetup) {
        let mut setup = ProgramSetup::new();
        setup.declare_table(T_PROTO, MatchKind::Exact, 8);
        setup.declare_table(T_ROUTE, MatchKind::Lpm, 256);
        setup.set_program(
            text::parse_tiny_program(
                "
                stage alu=4 mem=1
                    load    r0, ip_proto
                    table   t1, r0 -> m0
                stage alu=4 mem=1
                    load    r1, ip_dst
                    table   t2, r1 -> m1
                ",
            )
            .unwrap(),
        );
        // Catch both kinds of control packet and hand them to `on_punt`.
        for (id, proto) in [(1, PROTO_GREETING), (2, PROTO_REPORT)] {
            setup.add_entry(
                T_PROTO,
                TableEntry {
                    id: wire::EntryIdW(id),
                    key: proto as u64,
                    prefix_len: 8,
                    priority: 0,
                    action: TableAction::Punt {
                        reason: PuntReason::Custom(proto as u32),
                    },
                },
            );
        }
        let me = Self {
            switch_id,
            local_ports,
            port_capacity: HashMap::new(),
            customers: Vec::new(),
            neighbour: HashMap::new(),
            acc: HashMap::new(),
            history: HashMap::new(),
            cursor: 0,
            last_eval_ns: 0,
            my_seq: 0,
            seen_seq: HashMap::new(),
            picture: HashMap::new(),
        };
        (me, setup)
    }

    fn on_punt(&mut self, ev: PuntEvent) -> Vec<Action> {
        let mut out = Vec::new();
        match ev.ip_proto {
            PROTO_GREETING if ev.payload.len() == 4 => {
                let n = u32::from_le_bytes(ev.payload[..4].try_into().unwrap());
                self.neighbour.insert(ev.ingress_port, n);
            }
            PROTO_REPORT => {
                if let Some((origin, seq, links)) = Self::decode(&ev.payload) {
                    let fresh = origin != self.switch_id && self.seen_seq.get(&origin).map_or(true, |s| seq > *s);
                    if fresh {
                        self.seen_seq.insert(origin, seq);
                        self.remember(origin, &links);
                        self.flood(&ev.payload, Some(ev.ingress_port), &mut out);
                    }
                }
            }
            // A customer packet with no route. Your switch forwards it.
            _ => {}
        }
        out
    }

    fn on_timer(&mut self, ev: TimerEvent) -> Vec<Action> {
        let mut out = Vec::new();

        let hello = self.switch_id.to_le_bytes().to_vec();
        for port in &self.local_ports {
            out.push(actions::inject_packet(
                *port,
                0xa9fe_0000 | (self.switch_id & 0xffff),
                CTRL_IP,
                PROTO_GREETING,
                1,
                hello.clone(),
            ));
        }

        if self.measure(&ev) {
            let links = self.my_links();
            self.my_seq += 1;
            self.remember(self.switch_id, &links);
            let payload = Self::encode(self.switch_id, self.my_seq, &links);
            self.flood(&payload, None, &mut out);

            // `self.picture` now holds every link's capacity, load level
            // and customer count, as of the last half second. Your
            // traffic engineering starts here.
        }

        out.push(actions::schedule_timer(TICK_NS));
        out
    }
}

switch_program!(LoadReport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_survives_the_wire() {
        let links = vec![
            LinkLoad { neighbour: 7, capacity_kbps: 6000, level: 2, customers: 3 },
            LinkLoad { neighbour: 9, capacity_kbps: 10000, level: 0, customers: 0 },
        ];
        let bytes = LoadReport::encode(4, 12, &links);
        assert_eq!(LoadReport::decode(&bytes), Some((4, 12, links)));
        assert_eq!(LoadReport::decode(&bytes[..bytes.len() - 1]), None);
    }

    #[test]
    fn the_contract_is_kept_and_found_by_source_address() {
        let (me, _) = LoadReport::init_with(InitInput {
            switch_id: 3,
            local_ports: vec![0, 1],
            port_capacity_bps: vec![10_000_000, 2_000_000],
            customers: vec![
                Customer { prefix_addr: 0x0a00_0500, prefix_len: 24, cca: Cca::Aimd, ask_bps: Some(4_000_000) },
                Customer { prefix_addr: 0x0a00_0900, prefix_len: 24, cca: Cca::RateMatch, ask_bps: None },
            ],
        });
        assert_eq!(me.port_capacity[&1], 2_000_000);
        let c = me.customer(0x0a00_0907).expect("10.0.9.7 is a customer's address");
        assert_eq!((c.cca, c.ask_bps), (Cca::RateMatch, None));
        assert_eq!(me.customer(0x0a00_0501).unwrap().ask_bps, Some(4_000_000));
        assert!(me.customer(0x0a00_0101).is_none());
    }

    #[test]
    fn levels_rise_with_drops() {
        assert_eq!([level_of(0), level_of(1), level_of(5), level_of(25)], [0, 1, 2, 3]);
    }
}
