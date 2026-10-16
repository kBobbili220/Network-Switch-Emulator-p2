//! Wire types and TinyVM text parser shared between the simulator host and
//! the `switch_program_sdk`.
//!
//! All on-the-wire structs are postcard-serializable and `no_std`-friendly so
//! a switch program compiled to `wasm32-unknown-unknown` can encode and decode
//! them with the same code the simulator uses.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

pub mod text;
pub mod bgp;

// ---------- Identity ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortIdW(pub u16);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueueIdW(pub u16);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TableIdW(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegisterArrayIdW(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CounterArrayIdW(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryIdW(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetaKeyW(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RegW(pub u8);

// ---------- Packet fields ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PacketFieldW {
    IpSrc,
    IpDst,
    IpProto,
    IpTtl,
    IpDscp,
    SrcPort,
    DstPort,
    AppId,
    FlowId,
    Custom(u8),
    Size,
    Kind,
    LabelTop,
    LabelDepth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PuntReasonW {
    NoRoute,
    TtlExpired,
    Custom(u32),
}

// ---------- TinyVM ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MatchKindW {
    Exact,
    Lpm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstrW {
    LoadField { dst: RegW, field: PacketFieldW },
    StoreField { field: PacketFieldW, src: RegW },
    LoadMeta { dst: RegW, key: MetaKeyW },
    StoreMeta { key: MetaKeyW, src: RegW },
    Const { dst: RegW, value: u64 },
    Add { dst: RegW, a: RegW, b: RegW },
    Sub { dst: RegW, a: RegW, b: RegW },
    And { dst: RegW, a: RegW, b: RegW },
    Or  { dst: RegW, a: RegW, b: RegW },
    Xor { dst: RegW, a: RegW, b: RegW },
    Eq  { dst: RegW, a: RegW, b: RegW },
    Lt  { dst: RegW, a: RegW, b: RegW },
    TableLookup { table_id: TableIdW, key_reg: RegW, result_meta: MetaKeyW },
    RegisterRead { array: RegisterArrayIdW, index: RegW, dst: RegW },
    RegisterWrite { array: RegisterArrayIdW, index: RegW, src: RegW },
    CounterAdd { counter: CounterArrayIdW, index: RegW, value: RegW },
    BranchIf { cond: RegW, target: u32 },
    Drop,
    Punt { reason: PuntReasonW },
    SetEgress { port: PortIdW },
    SetQueue { queue: QueueIdW },
    Recirculate,
    Noop,
    PushLabel { label: u32 },
    PopLabel,
    SwapLabel { label: u32 },
    /// Mark the packet so that wherever it terminates, a TraceReply is
    /// magically delivered to *this* switch on the egress port the data
    /// plane chose. The reply consumes link bandwidth normally.
    MarkTrace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageProgramW {
    pub instrs: Vec<InstrW>,
    pub max_alu_ops: u32,
    pub max_memory_accesses: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TinyProgramW {
    pub stages: Vec<StageProgramW>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TableActionW {
    SetMeta { key: MetaKeyW, value: u64 },
    SetEgress { port: PortIdW },
    SetQueue { queue: QueueIdW },
    Drop,
    Punt { reason: PuntReasonW },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableEntryW {
    pub id: EntryIdW,
    pub key: u64,
    pub prefix_len: u8,
    pub priority: i32,
    pub action: TableActionW,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDeclW {
    pub id: TableIdW,
    pub kind: MatchKindW,
    pub max_entries: u32,
    pub initial_entries: Vec<TableEntryW>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterDeclW {
    pub id: RegisterArrayIdW,
    pub size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterDeclW {
    pub id: CounterArrayIdW,
    pub size: u32,
}

/// What `init` returns: the data plane's program plus state declarations.
/// The simulator takes ownership and installs them on the switch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProgramSetupW {
    pub program: TinyProgramW,
    pub tables: Vec<TableDeclW>,
    pub registers: Vec<RegisterDeclW>,
    pub counters: Vec<CounterDeclW>,
}

// ---------- Controller events / actions ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitInputW {
    pub switch_id: u32,
    /// Local port ids that have a link attached, filled in from the
    /// topology. Which port leads where is not said, and nothing here
    /// implies what a program should do with them.
    pub local_ports: Vec<u16>,
    /// The capacity of the link on each port, in bits per second, in the
    /// same order as `local_ports`. A switch knows how fast its own
    /// cables are; what is on the other end it still has to learn.
    /// Appended after the first release; older decoders read an empty
    /// list.
    #[serde(default)]
    pub port_capacity_bps: Vec<u64>,
    /// The customer contract: one row for every customer in the network,
    /// the same list at every switch. What each customer bought, not
    /// what it is doing now. Empty in a world without customers.
    /// Appended after `port_capacity_bps`; older decoders ignore it.
    #[serde(default)]
    pub customers: Vec<CustomerW>,
}

/// How a customer's sender slows down when the network is congested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CcaW {
    /// Halves its rate on a loss, climbs a step at a time without one.
    Aimd,
    /// Sends at the rate that was acknowledged lately, and a little more.
    RateMatch,
}

/// One row of the customer contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerW {
    /// The customer's source prefix: its packets carry an `ip_src`
    /// inside `prefix_addr/prefix_len`.
    pub prefix_addr: u32,
    pub prefix_len: u8,
    pub cca: CcaW,
    /// The rate it asks for while it is sending, in bits per second.
    /// `None` means no limit: it takes whatever it is given.
    pub ask_bps: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PuntEventW {
    pub now_ns: u64,
    pub switch_id: u32,
    pub ingress_port: u16,
    pub reason: PuntReasonW,
    pub packet_size: u64,
    pub ip_src: u32,
    pub ip_dst: u32,
    pub ip_proto: u8,
    pub ip_ttl: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkEventW {
    /// 0 = up, 1 = down
    pub kind: u8,
    pub port: u16,
}

/// Drops one egress port refused since the program's previous timer.
/// Only ports that dropped something are listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortDropsW {
    pub port: u16,
    /// Every refusal: pool full, queue full, or the drop threshold.
    pub dropped: u32,
    /// Of those, refusals by a queue's drop threshold -- the program's own
    /// policy, not a lack of buffer.
    pub policy: u32,
    /// The same refusals split by the queue the packet was headed for,
    /// indexed like the port's `set_queues` list.
    #[serde(default)]
    pub queues: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerEventW {
    pub now_ns: u64,
    /// Appended after the first release; a decoder that only knows
    /// `now_ns` still reads the event, because postcard ignores what
    /// follows the last field it asked for.
    #[serde(default)]
    pub port_drops: Vec<PortDropsW>,
}

/// One egress queue as a program asks for it. Everything is counted in
/// packets of the world's data size; the simulator converts to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSpecW {
    /// Drop-tail cap: admission fails once this many packets wait.
    pub max_packets: u32,
    /// Share of the wire under DRR (0 reads as 1). Ignored otherwise.
    pub weight: u32,
    /// Early-drop line for the drop policy; 0 = drop-tail only.
    pub drop_threshold_packets: u32,
}

impl QueueSpecW {
    /// A drop-tail queue holding at most `max_packets` packets, weight 1.
    pub fn new(max_packets: u32) -> Self {
        Self {
            max_packets,
            weight: 1,
            drop_threshold_packets: 0,
        }
    }

    /// Share of the wire under DRR.
    pub fn weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    /// Start dropping arrivals once this many packets wait (drop policy).
    pub fn drop_at(mut self, packets: u32) -> Self {
        self.drop_threshold_packets = packets;
        self
    }
}

/// Scheduler discipline codes for `ActionW::SetQueueConfig`.
pub const DISCIPLINE_FIFO: u8 = 0;
pub const DISCIPLINE_STRICT_PRIORITY: u8 = 1;
pub const DISCIPLINE_DRR: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionW {
    InstallTableEntry { table_id: TableIdW, entry: TableEntryW },
    DeleteTableEntry { table_id: TableIdW, entry_id: EntryIdW },
    /// Replace the queues on `port` and pick how they are served. Queue
    /// `i` of `queues` is what `SetQueue { queue: i }` in the data plane
    /// refers to. An empty `queues` keeps the current queues and only
    /// changes the discipline. Packets already waiting are kept.
    SetQueueConfig {
        port: PortIdW,
        /// One of the `DISCIPLINE_*` codes.
        discipline: u8,
        queues: Vec<QueueSpecW>,
    },
    InjectPacket {
        port: PortIdW,
        ip_src: u32,
        ip_dst: u32,
        ip_proto: u8,
        ip_ttl: u8,
        /// PacketKind discriminant: 0=Data, 1=SimpleBgp, 2=TraceProbe, 3=TraceReply.
        kind: u8,
        size_bytes: u64,
        payload: Vec<u8>,
    },
    /// Schedule a controller-timer callback `delay_ns` after now.
    ScheduleTimer { delay_ns: u64 },
    /// Declare what this switch believes about the link on `port`. Logged
    /// when it lands; changes nothing in the data plane.
    DeclareLink { port: PortIdW, up: bool },
    /// `InjectPacket` that also names the egress queue the packet joins.
    /// Appended so programs built against the older shape still decode.
    InjectPacketQueued {
        port: PortIdW,
        queue: QueueIdW,
        ip_src: u32,
        ip_dst: u32,
        ip_proto: u8,
        ip_ttl: u8,
        kind: u8,
        size_bytes: u64,
        payload: Vec<u8>,
    },
}

// ---------- Trace reply (delivered to marking switch's controller) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceHopW {
    pub switch_id: u32,
    pub arrived_at_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceReplyW {
    /// The switch that asked for the trace by running `MarkTrace`.
    pub mark_switch: u32,
    /// The port on `mark_switch` where the reply is arriving (this is
    /// also the egress port the marked packet originally took).
    pub mark_port: u16,
    /// Switches the original packet visited, in order.
    pub hops: Vec<TraceHopW>,
    /// Whether the packet was eventually delivered (true) or dropped.
    pub delivered: bool,
    /// 0 = delivered, 1 = ttl, 2 = no_route, 3 = no_egress_link,
    /// 4 = link_down, 5 = switch_failed, 6 = recirc, 7 = queue_full,
    /// 8 = dropped by the switch program.
    pub drop_reason: u8,
}

// ---------- Decode error ----------

#[derive(Debug)]
pub enum WireError {
    Decode(String),
}

impl WireError {
    pub fn decode(e: impl ToString) -> Self {
        WireError::Decode(e.to_string())
    }
}
