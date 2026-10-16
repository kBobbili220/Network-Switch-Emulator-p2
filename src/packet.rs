use crate::types::{AppId, AsId, FlowId, LinkId, MetaKey, PacketId, SwitchId, TraceId};
use std::collections::HashMap;
use std::time::Duration;

/// Top-level discriminator for what a packet *is*. The data plane sees
/// the kind via `PacketField::Kind`; switch programs can punt or handle
/// kinds explicitly. The simulator uses the kind to decide whether to
/// validate/deliver SimpleBGP envelopes and to recognize trace probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketKind {
    Data,
    SimpleBgp,
    TraceProbe,
    TraceReply,
    /// A customer's acknowledgement of one data packet, carrying the
    /// acknowledged `seq`. Small, and never counted as workload.
    Ack,
}

impl PacketKind {
    pub fn as_u8(self) -> u8 {
        match self {
            PacketKind::Data => 0,
            PacketKind::SimpleBgp => 1,
            PacketKind::TraceProbe => 2,
            PacketKind::TraceReply => 3,
            PacketKind::Ack => 4,
        }
    }

    pub fn from_u8(v: u8) -> PacketKind {
        match v {
            1 => PacketKind::SimpleBgp,
            2 => PacketKind::TraceProbe,
            3 => PacketKind::TraceReply,
            4 => PacketKind::Ack,
            _ => PacketKind::Data,
        }
    }
}

impl Default for PacketKind {
    fn default() -> Self {
        PacketKind::Data
    }
}

/// One hop in a packet's recorded path. Only populated when the simulator
/// has BGP / trace bookkeeping enabled. Invisible to TinyVM.
#[derive(Debug, Clone)]
pub struct TrailHop {
    pub switch: SwitchId,
    pub as_id: Option<AsId>,
    pub link_in: Option<LinkId>,
    pub arrived_at: Duration,
}

/// Hidden per-packet trail recorded by the simulator. Switch programs
/// cannot read this; it's used only by the BGP conformance monitor and
/// the trace facility.
#[derive(Debug, Clone, Default)]
pub struct PacketTrail {
    pub hops: Vec<TrailHop>,
    /// True when this packet is a `TraceProbe` and should generate a
    /// `TraceReply` on delivery / drop.
    pub trace_id: Option<TraceId>,
    pub trace_requester_as: Option<AsId>,
    /// Switch that executed `Instr::MarkTrace` on this packet. The
    /// simulator backfills `trace_mark_port` with the egress port the
    /// pipeline picked at that switch, then ships the trace reply back
    /// along that port when the packet terminates.
    pub trace_mark_switch: Option<SwitchId>,
    pub trace_mark_port: Option<crate::types::PortId>,
}

pub type SimTime = Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IpAddr(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PuntReason {
    NoRoute,
    TtlExpired,
    Custom(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketField {
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
    /// Top label of the MPLS stack, or 0 if the stack is empty.
    LabelTop,
    /// Depth of the MPLS stack.
    LabelDepth,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub id: PacketId,
    pub created_at: SimTime,
    pub size_bytes: u64,

    pub ip_src: IpAddr,
    pub ip_dst: IpAddr,
    pub ip_proto: u8,
    pub ip_ttl: u8,
    pub ip_dscp: u8,

    pub src_port: Option<u16>,
    pub dst_port: Option<u16>,

    pub app_id: AppId,
    pub flow_id: FlowId,

    /// Sequence number within `flow_id`, for customers that run a closed
    /// loop: a retransmission carries the same `(flow_id, seq)` under a
    /// fresh `id`, and an `Ack` names the `seq` it acknowledges. `None`
    /// for open-loop traffic and control packets.
    pub seq: Option<u64>,

    pub custom: [u64; 4],

    /// What this packet is. Visible to TinyVM via `PacketField::Kind`.
    pub kind: PacketKind,

    /// MPLS-style label stack. `labels.last()` is the top of stack
    /// (the label that gets popped first). The simulator imposes no
    /// semantics; it's a stack of u32s the data plane may push, pop, or
    /// swap.
    pub labels: Vec<u32>,

    /// Opaque application/control payload. The TinyVM doesn't read this; it
    /// rides through the simulator unchanged. A switch program may put
    /// whatever it likes in here. For SimpleBGP packets this is the
    /// postcard-encoded `BgpEnvelopeWire`; for TraceProbes the simulator
    /// may attach a trace id here too.
    pub payload: Vec<u8>,

    /// Simulator-private path trail. Not exposed to switch programs.
    pub trail: PacketTrail,

    /// Per-packet metadata. Persists across stages for the lifetime of
    /// the packet's traversal (and across recirculations).
    pub metadata: HashMap<MetaKey, u64>,

    /// Number of times this packet has been recirculated through the pipeline.
    pub recirculation_count: u8,
}

impl Packet {
    pub fn new(id: PacketId, created_at: SimTime, size_bytes: u64) -> Self {
        Self {
            id,
            created_at,
            size_bytes,
            ip_src: IpAddr::default(),
            ip_dst: IpAddr::default(),
            ip_proto: 0,
            ip_ttl: 64,
            ip_dscp: 0,
            src_port: None,
            dst_port: None,
            app_id: AppId::default(),
            flow_id: FlowId::default(),
            seq: None,
            custom: [0; 4],
            kind: PacketKind::Data,
            labels: Vec::new(),
            payload: Vec::new(),
            trail: PacketTrail::default(),
            metadata: HashMap::new(),
            recirculation_count: 0,
        }
    }

    pub fn read_field(&self, field: PacketField) -> u64 {
        match field {
            PacketField::IpSrc => self.ip_src.0 as u64,
            PacketField::IpDst => self.ip_dst.0 as u64,
            PacketField::IpProto => self.ip_proto as u64,
            PacketField::IpTtl => self.ip_ttl as u64,
            PacketField::IpDscp => self.ip_dscp as u64,
            PacketField::SrcPort => self.src_port.unwrap_or(0) as u64,
            PacketField::DstPort => self.dst_port.unwrap_or(0) as u64,
            PacketField::AppId => self.app_id.raw() as u64,
            PacketField::FlowId => self.flow_id.raw(),
            PacketField::Custom(i) => *self.custom.get(i as usize).unwrap_or(&0),
            PacketField::Size => self.size_bytes,
            PacketField::Kind => self.kind.as_u8() as u64,
            PacketField::LabelTop => self.labels.last().copied().unwrap_or(0) as u64,
            PacketField::LabelDepth => self.labels.len() as u64,
        }
    }

    pub fn write_field(&mut self, field: PacketField, value: u64) {
        match field {
            PacketField::IpSrc => self.ip_src = IpAddr(value as u32),
            PacketField::IpDst => self.ip_dst = IpAddr(value as u32),
            PacketField::IpProto => self.ip_proto = value as u8,
            PacketField::IpTtl => self.ip_ttl = value as u8,
            PacketField::IpDscp => self.ip_dscp = value as u8,
            PacketField::SrcPort => self.src_port = Some(value as u16),
            PacketField::DstPort => self.dst_port = Some(value as u16),
            PacketField::AppId => self.app_id = AppId::new(value as u32),
            PacketField::FlowId => self.flow_id = FlowId::new(value),
            PacketField::Custom(i) => {
                if let Some(slot) = self.custom.get_mut(i as usize) {
                    *slot = value;
                }
            }
            PacketField::Size => self.size_bytes = value,
            PacketField::Kind => self.kind = PacketKind::from_u8(value as u8),
            PacketField::LabelTop | PacketField::LabelDepth => {
                // Read-only fields; ignore writes.
            }
        }
    }
}
