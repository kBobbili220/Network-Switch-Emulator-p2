//! # hello_switch — your starting point
//!
//! This program compiles, installs on every switch, and runs to completion.
//! It also delivers almost nothing, and that is on purpose. It exists to
//! show you the three things every switch program is made of, and then to
//! get out of your way.
//!
//! Run it and read the report card (the kit README has the exact command
//! and the names of the practice worlds):
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown
//! ./target/release/competitive_net_sim run-world WORLD.toml \
//!     --program target/wasm32-unknown-unknown/release/hello_switch.wasm \
//!     --score
//! ```
//!
//! You will see that nothing is delivered and that the card fails. Your
//! job starts with turning that around.
//!
//! ## The three parts
//!
//! **`init`** builds the data plane. It runs once, before any traffic. You
//! declare your tables here and write the little pipeline program that will
//! touch every packet. The pipeline cannot think. It can load a field, look
//! it up in a table, and do what the table says.
//!
//! **`on_punt`** is your brain reacting to one packet. The pipeline punts a
//! packet when its tables do not know what to do with it. This is the only
//! way your program ever sees a packet, so it is the only way it ever learns
//! anything about the network.
//!
//! **`on_timer`** is your brain reacting to the clock. Use it to do things
//! nobody asked for: greet your neighbours, notice one has gone quiet,
//! recompute your routes.
//!
//! ## What this program does not do
//!
//! It never installs a route, so no packet is ever forwarded. It sends a
//! greeting to its neighbours but ignores every greeting it receives, so it
//! never learns who they are. It has no idea what the network looks like.
//!
//! All of that is yours to write.

use switch_program_sdk::*;

/// Table ids. These are yours to choose; the numbers just have to match
/// between `declare_table`, the pipeline text, and `install_route`.
const T_PROTO: u32 = 1;
const T_ROUTE: u32 = 2;

/// The IP protocol number our greeting rides on. Anything that is not
/// ordinary customer traffic will do. Customer traffic uses proto 0.
const PROTO_GREETING: u8 = 89;

/// Where to address control packets. Nothing routes this address; the proto
/// table is what catches these packets and punts them.
const CTRL_IP: u32 = 0xe000_0005; // 224.0.0.5

/// How often the clock wakes us up.
const TICK_NS: u64 = 50_000_000; // 50 ms

pub struct HelloSwitch {
    /// Which switch this copy is running on. Useful in your control
    /// messages, so a neighbour can learn your name. Not useful for
    /// deciding where to forward: the same program runs everywhere, and it
    /// cannot know what the network looks like just from its own id.
    switch_id: u32,

    /// The ports that have a cable attached. You are told the port numbers.
    /// You are not told who is on the other end. Finding that out is the
    /// first real problem in this assignment.
    local_ports: Vec<u16>,

    /// How many packets the pipeline has handed us. Every one of these is a
    /// packet that did not get delivered.
    punts_seen: u64,
}

impl SwitchProgram for HelloSwitch {
    fn init(switch_id: u32, local_ports: Vec<u16>) -> (Self, ProgramSetup) {
        let mut setup = ProgramSetup::new();

        // Two tables.
        //
        // T_PROTO is an exact match on the IP protocol number. We use it to
        // catch our own control traffic before it reaches the route lookup.
        //
        // T_ROUTE is a longest-prefix match on the destination address.
        // This is the real forwarding table, and it starts empty. Filling
        // it is the assignment.
        setup.declare_table(T_PROTO, MatchKind::Exact, 8);
        setup.declare_table(T_ROUTE, MatchKind::Lpm, 256);

        // The pipeline. Two stages, run in order on every packet.
        //
        //   stage 0: load the protocol number, look it up in T_PROTO.
        //   stage 1: load the destination address, look it up in T_ROUTE.
        //
        // A packet that matches nothing in T_ROUTE has no egress port. The
        // simulator turns that into a punt and hands it to `on_punt`.
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

        // One entry, installed before any traffic moves: catch our greeting
        // protocol and punt it. Without this, a greeting arriving at a
        // neighbour would fall through to the route lookup and be treated
        // as ordinary traffic.
        setup.add_entry(
            T_PROTO,
            TableEntry {
                id: wire::EntryIdW(1),
                key: PROTO_GREETING as u64,
                prefix_len: 8,
                priority: 0,
                action: TableAction::Punt {
                    reason: PuntReason::Custom(PROTO_GREETING as u32),
                },
            },
        );

        // Note what is missing: not one route. That is why nothing is
        // delivered.
        let me = Self {
            switch_id,
            local_ports,
            punts_seen: 0,
        };
        (me, setup)
    }

    /// A packet the pipeline could not handle. Two kinds arrive here:
    ///
    /// - a greeting from a neighbour, caught by T_PROTO
    /// - a customer packet with no matching route, which is every customer
    ///   packet until you install some routes
    ///
    /// Both are free information. `ev.ingress_port` tells you which cable it
    /// came in on. `ev.payload` is whatever the sender put there. Right now
    /// we throw all of it away.
    fn on_punt(&mut self, _ev: PuntEvent) -> Vec<Action> {
        self.punts_seen += 1;

        // Your work starts here. Some things worth doing:
        //
        //   - if this is a greeting, remember which switch is on which port
        //   - if this is a customer packet arriving from a port you have
        //     never heard a greeting on, that port probably faces your
        //     customer, and its source address tells you their prefix
        //   - once you know the network, call `actions::install_route` so
        //     the pipeline can forward the next one without waking you up
        Vec::new()
    }

    /// The clock. The simulator sends one of these to every switch at time
    /// zero, and after that only when you ask for another one.
    fn on_timer(&mut self, _ev: TimerEvent) -> Vec<Action> {
        let mut out = Vec::new();

        // Say hello on every cable. The payload is our switch id, so a
        // neighbour that bothers to read it learns our name.
        //
        // We send these on every tick, forever. That is deliberate: it is
        // also how you notice a link has died. If a neighbour's greetings
        // stop arriving, something between you and them has broken. Nobody
        // is going to tell you.
        let payload = self.switch_id.to_le_bytes().to_vec();
        for port in &self.local_ports {
            out.push(actions::inject_packet(
                *port,
                0xa9fe_0000 | (self.switch_id & 0xffff), // a source address nobody routes
                CTRL_IP,
                PROTO_GREETING,
                1, // ttl 1: this packet is for the neighbour, not the network
                payload.clone(),
            ));
        }

        // Ask for the next tick. Forget this line and your program gets
        // exactly one wake-up for the whole run, then goes silent forever.
        out.push(actions::schedule_timer(TICK_NS));
        out
    }
}

switch_program!(HelloSwitch);
