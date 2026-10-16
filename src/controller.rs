use crate::packet::{Packet, PuntReason};
use crate::tinyvm::TableEntry;
use crate::types::{EntryId, PortId, QueueId, SimTime, SwitchId, TableId};

#[derive(Debug, Clone)]
pub struct PuntEvent {
    pub now: SimTime,
    pub switch: SwitchId,
    pub ingress_port: PortId,
    pub reason: PuntReason,
    pub packet: Packet,
}

#[derive(Debug, Clone)]
pub enum LinkEvent {
    Up { port: PortId },
    Down { port: PortId },
}

/// How an egress port serves its queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Discipline {
    #[default]
    Fifo,
    StrictPriority,
    Drr,
}

impl Discipline {
    /// Wire encoding: 0 = FIFO, 1 = strict priority, 2 = DRR. Unknown
    /// values read as FIFO.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Discipline::StrictPriority,
            2 => Discipline::Drr,
            _ => Discipline::Fifo,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Discipline::Fifo => 0,
            Discipline::StrictPriority => 1,
            Discipline::Drr => 2,
        }
    }
}

/// One queue as a program asks for it. Everything is in packets: the
/// switch multiplies by its `packet_size_bytes` when it applies the
/// request, so students never see bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSpec {
    /// Drop-tail cap. Admission fails once this many packets wait.
    pub max_packets: u32,
    /// Share under DRR. Ignored otherwise.
    pub weight: u32,
    /// Early-drop line for the drop policy; `None` = drop-tail only.
    pub drop_threshold_packets: Option<u32>,
}

impl QueueSpec {
    pub fn new(max_packets: u32) -> Self {
        Self {
            max_packets,
            weight: 1,
            drop_threshold_packets: None,
        }
    }
}

/// The queues and scheduler a program wants on one port. An empty
/// `queues` keeps the port's current queues and changes only the
/// discipline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueConfig {
    pub discipline: Discipline,
    pub queues: Vec<QueueSpec>,
}

#[derive(Debug, Clone)]
pub enum ControllerAction {
    InstallTableEntry {
        table_id: TableId,
        entry: TableEntry,
    },
    DeleteTableEntry {
        table_id: TableId,
        entry_id: EntryId,
    },
    InjectPacket {
        packet: Packet,
        port: PortId,
        /// Which of the port's queues the packet joins.
        queue: QueueId,
    },
    SetQueueConfig {
        port: PortId,
        config: QueueConfig,
    },
    /// Ask the simulator to fire `on_timer(now)` again after `delay`.
    /// Travels through the config pipe like other actions.
    ScheduleTimer {
        delay: std::time::Duration,
    },
    /// Put on record what this switch believes about the link on `port`:
    /// `up: false` declares it dead, `up: true` declares it alive again.
    /// Nothing in the data plane changes; the declaration is logged when
    /// it lands so the grader can compare belief with truth.
    DeclareLink {
        port: PortId,
        up: bool,
    },
}

impl ControllerAction {
    /// Charged payload size for the config pipe.
    /// This is the *payload* component; the switch adds its `min_control_msg_size_bytes`.
    pub fn payload_size(&self) -> u64 {
        match self {
            ControllerAction::InstallTableEntry { .. } => 32,
            ControllerAction::DeleteTableEntry { .. } => 16,
            ControllerAction::InjectPacket { packet, .. } => packet.size_bytes,
            ControllerAction::SetQueueConfig { config, .. } => {
                16 + 8 * config.queues.len() as u64
            }
            ControllerAction::ScheduleTimer { .. } => 8,
            ControllerAction::DeclareLink { .. } => 8,
        }
    }
}

/// What one egress port refused since the controller's previous timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDrops {
    pub port: PortId,
    /// Every refusal: pool full, queue full, or the drop threshold.
    pub dropped: u64,
    /// Of those, refusals by a queue's drop threshold.
    pub policy: u64,
    /// The same refusals split by the queue the packet was headed for.
    pub queues: Vec<u64>,
}

pub trait SwitchController {
    fn on_punt(&mut self, event: PuntEvent) -> Vec<ControllerAction>;
    fn on_timer(&mut self, now: SimTime) -> Vec<ControllerAction>;
    fn on_link_event(&mut self, event: LinkEvent) -> Vec<ControllerAction>;

    /// The timer, with what each port dropped since the last one. The
    /// simulator calls this; controllers that do not care about drops
    /// implement `on_timer` and get it through this default.
    fn on_timer_with_drops(&mut self, now: SimTime, _drops: Vec<PortDrops>) -> Vec<ControllerAction> {
        self.on_timer(now)
    }

    /// Why this controller stopped running, if it did, as one printable
    /// block. A controller that fails is never called again and its
    /// switch forwards on frozen state -- which is indistinguishable from
    /// a routing bug unless somebody says otherwise. Controllers that
    /// cannot fail return `None`.
    fn failure(&self) -> Option<String> {
        None
    }
}

/// A controller that always returns no actions. Useful for tests where
/// we want to exercise the data plane in isolation.
pub struct NoopController;

impl SwitchController for NoopController {
    fn on_punt(&mut self, _event: PuntEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_timer(&mut self, _now: SimTime) -> Vec<ControllerAction> {
        Vec::new()
    }
    fn on_link_event(&mut self, _event: LinkEvent) -> Vec<ControllerAction> {
        Vec::new()
    }
}
