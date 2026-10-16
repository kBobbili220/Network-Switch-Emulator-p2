//! SDK for writing switch programs in idiomatic Rust and compiling them to
//! WebAssembly for the `competitive_net_sim` simulator.
//!
//! # Quickstart
//!
//! ```ignore
//! // src/lib.rs of your switch-program crate
//! #![no_main]
//! use switch_program_sdk::*;
//!
//! struct MyProgram { switch_id: u32 }
//!
//! impl SwitchProgram for MyProgram {
//!     fn init(switch_id: u32) -> (Self, ProgramSetup) {
//!         let mut setup = ProgramSetup::new();
//!         setup.declare_table(1, MatchKind::Exact, 16);
//!         setup.set_program(text::parse_tiny_program("
//!             stage alu=4 mem=1
//!                 load    r0, ip_dst
//!                 table   t1, r0 -> m0
//!         ").unwrap());
//!         (Self { switch_id }, setup)
//!     }
//!
//!     fn on_punt(&mut self, _ev: PuntEvent) -> Vec<Action> {
//!         vec![actions::install_route(1, 42, 0x0a000002, 32, 1)]
//!     }
//! }
//!
//! switch_program!(MyProgram);
//! ```
//!
//! Build with:
//!
//! ```sh
//! cargo build --release --target wasm32-unknown-unknown
//! ```
//!
//! Then in the simulator:
//!
//! ```ignore
//! let bytes = std::fs::read("target/wasm32-unknown-unknown/release/my_program.wasm")?;
//! sim.install_program(OwnerId::new(1), &bytes, WasmLimits::default())?;
//! ```

pub use switch_program_types as wire;
pub use switch_program_types::bgp;
pub use switch_program_types::text;

// Friendly aliases for SDK users.
pub use wire::{
    ActionW as Action, CcaW as Cca, CounterDeclW, CustomerW as Customer, InitInputW as InitInput, InstrW as Instr, LinkEventW as LinkEvent,
    MatchKindW as MatchKind, PacketFieldW as PacketField, PuntEventW as PuntEvent,
    PortDropsW as PortDrops, PuntReasonW as PuntReason, QueueSpecW as QueueSpec, RegisterDeclW, StageProgramW as StageProgram,
    TableActionW as TableAction, TableEntryW as TableEntry, TimerEventW as TimerEvent,
    TinyProgramW as TinyProgram,
};

/// Program setup declared by `init` and returned to the simulator.
#[derive(Debug, Default, Clone)]
pub struct ProgramSetup {
    inner: wire::ProgramSetupW,
}

impl ProgramSetup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the data-plane TinyVM program.
    pub fn set_program(&mut self, program: TinyProgram) -> &mut Self {
        self.inner.program = program;
        self
    }

    /// Declare a match-action table.
    pub fn declare_table(&mut self, id: u32, kind: MatchKind, max_entries: u32) -> &mut Self {
        self.inner.tables.push(wire::TableDeclW {
            id: wire::TableIdW(id),
            kind,
            max_entries,
            initial_entries: Vec::new(),
        });
        self
    }

    /// Pre-populate a table with an entry. The table must already be declared.
    pub fn add_entry(&mut self, table_id: u32, entry: TableEntry) -> &mut Self {
        if let Some(t) = self.inner.tables.iter_mut().find(|t| t.id.0 == table_id) {
            t.initial_entries.push(entry);
        }
        self
    }

    /// Declare a register array of `size` u64 slots.
    pub fn declare_register_array(&mut self, id: u32, size: u32) -> &mut Self {
        self.inner.registers.push(wire::RegisterDeclW {
            id: wire::RegisterArrayIdW(id),
            size,
        });
        self
    }

    /// Declare a counter array of `size` u64 slots.
    pub fn declare_counter_array(&mut self, id: u32, size: u32) -> &mut Self {
        self.inner.counters.push(wire::CounterDeclW {
            id: wire::CounterArrayIdW(id),
            size,
        });
        self
    }

    pub fn into_wire(self) -> wire::ProgramSetupW {
        self.inner
    }
}

