use crate::packet::{Packet, PacketField, PuntReason};
use crate::types::{
    CounterArrayId, EntryId, InstrIndex, MetaKey, PortId, QueueId, Reg, RegisterArrayId, TableId,
};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum Instr {
    LoadField {
        dst: Reg,
        field: PacketField,
    },
    StoreField {
        field: PacketField,
        src: Reg,
    },

    LoadMeta {
        dst: Reg,
        key: MetaKey,
    },
    StoreMeta {
        key: MetaKey,
        src: Reg,
    },

    Const {
        dst: Reg,
        value: u64,
    },

    Add {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Sub {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    And {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Or {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Xor {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Eq {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Lt {
        dst: Reg,
        a: Reg,
        b: Reg,
    },

    TableLookup {
        table_id: TableId,
        key_reg: Reg,
        result_meta: MetaKey,
    },

    RegisterRead {
        array: RegisterArrayId,
        index: Reg,
        dst: Reg,
    },
    RegisterWrite {
        array: RegisterArrayId,
        index: Reg,
        src: Reg,
    },

    CounterAdd {
        counter: CounterArrayId,
        index: Reg,
        value: Reg,
    },

    BranchIf {
        cond: Reg,
        target: InstrIndex,
    },

    Drop,
    Punt {
        reason: PuntReason,
    },
    SetEgress {
        port: PortId,
    },
    SetQueue {
        queue: QueueId,
    },
    Recirculate,
    Noop,

    /// Push `label` onto the MPLS stack (top of stack).
    PushLabel { label: u32 },
    /// Pop the top of the MPLS stack. No-op on an empty stack.
    PopLabel,
    /// Replace the top of the MPLS stack with `label`. Pushes if empty.
    SwapLabel { label: u32 },
    /// Mark the packet for trace. Wherever the packet terminates, the
    /// simulator synthesizes a `TraceReply` and queues it on the same
    /// egress link the packet was about to take, addressed back to this
    /// switch's controller. The reply consumes bandwidth normally.
    MarkTrace,
}

#[derive(Debug, Clone)]
pub struct StageProgram {
    pub instrs: Vec<Instr>,
    pub max_alu_ops: u32,
    pub max_memory_accesses: u8,
}

#[derive(Debug, Clone)]
pub struct TinyProgram {
    pub stages: Vec<StageProgram>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Exact,
    Lpm,
}

#[derive(Debug, Clone)]
pub enum TableAction {
    SetMeta { key: MetaKey, value: u64 },
    SetEgress { port: PortId },
    SetQueue { queue: QueueId },
    Drop,
    Punt { reason: PuntReason },
}

#[derive(Debug, Clone)]
pub struct TableEntry {
    pub id: EntryId,
    pub key: u64,
    /// For LPM: number of significant high bits.
    pub prefix_len: u8,
    pub priority: i32,
    pub action: TableAction,
}

#[derive(Debug, Clone)]
pub struct MatchActionTable {
    pub table_id: TableId,
    pub match_kind: MatchKind,
    pub entries: Vec<TableEntry>,
    pub max_entries: usize,
}

impl MatchActionTable {
    pub fn new(table_id: TableId, match_kind: MatchKind, max_entries: usize) -> Self {
        Self {
            table_id,
            match_kind,
            entries: Vec::new(),
            max_entries,
        }
    }

    pub fn install(&mut self, entry: TableEntry) -> Result<(), &'static str> {
        if self.entries.len() >= self.max_entries {
            return Err("table full");
        }
        self.entries.push(entry);
        // Sort LPM by prefix_len desc then priority desc; Exact by priority desc.
        match self.match_kind {
            MatchKind::Lpm => self
                .entries
                .sort_by(|a, b| b.prefix_len.cmp(&a.prefix_len).then(b.priority.cmp(&a.priority))),
            MatchKind::Exact => self.entries.sort_by(|a, b| b.priority.cmp(&a.priority)),
        }
        Ok(())
    }

    pub fn delete(&mut self, id: EntryId) -> bool {
        let len_before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != len_before
    }

    pub fn lookup(&self, key: u64) -> Option<&TableEntry> {
        for e in &self.entries {
            match self.match_kind {
                MatchKind::Exact => {
                    if e.key == key {
                        return Some(e);
                    }
                }
                MatchKind::Lpm => {
                    // IPv4-style LPM: `prefix_len` is measured from the
                    // MSB of the low-order 32 bits of the key.
                    // (0 -> match all, >=32 -> exact 32-bit match.)
                    let bits = e.prefix_len as u32;
                    let mask: u64 = if bits == 0 {
                        0
                    } else if bits >= 32 {
                        0xFFFF_FFFF
                    } else {
                        ((!0u32 << (32 - bits)) as u32) as u64
                    };
                    if (key & mask) == (e.key & mask) {
                        return Some(e);
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct RegisterArray {
    pub id: RegisterArrayId,
    pub data: Vec<u64>,
}

impl RegisterArray {
    pub fn new(id: RegisterArrayId, size: usize) -> Self {
        Self {
            id,
            data: vec![0; size],
        }
    }
}

#[derive(Debug, Clone)]
pub struct CounterArray {
    pub id: CounterArrayId,
    pub data: Vec<u64>,
}

impl CounterArray {
    pub fn new(id: CounterArrayId, size: usize) -> Self {
        Self {
            id,
            data: vec![0; size],
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TinyVmState {
    pub tables: Vec<MatchActionTable>,
    pub registers: Vec<RegisterArray>,
    pub counters: Vec<CounterArray>,
}

impl TinyVmState {
    pub fn table_mut(&mut self, id: TableId) -> Option<&mut MatchActionTable> {
        self.tables.iter_mut().find(|t| t.table_id == id)
    }
    pub fn table(&self, id: TableId) -> Option<&MatchActionTable> {
        self.tables.iter().find(|t| t.table_id == id)
    }
    pub fn register_mut(&mut self, id: RegisterArrayId) -> Option<&mut RegisterArray> {
        self.registers.iter_mut().find(|r| r.id == id)
    }
    pub fn register(&self, id: RegisterArrayId) -> Option<&RegisterArray> {
        self.registers.iter().find(|r| r.id == id)
    }
    pub fn counter_mut(&mut self, id: CounterArrayId) -> Option<&mut CounterArray> {
        self.counters.iter_mut().find(|c| c.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// The program has more stages than a switch's pipeline.
    TooManyStages { stages: usize, max: usize },
    LoopDetected { stage: usize },
    TooManyInstructions { stage: usize },
    TooManyAluOps { stage: usize },
    TooManyMemoryAccesses { stage: usize },
    UnknownTable { stage: usize, table: TableId },
    UnknownRegister { stage: usize, array: RegisterArrayId },
    UnknownCounter { stage: usize, counter: CounterArrayId },
    BadBranchTarget { stage: usize, target: u32 },
    MultipleMemoryResources { stage: usize },
}

/// How many stages a program may have. A switch's pipeline is a fixed
/// piece of hardware; a program that wants more work per packet
/// recirculates, it does not grow the pipeline.
pub const MAX_STAGES: usize = 8;

#[derive(Debug, Clone)]
pub struct ValidatorLimits {
    pub max_stages: usize,
    pub max_instrs_per_stage: usize,
    pub max_memory_resources_per_stage: u8,
}

impl Default for ValidatorLimits {
    fn default() -> Self {
        Self {
            max_stages: MAX_STAGES,
            max_instrs_per_stage: 64,
            max_memory_resources_per_stage: 1,
        }
    }
}

/// Validate a TinyProgram against the simulator state and limits.
///
/// Spec'd checks:
/// * loops via BranchIf must be acyclic
/// * instruction-count budget per stage
/// * ALU op budget per stage
/// * memory access budget per stage
/// * tables/registers/counters referenced must exist
/// * at most one memory resource per stage unless allowed
pub fn validate(
    program: &TinyProgram,
    state: &TinyVmState,
    limits: &ValidatorLimits,
) -> Result<(), ValidationError> {
    if program.stages.len() > limits.max_stages {
        return Err(ValidationError::TooManyStages {
            stages: program.stages.len(),
            max: limits.max_stages,
        });
    }
    for (stage_idx, stage) in program.stages.iter().enumerate() {
        if stage.instrs.len() > limits.max_instrs_per_stage {
            return Err(ValidationError::TooManyInstructions { stage: stage_idx });
        }

        let mut alu_count = 0u32;
        let mut mem_count = 0u8;
        let mut distinct_mem_resources: Vec<u64> = Vec::new();

        for (i, instr) in stage.instrs.iter().enumerate() {
            match instr {
                Instr::Add { .. }
                | Instr::Sub { .. }
                | Instr::And { .. }
                | Instr::Or { .. }
                | Instr::Xor { .. }
                | Instr::Eq { .. }
                | Instr::Lt { .. }
                | Instr::Const { .. } => {
                    alu_count += 1;
                }
                Instr::TableLookup { table_id, .. } => {
                    mem_count += 1;
                    let key = (1u64 << 60) | (table_id.raw() as u64);
                    if !distinct_mem_resources.contains(&key) {
                        distinct_mem_resources.push(key);
                    }
                    if state.table(*table_id).is_none() {
                        return Err(ValidationError::UnknownTable {
                            stage: stage_idx,
                            table: *table_id,
                        });
                    }
                }
                Instr::RegisterRead { array, .. } | Instr::RegisterWrite { array, .. } => {
                    mem_count += 1;
                    let key = (2u64 << 60) | (array.raw() as u64);
                    if !distinct_mem_resources.contains(&key) {
                        distinct_mem_resources.push(key);
                    }
                    if state.register(*array).is_none() {
                        return Err(ValidationError::UnknownRegister {
                            stage: stage_idx,
                            array: *array,
                        });
                    }
                }
                Instr::CounterAdd { counter, .. } => {
                    mem_count += 1;
                    let key = (3u64 << 60) | (counter.raw() as u64);
                    if !distinct_mem_resources.contains(&key) {
                        distinct_mem_resources.push(key);
                    }
                    if state.counters.iter().all(|c| c.id != *counter) {
                        return Err(ValidationError::UnknownCounter {
                            stage: stage_idx,
                            counter: *counter,
                        });
                    }
                }
                Instr::BranchIf { target, .. } => {
                    if (target.raw() as usize) >= stage.instrs.len() {
                        return Err(ValidationError::BadBranchTarget {
                            stage: stage_idx,
                            target: target.raw(),
                        });
                    }
                    // Loop = branch to <= current index.
                    if (target.raw() as usize) <= i {
                        return Err(ValidationError::LoopDetected { stage: stage_idx });
                    }
                }
                _ => {}
            }
        }

        if alu_count > stage.max_alu_ops {
            return Err(ValidationError::TooManyAluOps { stage: stage_idx });
        }
        if mem_count > stage.max_memory_accesses {
            return Err(ValidationError::TooManyMemoryAccesses { stage: stage_idx });
        }
        if (distinct_mem_resources.len() as u8) > limits.max_memory_resources_per_stage {
            return Err(ValidationError::MultipleMemoryResources { stage: stage_idx });
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageDecision {
    Continue,
    Drop,
    Punt(PuntReason),
    Recirculate,
}

#[derive(Debug, Clone, Default)]
pub struct PipelineResult {
    pub egress_port: Option<PortId>,
    pub queue: Option<QueueId>,
    pub decision: Option<StageDecision>,
    /// `Instr::MarkTrace` ran during this packet's pass through the
    /// pipeline. The simulator records the marking switch + chosen
    /// egress port on the packet's trail.
    pub trace_marked: bool,
}

/// Execute a single stage on the packet. Mutates the packet's metadata
/// and the TinyVM state. Returns (decision, accumulated egress/queue).
pub fn run_stage(
    stage: &StageProgram,
    packet: &mut Packet,
    state: &mut TinyVmState,
    result: &mut PipelineResult,
) -> StageDecision {
    let mut regs: HashMap<u8, u64> = HashMap::new();
    let mut pc: usize = 0;

    while pc < stage.instrs.len() {
        let instr = &stage.instrs[pc];
        let mut next_pc = pc + 1;

        match instr {
            Instr::LoadField { dst, field } => {
                regs.insert(dst.raw(), packet.read_field(*field));
            }
            Instr::StoreField { field, src } => {
                let v = *regs.get(&src.raw()).unwrap_or(&0);
                packet.write_field(*field, v);
            }
            Instr::LoadMeta { dst, key } => {
                let v = *packet.metadata.get(key).unwrap_or(&0);
                regs.insert(dst.raw(), v);
            }
            Instr::StoreMeta { key, src } => {
                let v = *regs.get(&src.raw()).unwrap_or(&0);
                packet.metadata.insert(*key, v);
            }
            Instr::Const { dst, value } => {
                regs.insert(dst.raw(), *value);
            }
            Instr::Add { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), va.wrapping_add(vb));
            }
            Instr::Sub { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), va.wrapping_sub(vb));
            }
            Instr::And { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), va & vb);
            }
            Instr::Or { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), va | vb);
            }
            Instr::Xor { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), va ^ vb);
            }
            Instr::Eq { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), if va == vb { 1 } else { 0 });
            }
            Instr::Lt { dst, a, b } => {
                let va = *regs.get(&a.raw()).unwrap_or(&0);
                let vb = *regs.get(&b.raw()).unwrap_or(&0);
                regs.insert(dst.raw(), if va < vb { 1 } else { 0 });
            }
            Instr::TableLookup {
                table_id,
                key_reg,
                result_meta,
            } => {
                let key = *regs.get(&key_reg.raw()).unwrap_or(&0);
                if let Some(table) = state.table(*table_id) {
                    if let Some(entry) = table.lookup(key) {
                        // Apply action immediately as well as record into metadata.
                        match &entry.action {
                            TableAction::SetMeta { key, value } => {
                                packet.metadata.insert(*key, *value);
                            }
                            TableAction::SetEgress { port } => {
                                result.egress_port = Some(*port);
                            }
                            TableAction::SetQueue { queue } => {
                                result.queue = Some(*queue);
                            }
                            TableAction::Drop => {
                                return StageDecision::Drop;
                            }
                            TableAction::Punt { reason } => {
                                return StageDecision::Punt(*reason);
                            }
                        }
                        packet.metadata.insert(*result_meta, entry.id.raw());
                    }
                }
            }
            Instr::RegisterRead { array, index, dst } => {
                let idx = *regs.get(&index.raw()).unwrap_or(&0) as usize;
                if let Some(arr) = state.register(*array) {
                    let v = arr.data.get(idx).copied().unwrap_or(0);
                    regs.insert(dst.raw(), v);
                }
            }
            Instr::RegisterWrite { array, index, src } => {
                let idx = *regs.get(&index.raw()).unwrap_or(&0) as usize;
                let v = *regs.get(&src.raw()).unwrap_or(&0);
                if let Some(arr) = state.register_mut(*array) {
                    if let Some(slot) = arr.data.get_mut(idx) {
                        *slot = v;
                    }
                }
            }
            Instr::CounterAdd {
                counter,
                index,
                value,
            } => {
                let idx = *regs.get(&index.raw()).unwrap_or(&0) as usize;
                let v = *regs.get(&value.raw()).unwrap_or(&0);
                if let Some(c) = state.counter_mut(*counter) {
                    if let Some(slot) = c.data.get_mut(idx) {
                        *slot = slot.wrapping_add(v);
                    }
                }
            }
            Instr::BranchIf { cond, target } => {
                if *regs.get(&cond.raw()).unwrap_or(&0) != 0 {
                    next_pc = target.raw() as usize;
                }
            }
            Instr::Drop => return StageDecision::Drop,
            Instr::Punt { reason } => return StageDecision::Punt(*reason),
            Instr::SetEgress { port } => {
                result.egress_port = Some(*port);
            }
            Instr::SetQueue { queue } => {
                result.queue = Some(*queue);
            }
            Instr::Recirculate => return StageDecision::Recirculate,
            Instr::Noop => {}
            Instr::PushLabel { label } => {
                packet.labels.push(*label);
            }
            Instr::PopLabel => {
                packet.labels.pop();
            }
            Instr::SwapLabel { label } => {
                if let Some(top) = packet.labels.last_mut() {
                    *top = *label;
                } else {
                    packet.labels.push(*label);
                }
            }
            Instr::MarkTrace => {
                result.trace_marked = true;
            }
        }

        pc = next_pc;
    }

    StageDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PacketId;
    use std::time::Duration;

    fn empty_state() -> TinyVmState {
        TinyVmState::default()
    }

    fn pkt() -> Packet {
        let mut p = Packet::new(PacketId::new(0), Duration::ZERO, 100);
        p.ip_dst = crate::packet::IpAddr(0x0a000001);
        p
    }

    #[test]
    fn field_load_store() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::LoadField {
                    dst: Reg::new(0),
                    field: PacketField::IpDst,
                },
                Instr::StoreField {
                    field: PacketField::IpSrc,
                    src: Reg::new(0),
                },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(p.ip_src.0, 0x0a000001);
    }

    #[test]
    fn meta_load_store() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        p.metadata.insert(MetaKey::new(7), 42);
        let stage = StageProgram {
            instrs: vec![
                Instr::LoadMeta {
                    dst: Reg::new(0),
                    key: MetaKey::new(7),
                },
                Instr::StoreMeta {
                    key: MetaKey::new(8),
                    src: Reg::new(0),
                },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(p.metadata[&MetaKey::new(8)], 42);
    }

    #[test]
    fn arith_and_branch() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::Const {
                    dst: Reg::new(0),
                    value: 3,
                },
                Instr::Const {
                    dst: Reg::new(1),
                    value: 4,
                },
                Instr::Add {
                    dst: Reg::new(2),
                    a: Reg::new(0),
                    b: Reg::new(1),
                },
                Instr::Const {
                    dst: Reg::new(3),
                    value: 7,
                },
                Instr::Eq {
                    dst: Reg::new(4),
                    a: Reg::new(2),
                    b: Reg::new(3),
                },
                Instr::BranchIf {
                    cond: Reg::new(4),
                    target: InstrIndex::new(7),
                },
                Instr::Drop,
                Instr::Noop,
            ],
            max_alu_ops: 16,
            max_memory_accesses: 0,
        };
        let dec = run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(dec, StageDecision::Continue);
    }

    #[test]
    fn drop_action() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![Instr::Drop],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        let dec = run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(dec, StageDecision::Drop);
    }

    #[test]
    fn punt_action() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![Instr::Punt {
                reason: PuntReason::NoRoute,
            }],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        let dec = run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(dec, StageDecision::Punt(PuntReason::NoRoute));
    }

    #[test]
    fn set_egress_action() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![Instr::SetEgress {
                port: PortId::new(3),
            }],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(r.egress_port, Some(PortId::new(3)));
    }

    #[test]
    fn set_queue_action() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![Instr::SetQueue {
                queue: QueueId::new(2),
            }],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(r.queue, Some(QueueId::new(2)));
    }

    #[test]
    fn recirculate_action() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![Instr::Recirculate],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        let dec = run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(dec, StageDecision::Recirculate);
    }

    #[test]
    fn table_lookup_exact() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut t = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
        t.install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000001,
            prefix_len: 32,
            priority: 0,
            action: TableAction::SetEgress {
                port: PortId::new(7),
            },
        })
        .unwrap();
        s.tables.push(t);
        let mut r = PipelineResult::default();
        let stage = StageProgram {
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
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(r.egress_port, Some(PortId::new(7)));
    }

    #[test]
    fn table_lookup_lpm() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut t = MatchActionTable::new(TableId::new(1), MatchKind::Lpm, 16);
        t.install(TableEntry {
            id: EntryId::new(1),
            key: 0x0a000000,
            prefix_len: 8,
            priority: 0,
            action: TableAction::SetEgress {
                port: PortId::new(2),
            },
        })
        .unwrap();
        t.install(TableEntry {
            id: EntryId::new(2),
            key: 0x0a000000,
            prefix_len: 16,
            priority: 0,
            action: TableAction::SetEgress {
                port: PortId::new(3),
            },
        })
        .unwrap();
        s.tables.push(t);

        // ip_dst = 0x0a000001; should match longer prefix entry (port 3).
        // LPM masks bottom-32-bit-window from MSB of the address.
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::Const { dst: Reg::new(0), value: 0x0a000001 },
                Instr::TableLookup {
                    table_id: TableId::new(1),
                    key_reg: Reg::new(0),
                    result_meta: MetaKey::new(0),
                },
            ],
            max_alu_ops: 1,
            max_memory_accesses: 1,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(r.egress_port, Some(PortId::new(3)));
    }

    #[test]
    fn register_read_write() {
        let mut p = pkt();
        let mut s = empty_state();
        s.registers.push(RegisterArray::new(RegisterArrayId::new(0), 4));
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::Const {
                    dst: Reg::new(0),
                    value: 1,
                },
                Instr::Const {
                    dst: Reg::new(1),
                    value: 99,
                },
                Instr::RegisterWrite {
                    array: RegisterArrayId::new(0),
                    index: Reg::new(0),
                    src: Reg::new(1),
                },
            ],
            max_alu_ops: 2,
            max_memory_accesses: 1,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(s.registers[0].data[1], 99);

        let mut r = PipelineResult::default();
        let stage2 = StageProgram {
            instrs: vec![
                Instr::Const {
                    dst: Reg::new(0),
                    value: 1,
                },
                Instr::RegisterRead {
                    array: RegisterArrayId::new(0),
                    index: Reg::new(0),
                    dst: Reg::new(2),
                },
                Instr::StoreMeta {
                    key: MetaKey::new(0),
                    src: Reg::new(2),
                },
            ],
            max_alu_ops: 1,
            max_memory_accesses: 1,
        };
        run_stage(&stage2, &mut p, &mut s, &mut r);
        assert_eq!(p.metadata[&MetaKey::new(0)], 99);
    }

    #[test]
    fn counter_increment() {
        let mut p = pkt();
        let mut s = empty_state();
        s.counters.push(CounterArray::new(CounterArrayId::new(0), 4));
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::Const {
                    dst: Reg::new(0),
                    value: 2,
                },
                Instr::Const {
                    dst: Reg::new(1),
                    value: 1,
                },
                Instr::CounterAdd {
                    counter: CounterArrayId::new(0),
                    index: Reg::new(0),
                    value: Reg::new(1),
                },
            ],
            max_alu_ops: 2,
            max_memory_accesses: 1,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(s.counters[0].data[2], 2);
    }

    #[test]
    fn label_push_pop_swap() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::PushLabel { label: 100 },
                Instr::PushLabel { label: 200 },
                Instr::SwapLabel { label: 250 },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(p.labels, vec![100, 250]);

        let stage2 = StageProgram {
            instrs: vec![Instr::PopLabel],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage2, &mut p, &mut s, &mut r);
        assert_eq!(p.labels, vec![100]);
    }

    #[test]
    fn label_top_and_depth_fields() {
        let mut p = pkt();
        let mut s = empty_state();
        let mut r = PipelineResult::default();
        let stage = StageProgram {
            instrs: vec![
                Instr::PushLabel { label: 7 },
                Instr::PushLabel { label: 11 },
                Instr::LoadField { dst: Reg::new(0), field: PacketField::LabelTop },
                Instr::StoreMeta { key: MetaKey::new(0), src: Reg::new(0) },
                Instr::LoadField { dst: Reg::new(1), field: PacketField::LabelDepth },
                Instr::StoreMeta { key: MetaKey::new(1), src: Reg::new(1) },
            ],
            max_alu_ops: 0,
            max_memory_accesses: 0,
        };
        run_stage(&stage, &mut p, &mut s, &mut r);
        assert_eq!(p.metadata[&MetaKey::new(0)], 11);
        assert_eq!(p.metadata[&MetaKey::new(1)], 2);
    }

    #[test]
    fn loop_rejection() {
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::Const {
                        dst: Reg::new(0),
                        value: 1,
                    },
                    Instr::BranchIf {
                        cond: Reg::new(0),
                        target: InstrIndex::new(0),
                    },
                ],
                max_alu_ops: 8,
                max_memory_accesses: 0,
            }],
        };
        let s = empty_state();
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(err, ValidationError::LoopDetected { .. }));
    }

    #[test]
    fn instr_budget_enforced() {
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![Instr::Noop; 1000],
                max_alu_ops: 0,
                max_memory_accesses: 0,
            }],
        };
        let s = empty_state();
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(err, ValidationError::TooManyInstructions { .. }));
    }

    #[test]
    fn alu_budget_enforced() {
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::Const {
                        dst: Reg::new(0),
                        value: 1,
                    };
                    5
                ],
                max_alu_ops: 2,
                max_memory_accesses: 0,
            }],
        };
        let s = empty_state();
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(err, ValidationError::TooManyAluOps { .. }));
    }

    #[test]
    fn memory_budget_enforced() {
        let mut s = empty_state();
        s.registers.push(RegisterArray::new(RegisterArrayId::new(0), 4));
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::Const {
                        dst: Reg::new(0),
                        value: 0,
                    },
                    Instr::RegisterRead {
                        array: RegisterArrayId::new(0),
                        index: Reg::new(0),
                        dst: Reg::new(1),
                    },
                    Instr::RegisterRead {
                        array: RegisterArrayId::new(0),
                        index: Reg::new(0),
                        dst: Reg::new(2),
                    },
                ],
                max_alu_ops: 1,
                max_memory_accesses: 1,
            }],
        };
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(err, ValidationError::TooManyMemoryAccesses { .. }));
    }

    #[test]
    fn unknown_table_rejected() {
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::Const {
                        dst: Reg::new(0),
                        value: 0,
                    },
                    Instr::TableLookup {
                        table_id: TableId::new(99),
                        key_reg: Reg::new(0),
                        result_meta: MetaKey::new(0),
                    },
                ],
                max_alu_ops: 1,
                max_memory_accesses: 1,
            }],
        };
        let s = empty_state();
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(err, ValidationError::UnknownTable { .. }));
    }

    #[test]
    fn multiple_memory_resources_rejected() {
        let mut s = empty_state();
        s.registers.push(RegisterArray::new(RegisterArrayId::new(0), 4));
        s.tables.push(MatchActionTable::new(
            TableId::new(0),
            MatchKind::Exact,
            4,
        ));
        let prog = TinyProgram {
            stages: vec![StageProgram {
                instrs: vec![
                    Instr::Const {
                        dst: Reg::new(0),
                        value: 0,
                    },
                    Instr::RegisterRead {
                        array: RegisterArrayId::new(0),
                        index: Reg::new(0),
                        dst: Reg::new(1),
                    },
                    Instr::TableLookup {
                        table_id: TableId::new(0),
                        key_reg: Reg::new(0),
                        result_meta: MetaKey::new(0),
                    },
                ],
                max_alu_ops: 1,
                max_memory_accesses: 2,
            }],
        };
        let err = validate(&prog, &s, &ValidatorLimits::default()).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::MultipleMemoryResources { .. }
        ));
    }
}
