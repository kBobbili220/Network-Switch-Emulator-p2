# TinyVM

TinyVM is the data-plane language each switch runs against every packet that
arrives. It is intentionally not Turing-complete: there are no loops, no
recursion, no heap, no dynamic allocation, and no hidden persistent state.
The only persistent state a program can touch is what the simulator owns
(tables, register arrays, counter arrays).

If a program needs more processing than fits in a single pipeline pass, it
must explicitly **recirculate** the packet — which costs another pipeline
traversal.

## Pipeline shape

```
structured packet
  → ingress TinyVM stages (run in order)
  → queue selection (SetQueue)
  → egress port selection (SetEgress)
  → egress link
```

A `TinyProgram` is a `Vec<StageProgram>`. Stages execute sequentially. Each
stage is its own straight-line program with its own ALU and memory budgets:

```rust
pub struct StageProgram {
    pub instrs: Vec<Instr>,
    pub max_alu_ops: u32,
    pub max_memory_accesses: u8,
}

pub struct TinyProgram {
    pub stages: Vec<StageProgram>,
}
```

A program has at most **8** stages (`tinyvm::MAX_STAGES`); the loader
refuses a longer one. Within a stage, registers (`Reg`) are scratch space —
they reset between stages, so every stage loads the fields it reads.
Metadata (`MetaKey`) is per-packet and persists across stages and
recirculations.

## Instruction set

```rust
pub enum Instr {
    LoadField  { dst, field },     // packet field -> reg
    StoreField { field, src },     // reg -> packet field
    LoadMeta   { dst, key },       // packet metadata -> reg
    StoreMeta  { key, src },       // reg -> packet metadata
    Const      { dst, value },

    Add | Sub | And | Or | Xor | Eq | Lt { dst, a, b },

    TableLookup { table_id, key_reg, result_meta },

    RegisterRead  { array, index, dst },
    RegisterWrite { array, index, src },
    CounterAdd    { counter, index, value },

    BranchIf { cond, target },     // forward-only branch (see below)

    Drop,
    Punt        { reason },
    SetEgress   { port },
    SetQueue    { queue },
    Recirculate,
    Noop,
}
```

`PacketField` exposes the structured packet to the program:
`IpSrc`, `IpDst`, `IpProto`, `IpTtl`,
`IpDscp`, `SrcPort`, `DstPort`, `AppId`, `FlowId`, `Custom(0..=3)`,
`Size`. There is no real parsing — these accessors return / write the
already-typed fields on the `Packet`.

### Branches and the no-loops rule

`BranchIf` exists so programs can express conditionals, but it is
**forward-only**: a target less than or equal to the current instruction
index is rejected by the validator with `LoopDetected`. Falling off the end
of a stage's instruction vector ends the stage with `Continue`.

Pattern for a branch-around-block:

```text
0  LoadMeta r0, key 0
1  Const    r1 = 1
2  Eq       r2 = r0 == r1
3  BranchIf r2 -> 7        ; if "second pass", skip to forward path
4  Const    r3 = 1
5  StoreMeta key 0 = r3
6  Recirculate              ; (terminates this pass)
7  SetEgress port = 1
```

### Terminating instructions

`Drop`, `Punt`, and `Recirculate` end the pipeline immediately for the
current packet. `Drop` discards it; `Punt` sends it up the punt pipe to the
controller; `Recirculate` re-enters ingress (subject to
`max_recirculations`).

`SetEgress` and `SetQueue` do **not** terminate the stage — they record an
output decision into a `PipelineResult`. The pipeline only forwards the
packet if all stages run to completion (or hit a non-terminating end) and
some stage set an egress port. If no stage set an egress, the simulator
treats it as `NoEgress` and punts with `PuntReason::NoRoute`.

## Match-action tables

Tables are simulator-owned, addressed by `TableId`, looked up via
`TableLookup`:

```rust
pub struct MatchActionTable {
    pub table_id: TableId,
    pub match_kind: MatchKind,   // Exact | Lpm
    pub entries: Vec<TableEntry>,
    pub max_entries: usize,
}

pub struct TableEntry {
    pub id: EntryId,
    pub key: u64,
    pub prefix_len: u8,          // for LPM
    pub priority: i32,
    pub action: TableAction,     // SetMeta | SetEgress | SetQueue | Drop | Punt
}
```

`install` and `delete` keep entries sorted: LPM by `(prefix_len desc,
priority desc)`, Exact by `priority desc`. The first matching entry wins.

When `TableLookup` fires the action is applied immediately:

- `SetMeta` writes a metadata key
- `SetEgress` / `SetQueue` populate the pipeline result
- `Drop` / `Punt` terminate the stage with the corresponding decision

The matched entry's `id` is also written into `result_meta` so subsequent
stages can branch on whether a hit occurred.

## Registers and counters

```rust
pub struct RegisterArray { pub id: RegisterArrayId, pub data: Vec<u64> }
pub struct CounterArray  { pub id: CounterArrayId,  pub data: Vec<u64> }
```

Register arrays are read/write with arbitrary indices (saturating to no-op
out of bounds). Counter arrays are write-only via `CounterAdd`. Both live
in `TinyVmState` alongside tables.

## Validation

Before simulation runs, validate every program against the switch's state
and limits:

```rust
let limits = ValidatorLimits::default();
validate(&program, &state, &limits)?;
```

Rejections returned as `ValidationError`:

- `LoopDetected` — `BranchIf` target ≤ current index
- `TooManyInstructions` — stage exceeds `limits.max_instrs_per_stage`
- `TooManyAluOps` — stage exceeds its own `max_alu_ops`
- `TooManyMemoryAccesses` — stage exceeds its own `max_memory_accesses`
- `UnknownTable` / `UnknownRegister` / `UnknownCounter` — referenced resource not in state
- `BadBranchTarget` — out-of-range branch index
- `MultipleMemoryResources` — stage touches more than one distinct memory
  resource (table / register-array / counter-array), unless the limit is
  raised. Two reads of the *same* register array count as one resource but
  two memory accesses.

`max_alu_ops` covers `Add`, `Sub`, `And`, `Or`, `Xor`, `Eq`, `Lt`, `Const`.
`max_memory_accesses` covers `TableLookup`, `RegisterRead`,
`RegisterWrite`, `CounterAdd`.

## Worked example: IPv4 destination → egress port

A one-stage program that forwards based on `ip_dst` via an Exact-match
table:

```rust
let mut table = MatchActionTable::new(TableId::new(1), MatchKind::Exact, 16);
table.install(TableEntry {
    id: EntryId::new(1),
    key: 0x0a000002,
    prefix_len: 32,
    priority: 0,
    action: TableAction::SetEgress { port: PortId::new(1) },
})?;

let prog = TinyProgram { stages: vec![StageProgram {
    instrs: vec![
        Instr::LoadField   { dst: Reg::new(0), field: PacketField::IpDst },
        Instr::TableLookup { table_id: TableId::new(1),
                             key_reg: Reg::new(0),
                             result_meta: MetaKey::new(0) },
    ],
    max_alu_ops: 0,
    max_memory_accesses: 1,
}]};
```

The same program with LPM only differs in the `MatchKind` and how keys are
laid out (LPM masks the high bits of a `u64`, so callers typically shift
the IP into the upper word).