/// Action constructors.
pub mod actions {
    use super::*;

    pub fn install_route(
        table_id: u32,
        entry_id: u64,
        key: u64,
        prefix_len: u8,
        egress_port: u16,
    ) -> Action {
        Action::InstallTableEntry {
            table_id: wire::TableIdW(table_id),
            entry: wire::TableEntryW {
                id: wire::EntryIdW(entry_id),
                key,
                prefix_len,
                priority: 0,
                action: wire::TableActionW::SetEgress { port: wire::PortIdW(egress_port) },
            },
        }
    }

    pub fn install_entry(table_id: u32, entry: TableEntry) -> Action {
        Action::InstallTableEntry {
            table_id: wire::TableIdW(table_id),
            entry,
        }
    }

    pub fn delete_entry(table_id: u32, entry_id: u64) -> Action {
        Action::DeleteTableEntry {
            table_id: wire::TableIdW(table_id),
            entry_id: wire::EntryIdW(entry_id),
        }
    }

    /// Inject a regular Data packet on `port`.
    pub fn inject_packet(
        port: u16,
        ip_src: u32,
        ip_dst: u32,
        ip_proto: u8,
        ip_ttl: u8,
        payload: Vec<u8>,
    ) -> Action {
        let size = (20 + payload.len()) as u64; // approx IPv4 + payload
        Action::InjectPacket {
            port: wire::PortIdW(port),
            ip_src,
            ip_dst,
            ip_proto,
            ip_ttl,
            kind: 0, // PacketKind::Data
            size_bytes: size,
            payload,
        }
    }

    /// Inject a SimpleBGP packet (kind = SimpleBgp) on `port`. The
    /// payload is a postcard-encoded `BgpEnvelopeW`.
    pub fn inject_bgp_packet(
        port: u16,
        sender_speaker_ip: u32,
        receiver_speaker_ip: u32,
        ip_ttl: u8,
        envelope_bytes: Vec<u8>,
    ) -> Action {
        let size = (20 + envelope_bytes.len()) as u64;
        Action::InjectPacket {
            port: wire::PortIdW(port),
            ip_src: sender_speaker_ip,
            ip_dst: receiver_speaker_ip,
            ip_proto: 179, // conventional BGP port; advisory only
            ip_ttl,
            kind: 1, // PacketKind::SimpleBgp
            size_bytes: size,
            payload: envelope_bytes,
        }
    }

    /// Ask the simulator to fire `on_timer` again `delay_ns` after now.
    pub fn schedule_timer(delay_ns: u64) -> Action {
        Action::ScheduleTimer { delay_ns }
    }

    /// Declare that the link on `port` is dead. This is a statement for
    /// the record, compared against the truth in grading; it does not
    /// change forwarding. Say it when your control plane concludes the
    /// link is gone, and say `link_up` when it concludes otherwise.
    pub fn link_down(port: u16) -> Action {
        Action::DeclareLink {
            port: wire::PortIdW(port),
            up: false,
        }
    }

    /// Declare that the link on `port` is alive (again). See `link_down`.
    pub fn link_up(port: u16) -> Action {
        Action::DeclareLink {
            port: wire::PortIdW(port),
            up: true,
        }
    }

    /// Send an injected packet through queue `queue` of its port instead
    /// of queue 0. Wrap `inject_packet` / `inject_bgp_packet`:
    ///
    /// ```ignore
    /// actions::in_queue(0, actions::inject_bgp_packet(port, me, peer, 64, bytes))
    /// ```
    ///
    /// Non-inject actions are returned unchanged.
    pub fn in_queue(queue: u16, action: Action) -> Action {
        match action {
            Action::InjectPacket {
                port,
                ip_src,
                ip_dst,
                ip_proto,
                ip_ttl,
                kind,
                size_bytes,
                payload,
            }
            | Action::InjectPacketQueued {
                port,
                ip_src,
                ip_dst,
                ip_proto,
                ip_ttl,
                kind,
                size_bytes,
                payload,
                ..
            } => Action::InjectPacketQueued {
                port,
                queue: wire::QueueIdW(queue),
                ip_src,
                ip_dst,
                ip_proto,
                ip_ttl,
                kind,
                size_bytes,
                payload,
            },
            other => other,
        }
    }

