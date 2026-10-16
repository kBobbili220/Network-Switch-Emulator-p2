//! Convert simulator-native `TinyProgram` / `TinyVmState` /
//! `TableEntry` types into their `switch_program_types` wire mirrors.
//! Used by the event-log writer; the inverse direction lives in
//! `crate::wasm`.

use switch_program_types as wire;

use crate::packet::{PacketField, PuntReason};
use crate::tinyvm::{Instr, MatchKind, TableAction, TableEntry, TinyProgram, TinyVmState};

pub fn instr_to_wire(i: &Instr) -> wire::InstrW {
    use Instr as I;
    match i {
        I::LoadField { dst, field } => wire::InstrW::LoadField {
            dst: wire::RegW(dst.raw()),
            field: field_to_wire(*field),
        },
        I::StoreField { field, src } => wire::InstrW::StoreField {
            field: field_to_wire(*field),
            src: wire::RegW(src.raw()),
        },
        I::LoadMeta { dst, key } => wire::InstrW::LoadMeta {
            dst: wire::RegW(dst.raw()),
            key: wire::MetaKeyW(key.raw()),
        },
        I::StoreMeta { key, src } => wire::InstrW::StoreMeta {
            key: wire::MetaKeyW(key.raw()),
            src: wire::RegW(src.raw()),
        },
        I::Const { dst, value } => wire::InstrW::Const {
            dst: wire::RegW(dst.raw()),
            value: *value,
        },
        I::Add { dst, a, b } => wire::InstrW::Add { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::Sub { dst, a, b } => wire::InstrW::Sub { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::And { dst, a, b } => wire::InstrW::And { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::Or  { dst, a, b } => wire::InstrW::Or  { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::Xor { dst, a, b } => wire::InstrW::Xor { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::Eq  { dst, a, b } => wire::InstrW::Eq  { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::Lt  { dst, a, b } => wire::InstrW::Lt  { dst: wire::RegW(dst.raw()), a: wire::RegW(a.raw()), b: wire::RegW(b.raw()) },
        I::TableLookup { table_id, key_reg, result_meta } => wire::InstrW::TableLookup {
            table_id: wire::TableIdW(table_id.raw()),
            key_reg: wire::RegW(key_reg.raw()),
            result_meta: wire::MetaKeyW(result_meta.raw()),
        },
        I::RegisterRead { array, index, dst } => wire::InstrW::RegisterRead {
            array: wire::RegisterArrayIdW(array.raw()),
            index: wire::RegW(index.raw()),
            dst: wire::RegW(dst.raw()),
        },
        I::RegisterWrite { array, index, src } => wire::InstrW::RegisterWrite {
            array: wire::RegisterArrayIdW(array.raw()),
            index: wire::RegW(index.raw()),
            src: wire::RegW(src.raw()),
        },
        I::CounterAdd { counter, index, value } => wire::InstrW::CounterAdd {
            counter: wire::CounterArrayIdW(counter.raw()),
            index: wire::RegW(index.raw()),
            value: wire::RegW(value.raw()),
        },
        I::BranchIf { cond, target } => wire::InstrW::BranchIf {
            cond: wire::RegW(cond.raw()),
            target: target.raw(),
        },
        I::Drop => wire::InstrW::Drop,
        I::Punt { reason } => wire::InstrW::Punt { reason: punt_reason_to_wire(*reason) },
        I::SetEgress { port } => wire::InstrW::SetEgress { port: wire::PortIdW(port.raw()) },
        I::SetQueue { queue } => wire::InstrW::SetQueue { queue: wire::QueueIdW(queue.raw()) },
        I::Recirculate => wire::InstrW::Recirculate,
        I::Noop => wire::InstrW::Noop,
        I::PushLabel { label } => wire::InstrW::PushLabel { label: *label },
        I::PopLabel => wire::InstrW::PopLabel,
        I::SwapLabel { label } => wire::InstrW::SwapLabel { label: *label },
        I::MarkTrace => wire::InstrW::MarkTrace,
    }
}

pub fn field_to_wire(f: PacketField) -> wire::PacketFieldW {
    match f {
        PacketField::IpSrc => wire::PacketFieldW::IpSrc,
        PacketField::IpDst => wire::PacketFieldW::IpDst,
        PacketField::IpProto => wire::PacketFieldW::IpProto,
        PacketField::IpTtl => wire::PacketFieldW::IpTtl,
        PacketField::IpDscp => wire::PacketFieldW::IpDscp,
        PacketField::SrcPort => wire::PacketFieldW::SrcPort,
        PacketField::DstPort => wire::PacketFieldW::DstPort,
        PacketField::AppId => wire::PacketFieldW::AppId,
        PacketField::FlowId => wire::PacketFieldW::FlowId,
        PacketField::Custom(i) => wire::PacketFieldW::Custom(i),
        PacketField::Size => wire::PacketFieldW::Size,
        PacketField::Kind => wire::PacketFieldW::Kind,
        PacketField::LabelTop => wire::PacketFieldW::LabelTop,
        PacketField::LabelDepth => wire::PacketFieldW::LabelDepth,
    }
}

pub fn punt_reason_to_wire(r: PuntReason) -> wire::PuntReasonW {
    match r {
        PuntReason::NoRoute => wire::PuntReasonW::NoRoute,
        PuntReason::TtlExpired => wire::PuntReasonW::TtlExpired,
        PuntReason::Custom(c) => wire::PuntReasonW::Custom(c),
    }
}

pub fn match_kind_to_wire(k: MatchKind) -> wire::MatchKindW {
    match k {
        MatchKind::Exact => wire::MatchKindW::Exact,
        MatchKind::Lpm => wire::MatchKindW::Lpm,
    }
}

pub fn table_action_to_wire(a: &TableAction) -> wire::TableActionW {
    match a {
        TableAction::SetMeta { key, value } => wire::TableActionW::SetMeta {
            key: wire::MetaKeyW(key.raw()),
            value: *value,
        },
        TableAction::SetEgress { port } => wire::TableActionW::SetEgress {
            port: wire::PortIdW(port.raw()),
        },
        TableAction::SetQueue { queue } => wire::TableActionW::SetQueue {
            queue: wire::QueueIdW(queue.raw()),
        },
        TableAction::Drop => wire::TableActionW::Drop,
        TableAction::Punt { reason } => wire::TableActionW::Punt {
            reason: punt_reason_to_wire(*reason),
        },
    }
}

pub fn table_entry_to_wire(e: &TableEntry) -> wire::TableEntryW {
    wire::TableEntryW {
        id: wire::EntryIdW(e.id.raw()),
        key: e.key,
        prefix_len: e.prefix_len,
        priority: e.priority,
        action: table_action_to_wire(&e.action),
    }
}

pub fn tiny_program_to_wire(p: &TinyProgram) -> wire::TinyProgramW {
    wire::TinyProgramW {
        stages: p
            .stages
            .iter()
            .map(|s| wire::StageProgramW {
                instrs: s.instrs.iter().map(instr_to_wire).collect(),
                max_alu_ops: s.max_alu_ops,
                max_memory_accesses: s.max_memory_accesses,
            })
            .collect(),
    }
}

pub fn state_snapshots(
    state: &TinyVmState,
) -> (
    Vec<crate::sim_log::TableSnapshotW>,
    Vec<crate::sim_log::RegSnapshotW>,
    Vec<crate::sim_log::CounterSnapshotW>,
) {
    let tables = state
        .tables
        .iter()
        .map(|t| crate::sim_log::TableSnapshotW {
            table_id: t.table_id.raw(),
            kind: match_kind_to_wire(t.match_kind),
            max_entries: t.max_entries as u32,
            entries: t.entries.iter().map(table_entry_to_wire).collect(),
        })
        .collect();
    let registers = state
        .registers
        .iter()
        .map(|r| crate::sim_log::RegSnapshotW {
            array_id: r.id.raw(),
            size: r.data.len() as u32,
        })
        .collect();
    let counters = state
        .counters
        .iter()
        .map(|c| crate::sim_log::CounterSnapshotW {
            array_id: c.id.raw(),
            size: c.data.len() as u32,
        })
        .collect();
    (tables, registers, counters)
}