    /// Give `port` the queues in `queues`, served by `discipline`. Queue
    /// `i` here is what the data plane's `queue i` instruction (or a
    /// table's `SetQueue`) selects. Sizes are in packets. Packets already
    /// waiting on the port are kept.
    pub fn set_queues(port: u16, discipline: Discipline, queues: Vec<QueueSpec>) -> Action {
        Action::SetQueueConfig {
            port: wire::PortIdW(port),
            discipline: discipline as u8,
            queues,
        }
    }
}

/// How an egress port serves its queues. See [`actions::set_queues`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Discipline {
    /// Arrival order across all queues; with several queues this is the
    /// same as `StrictPriority`.
    Fifo = wire::DISCIPLINE_FIFO,
    /// Queue 0 whenever it has a packet, else queue 1, and so on.
    StrictPriority = wire::DISCIPLINE_STRICT_PRIORITY,
    /// Deficit round robin: each queue gets `weight` packets of credit
    /// per round.
    Drr = wire::DISCIPLINE_DRR,
}


/// User-facing trait. Implement this for your switch program type, then call
/// the [`switch_program!`] macro at module scope to wire up exports.
pub trait SwitchProgram: Sized + 'static {
    /// Construct the program. `local_ports` lists the port ids that have a
    /// link attached on this switch — the program does not know who is on
    /// the other side of each port; it has to learn that.
    fn init(switch_id: u32, local_ports: Vec<u16>) -> (Self, ProgramSetup);

    /// Construct the program from everything the host says at start:
    /// the switch id, the ports with a link, each link's capacity in
    /// bits per second (`input.port_capacity_bps`, parallel to
    /// `input.local_ports`), and the customer contract
    /// (`input.customers`: every customer's source prefix, its [`Cca`]
    /// and the rate it asks for). The default forwards to `init`;
    /// override this instead when the capacities or the contract matter
    /// to you.
    fn init_with(input: InitInput) -> (Self, ProgramSetup) {
        Self::init(input.switch_id, input.local_ports)
    }

    fn on_punt(&mut self, _event: PuntEvent) -> Vec<Action> {
        Vec::new()
    }
    fn on_timer(&mut self, _event: TimerEvent) -> Vec<Action> {
        Vec::new()
    }
}

// Note for the curious: the wasm module also exports `on_link_event`, and
// the simulator never calls it. A dead link is not announced — it just goes
// quiet, and noticing the quiet is your job. There is deliberately no way to
// hook that export from this trait.

#[doc(hidden)]
pub mod __export {
    pub use postcard;

    /// Decode `Input` from `(in_ptr, in_len)`, run `body`, encode the result
    /// into `(out_ptr, out_cap)`. Returns the byte count written, or a
    /// negative value on error.
    pub fn run_call<Input, Output>(
        in_ptr: i32,
        in_len: i32,
        out_ptr: i32,
        out_cap: i32,
        body: impl FnOnce(Input) -> Output,
    ) -> i32
    where
        Input: serde::de::DeserializeOwned,
        Output: serde::Serialize,
    {
        let in_slice = unsafe {
            core::slice::from_raw_parts(in_ptr as *const u8, in_len as usize)
        };
        let input: Input = match postcard::from_bytes(in_slice) {
            Ok(v) => v,
            Err(_) => return -1,
        };
        let output = body(input);
        let out_slice = unsafe {
            core::slice::from_raw_parts_mut(out_ptr as *mut u8, out_cap as usize)
        };
        match postcard::to_slice(&output, out_slice) {
            Ok(written) => written.len() as i32,
            Err(_) => -2,
        }
    }
}

/// Wire your `SwitchProgram` impl up as the four wasm exports.
///
/// Generates `init`, `on_punt`, `on_timer`, `on_link_event` extern "C"
/// functions and a static slot holding your program instance.
#[macro_export]
macro_rules! switch_program {
    ($ty:ty) => {
        thread_local! {
            static __SWITCH_PROGRAM_SLOT: ::core::cell::RefCell<::core::option::Option<$ty>>
                = const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn init(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
            $crate::__export::run_call::<$crate::wire::InitInputW, $crate::wire::ProgramSetupW>(
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                |input| {
                    let (program, setup) = <$ty as $crate::SwitchProgram>::init_with(input);
                    __SWITCH_PROGRAM_SLOT.with(|cell| *cell.borrow_mut() = Some(program));
                    setup.into_wire()
                },
            )
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn on_punt(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
            $crate::__export::run_call::<$crate::wire::PuntEventW, ::std::vec::Vec<$crate::wire::ActionW>>(
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                |ev| {
                    __SWITCH_PROGRAM_SLOT.with(|cell| {
                        match cell.borrow_mut().as_mut() {
                            Some(p) => <$ty as $crate::SwitchProgram>::on_punt(p, ev),
                            None => ::std::vec::Vec::new(),
                        }
                    })
                },
            )
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn on_timer(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
            $crate::__export::run_call::<$crate::wire::TimerEventW, ::std::vec::Vec<$crate::wire::ActionW>>(
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                |ev| {
                    __SWITCH_PROGRAM_SLOT.with(|cell| {
                        match cell.borrow_mut().as_mut() {
                            Some(p) => <$ty as $crate::SwitchProgram>::on_timer(p, ev),
                            None => ::std::vec::Vec::new(),
                        }
                    })
                },
            )
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn on_link_event(
            in_ptr: i32,
            in_len: i32,
            out_ptr: i32,
            out_cap: i32,
        ) -> i32 {
            $crate::__export::run_call::<$crate::wire::LinkEventW, ::std::vec::Vec<$crate::wire::ActionW>>(
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                |ev| {
                    {
                        // The simulator does not deliver these. The export
                        // exists only because the ABI requires it.
                        let _ = ev;
                        ::std::vec::Vec::new()
                    }
                },
            )
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ToyProgram {
        switch_id: u32,
    }

    impl SwitchProgram for ToyProgram {
        fn init(switch_id: u32, _local_ports: Vec<u16>) -> (Self, ProgramSetup) {
            let mut setup = ProgramSetup::new();
            setup.declare_table(1, MatchKind::Exact, 16);
            setup.set_program(
                text::parse_tiny_program(
                    "
                stage alu=4 mem=1
                    load    r0, ip_dst
                    table   t1, r0 -> m0
                ",
                )
                .unwrap(),
            );
            (Self { switch_id }, setup)
        }

        fn on_punt(&mut self, _ev: PuntEvent) -> Vec<Action> {
            vec![actions::install_route(1, 42, 0x0a000002, 32, 1)]
        }
    }

    #[test]
    fn builder_round_trip() {
        let (_p, setup) = ToyProgram::init(7, vec![]);
        let wire = setup.into_wire();
        assert_eq!(wire.tables.len(), 1);
        assert_eq!(wire.program.stages.len(), 1);
        assert_eq!(wire.program.stages[0].instrs.len(), 2);
    }

    #[test]
    fn action_helpers() {
        let a = actions::install_route(1, 42, 0xdead, 32, 7);
        match a {
            Action::InstallTableEntry { table_id, entry } => {
                assert_eq!(table_id.0, 1);
                assert_eq!(entry.id.0, 42);
                assert_eq!(entry.key, 0xdead);
                assert!(matches!(
                    entry.action,
                    TableAction::SetEgress { port: wire::PortIdW(7) }
                ));
            }
            _ => panic!(),
        }
    }
}
