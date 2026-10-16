//! WASM-backed switch programs.
//!
//! A *switch program* is a single `.wasm` module supplied by the switch's
//! owner. The simulator instantiates the module once per switch, calls
//! `init` to obtain the data-plane TinyVM program and state declarations,
//! and uses the module's controller handlers for the lifetime of the
//! simulation.
//!
//! # ABI
//!
//! The module must export a linear `memory` and the following functions:
//!
//! ```text
//! // input  bytes (postcard `InitInputW { switch_id }`) at in_ptr..in_ptr+in_len
//! // output bytes (postcard `ProgramSetupW`) written into out_ptr..out_ptr+ret
//! init(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32
//!
//! // postcard `PuntEventW` -> postcard `Vec<ActionW>`
//! on_punt(in_ptr, in_len, out_ptr, out_cap) -> i32
//!
//! // postcard `TimerEventW` -> postcard `Vec<ActionW>`
//! on_timer(in_ptr, in_len, out_ptr, out_cap) -> i32
//!
//! // postcard `LinkEventW` -> postcard `Vec<ActionW>`
//! on_link_event(in_ptr, in_len, out_ptr, out_cap) -> i32
//! ```
//!
//! Returning a negative value from any export marks the program as failed:
//! the data plane keeps running but the controller stops receiving events.
//! Traps, fuel exhaustion, and decode failures have the same effect.

use crate::controller::{
    ControllerAction, Discipline, LinkEvent, PortDrops, PuntEvent, QueueConfig, QueueSpec, SwitchController,
};
use crate::packet::{PacketField, PuntReason};
use crate::tinyvm::{
    CounterArray, Instr, MatchActionTable, MatchKind, RegisterArray, StageProgram, TableAction,
    TableEntry, TinyProgram, TinyVmState,
};
use crate::types::{
    CounterArrayId, EntryId, MetaKey, PortId, QueueId, Reg, RegisterArrayId, SimTime, TableId,
};

use switch_program_types as wire;
use wasmtime::{Engine, Linker, Memory, Module, Store, Trap, TypedFunc};

#[derive(Debug, Clone)]
pub struct WasmLimits {
    pub fuel_per_call: u64,
    pub max_output_bytes: u32,
    pub input_buffer_offset: u32,
    pub output_buffer_offset: u32,
    /// Ceilings on what a program may declare in `init`. Without these a
    /// single line in a switch program sizes an allocation on the host --
    /// a register array of 4e9 slots is a `vec![0; 4e9]` -- so they are
    /// enforced before anything is built.
    pub max_tables: usize,
    pub max_table_entries: u32,
    pub max_register_arrays: usize,
    pub max_register_slots: u32,
    pub max_counter_arrays: usize,
    pub max_counter_slots: u32,
    /// Ceiling on one instance's linear memory. Without it a module can
    /// `memory.grow` toward wasm32's 4 GiB -- cheap enough in instructions
    /// to fit inside the fuel budget -- across every switch in the world,
    /// and take the host down. Fuel stops a program that runs forever; it
    /// does nothing about one that asks for everything in a single step.
    pub max_memory_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            // Measured, not guessed: the reference solution peaks at
            // ~193k fuel per call on a 15-switch Part 2 world, and cost
            // grows about linearly with switch count. 4M leaves a student
            // solution room to be ~20x less efficient than the reference
            // -- a full recompute every tick instead of a diff -- before
            // it trips. Deliberately loose for A1; tighten once real
            // submissions say what the spread actually is.
            fuel_per_call: 4_000_000,
            max_output_bytes: 4096,
            input_buffer_offset: 0,
            output_buffer_offset: 8192,
            max_tables: 16,
            max_table_entries: 1024,
            max_register_arrays: 16,
            max_register_slots: 4096,
            max_counter_arrays: 16,
            max_counter_slots: 4096,
            // A link-state solution on a 15-switch world holds an adjacency
            // graph, tables already bounded above at 16 x 1024 entries, and
            // some buffers -- kilobytes to low megabytes; the whole reference
            // *module* is 116 KB. So this is ~1000x headroom over honest use,
            // while 15 instances at the ceiling still cap a run near 2 GB.
            // Reaching it means a runaway allocation, not a design choice.
            max_memory_bytes: 128 * 1024 * 1024,
        }
    }
}

/// Per-instance store state: enforces [`WasmLimits::max_memory_bytes`] and
/// records what was actually used.
///
/// A denied growth does **not** trap on its own -- wasm's `memory.grow`
/// simply returns -1, and it is the guest allocator that panics a moment
/// later. That panic arrives here as an ordinary trap, indistinguishable
/// from a divide by zero. So the denial is remembered, and [`call_export`]
/// consults it to name the cause instead of reporting a mystery.
pub struct HostState {
    max_memory_bytes: usize,
    /// Set when a growth was refused, so the resulting trap can be named.
    memory_denied: bool,
    /// High-water mark of linear memory actually granted, in bytes.
    peak_memory_bytes: usize,
}

impl wasmtime::ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_memory_bytes {
            self.memory_denied = true;
            return Ok(false);
        }
        self.peak_memory_bytes = self.peak_memory_bytes.max(desired);
        Ok(true)
    }

    /// Tables here are wasm function tables, not the data-plane tables
    /// `init` declares -- those are capped separately and far lower. A
    /// module's function table is bounded by its own type section, so the
    /// memory ceiling is the one that matters.
    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}


#[derive(Debug)]
pub enum WasmError {
    Compile(String),
    Instantiate(String),
    MissingExport(String),
    /// The call used its whole fuel budget. Almost always an unbounded
    /// loop, and worth telling apart from a genuine trap: the fix is
    /// completely different.
    OutOfFuel,
    /// Any other trap -- a panic, a divide by zero, an out-of-bounds
    /// access. Carries wasmtime's own description.
    Trap(String),
    OutputTooLarge,
    DecodeError(String),
    InitFailed(i32),
    InputTooLarge,
    Memory(String),
    /// `init` declared more state than the limits allow. Carries a message
    /// naming what was over and by how much.
    DeclarationTooLarge(String),
    /// The program declared more TinyVM stages than a pipeline has.
    TooManyStages(usize),
    /// The program tried to grow its linear memory past the ceiling.
    /// Carries the ceiling, in bytes.
    MemoryLimit(usize),
}

impl core::fmt::Display for WasmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WasmError::Compile(s) => write!(f, "compile error: {s}"),
            WasmError::Instantiate(s) => write!(f, "instantiate error: {s}"),
            WasmError::MissingExport(s) => write!(f, "missing export: {s}"),
            WasmError::OutOfFuel => write!(f, "out of fuel"),
            WasmError::Trap(s) => write!(f, "wasm trap: {s}"),
            WasmError::OutputTooLarge => write!(f, "wasm output too large"),
            WasmError::DecodeError(s) => write!(f, "decode error: {s}"),
            WasmError::InitFailed(n) => write!(f, "init returned {n}"),
            WasmError::InputTooLarge => write!(f, "wasm input too large"),
            WasmError::Memory(s) => write!(f, "memory error: {s}"),
            WasmError::DeclarationTooLarge(s) => write!(f, "declaration too large -- {s}"),
            WasmError::TooManyStages(n) => write!(
                f,
                "the program has {n} TinyVM stages; a pipeline has at most {} -- recirculate instead",
                crate::tinyvm::MAX_STAGES
            ),
            WasmError::MemoryLimit(cap) => write!(
                f,
                "grew past the {} MiB linear-memory ceiling",
                cap / (1024 * 1024)
            ),
        }
    }
}

impl std::error::Error for WasmError {}

/// Output of loading a WASM switch program: the data-plane configuration
/// and a controller handle the simulator can install on the switch.
pub struct LoadedProgram {
    pub program: TinyProgram,
    pub state: TinyVmState,
    pub controller: WasmController,
}

/// Why a switch program stopped running, and when.
///
/// A program that fails is never called again: the data plane keeps
/// forwarding on whatever it had installed, but nothing updates it. That
/// looks exactly like a routing bug from the outside, so it has to be
/// said out loud rather than inferred from a low score.
#[derive(Debug, Clone)]
pub struct ProgramFailure {
    pub switch_id: u32,
    /// Simulation time of the call that failed. Zero if it failed before
    /// the first timed event.
    pub at_ns: u64,
    /// The export that was running: `on_punt`, `on_timer`, `on_link_event`.
    pub during: &'static str,
    /// What went wrong, in the terms the program author needs.
    pub cause: String,
}

/// `4000000` -> `4,000,000`. These numbers are read by people who are
/// deciding whether their loop is too big, so they should be legible.
fn commas(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Greedy wrap at `width`, every line prefixed with `indent`. The cause
/// text includes a description from wasmtime whose length we do not
/// control, so it cannot just be printed as one line.
fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for word in text.split_whitespace() {
        if col == 0 {
            out.push_str(indent);
            col = indent.len();
        } else if col + 1 + word.len() > width {
            out.push('\n');
            out.push_str(indent);
            col = indent.len();
        } else {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += word.len();
    }
    out
}

impl core::fmt::Display for ProgramFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "program failed: switch {} stopped at t={} ms in {}\n{}",
            self.switch_id,
            self.at_ns / 1_000_000,
            self.during,
            wrap(&self.cause, 78, "  ")
        )
    }
}

/// Per-switch WASM controller state. Implements [`SwitchController`].
pub struct WasmController {
    store: Store<HostState>,
    memory: Memory,
    on_punt: TypedFunc<(u32, u32, u32, u32), i32>,
    on_timer: TypedFunc<(u32, u32, u32, u32), i32>,
    on_link_event: TypedFunc<(u32, u32, u32, u32), i32>,
    limits: WasmLimits,
    switch_id: u32,
    /// Simulation time of the most recent event handed to this program,
    /// so a failure can say *when* rather than just *that*.
    now_ns: u64,
    pub failed: bool,
    /// Set once, the first time the program fails.
    pub failure: Option<ProgramFailure>,
}

/// Load a WASM module (bytes — `.wasm` or `.wat`) for a specific switch and
/// run its `init` to obtain the data-plane setup. Returns the simulator-side
/// pieces ready to feed into `Switch::new`.
pub fn load_program(
    module_bytes: &[u8],
    switch_id: u32,
    local_ports: Vec<u16>,
    port_capacity_bps: Vec<u64>,
    customers: Vec<wire::CustomerW>,
    limits: WasmLimits,
) -> Result<LoadedProgram, WasmError> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).map_err(|e| WasmError::Compile(e.to_string()))?;
    let module = Module::new(&engine, module_bytes)
        .map_err(|e| WasmError::Compile(e.to_string()))?;
    let mut store = Store::new(
        &engine,
        HostState {
            max_memory_bytes: limits.max_memory_bytes,
            memory_denied: false,
            peak_memory_bytes: 0,
        },
    );
    store.limiter(|s| s);
    store
        .set_fuel(limits.fuel_per_call)
        .map_err(|e| WasmError::Compile(e.to_string()))?;
    let linker: Linker<HostState> = Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| WasmError::MissingExport("memory".into()))?;

    let init: TypedFunc<(u32, u32, u32, u32), i32> = instance
        .get_typed_func(&mut store, "init")
        .map_err(|_| WasmError::MissingExport("init".into()))?;
    let on_punt: TypedFunc<(u32, u32, u32, u32), i32> = instance
        .get_typed_func(&mut store, "on_punt")
        .map_err(|_| WasmError::MissingExport("on_punt".into()))?;
    let on_timer: TypedFunc<(u32, u32, u32, u32), i32> = instance
        .get_typed_func(&mut store, "on_timer")
        .map_err(|_| WasmError::MissingExport("on_timer".into()))?;
    let on_link_event: TypedFunc<(u32, u32, u32, u32), i32> = instance
        .get_typed_func(&mut store, "on_link_event")
        .map_err(|_| WasmError::MissingExport("on_link_event".into()))?;

    // Drive `init`.
    let init_input = postcard::to_allocvec(&wire::InitInputW {
        switch_id,
        local_ports,
        port_capacity_bps,
        customers,
    })
        .map_err(|e| WasmError::DecodeError(e.to_string()))?;
    let setup_bytes = call_export(
        &mut store,
        &memory,
        &init,
        &init_input,
        &limits,
        "init",
    )?;
    let setup: wire::ProgramSetupW = postcard::from_bytes(&setup_bytes)
        .map_err(|e| WasmError::DecodeError(e.to_string()))?;

    let state = state_from_wire(&setup, &limits)?;
    if setup.program.stages.len() > crate::tinyvm::MAX_STAGES {
        return Err(WasmError::TooManyStages(setup.program.stages.len()));
    }
    let program = tiny_program_from_wire(setup.program);

    let controller = WasmController {
        store,
        memory,
        on_punt,
        on_timer,
        on_link_event,
        limits,
        switch_id,
        now_ns: 0,
        failed: false,
        failure: None,
    };
    let _ = instance;
    Ok(LoadedProgram { program, state, controller })
}

/// Generic export caller: writes input into wasm memory, calls the function,
/// reads the returned bytes from the output buffer.
fn call_export(
    store: &mut Store<HostState>,
    memory: &Memory,
    func: &TypedFunc<(u32, u32, u32, u32), i32>,
    input: &[u8],
    limits: &WasmLimits,
    _which: &str,
) -> Result<Vec<u8>, WasmError> {
    store
        .set_fuel(limits.fuel_per_call)
        .map_err(|e| WasmError::Memory(e.to_string()))?;
    if input.len() > limits.max_output_bytes as usize {
        return Err(WasmError::InputTooLarge);
    }
    let in_ptr = limits.input_buffer_offset;
    memory
        .write(&mut *store, in_ptr as usize, input)
        .map_err(|e| WasmError::Memory(e.to_string()))?;
    let out_ptr = limits.output_buffer_offset;
    let out_cap = limits.max_output_bytes;
    let written = match func.call(
        &mut *store,
        (in_ptr, input.len() as u32, out_ptr, out_cap),
    ) {
        Ok(n) => n,
        Err(e) => {
            // Fuel exhaustion arrives as an ordinary trap. Separating it
            // out is the whole point: "your loop never ends" and "you
            // divided by zero" are different bugs and deserve different
            // words.
            // A refused memory growth surfaces here as the guest
            // allocator's panic, so check that before reading the trap.
            if store.data().memory_denied {
                return Err(WasmError::MemoryLimit(limits.max_memory_bytes));
            }
            return Err(match e.downcast_ref::<Trap>() {
                Some(Trap::OutOfFuel) => WasmError::OutOfFuel,
                Some(t) => WasmError::Trap(t.to_string()),
                None => WasmError::Trap(e.to_string()),
            });
        }
    };
    if written < 0 {
        return Err(WasmError::InitFailed(written));
    }
    let written = written as u32;
    if written > out_cap {
        return Err(WasmError::OutputTooLarge);
    }
    let mut buf = vec![0u8; written as usize];
    memory
        .read(&*store, out_ptr as usize, &mut buf)
        .map_err(|e| WasmError::Memory(e.to_string()))?;
    Ok(buf)
}

impl WasmController {
    /// Turn a host-side error into something the program author can act
    /// on. The sandbox knows exactly what went wrong; the only reason a
    /// student would ever be confused is if we decline to say.
    fn explain(&self, e: &WasmError) -> String {
        let fuel = self.limits.fuel_per_call;
        let cap = self.limits.max_output_bytes;
        match e {
            WasmError::OutOfFuel => format!(
                "ran out of fuel -- this one call used more than {} instructions. \
                 That is the per-call ceiling, not a budget for the whole run, so \
                 the cause is almost always a loop that never ends.",
                commas(fuel)
            ),
            WasmError::Trap(what) => format!(
                "trapped: {}. Something in the handler panicked or hit an invalid \
                 operation -- an unwrap on None, an index past the end of a slice, \
                 a divide by zero.",
                what.strip_prefix("wasm trap: ").unwrap_or(what)
            ),
            WasmError::OutputTooLarge => format!(
                "returned more than {} bytes of actions. Split the work across \
                 several calls.",
                commas(cap as u64)
            ),
            WasmError::InputTooLarge => format!(
                "the event did not fit in the {}-byte input buffer.",
                commas(cap as u64)
            ),
            WasmError::InitFailed(n) => format!(
                "returned {n}. A negative return value means the handler is \
                 reporting its own failure."
            ),
            WasmError::DecodeError(m) => format!(
                "returned bytes that are not a valid action list ({m})."
            ),
            // Worth more words than the others: the A1 kit shipped before
            // this ceiling existed, so this is the one failure a student
            // cannot reproduce at home.
            WasmError::MemoryLimit(bytes) => format!(
                "grew its linear memory past the {} MiB ceiling. That ceiling \
                 is only here to stop one runaway allocation taking the grader \
                 down -- it is hundreds of times more memory than a working \
                 solution needs, so the cause is almost always state that grows \
                 on every event and is never bounded: a record of every packet \
                 seen, or a map keyed by something that keeps changing.",
                bytes / (1024 * 1024)
            ),
            other => format!("{other}"),
        }
    }

    /// Record the failure, say so, and stop calling the program.
    fn fail(&mut self, during: &'static str, e: &WasmError) {
        self.failed = true;
        let f = ProgramFailure {
            switch_id: self.switch_id,
            at_ns: self.now_ns,
            during,
            cause: self.explain(e),
        };
        // Said once per switch -- `failed` short-circuits every later call,
        // so this cannot become a flood.
        eprintln!("{f}");
        self.failure = Some(f);
    }

    fn invoke(&mut self, which: WhichFn, input: Vec<u8>) -> Vec<ControllerAction> {
        if self.failed {
            return Vec::new();
        }
        let (func, name) = match which {
            WhichFn::Punt => (&self.on_punt, "on_punt"),
            WhichFn::Timer => (&self.on_timer, "on_timer"),
            WhichFn::Link => (&self.on_link_event, "on_link_event"),
        };
        let bytes = match call_export(
            &mut self.store,
            &self.memory,
            func,
            &input,
            &self.limits,
            name,
        ) {
            Ok(b) => b,
            Err(e) => {
                self.fail(name, &e);
                return Vec::new();
            }
        };
        let actions: Vec<wire::ActionW> = match postcard::from_bytes(&bytes) {
            Ok(v) => v,
            Err(e) => {
                self.fail(name, &WasmError::DecodeError(e.to_string()));
                return Vec::new();
            }
        };
        actions
            .into_iter()
            .filter_map(action_from_wire)
            .collect()
    }
}

enum WhichFn {
    Punt,
    Timer,
    Link,
}

impl SwitchController for WasmController {
    fn on_punt(&mut self, event: PuntEvent) -> Vec<ControllerAction> {
        let wire = wire::PuntEventW {
            now_ns: event.now.as_nanos() as u64,
            switch_id: event.switch.raw(),
            ingress_port: event.ingress_port.raw(),
            reason: punt_reason_to_wire(event.reason),
            packet_size: event.packet.size_bytes,
            ip_src: event.packet.ip_src.0,
            ip_dst: event.packet.ip_dst.0,
            ip_proto: event.packet.ip_proto,
            ip_ttl: event.packet.ip_ttl,
            payload: event.packet.payload.clone(),
        };
        let bytes = postcard::to_allocvec(&wire).expect("postcard encode");
        self.now_ns = event.now.as_nanos() as u64;
        self.invoke(WhichFn::Punt, bytes)
    }

    fn on_timer(&mut self, now: SimTime) -> Vec<ControllerAction> {
        self.on_timer_with_drops(now, Vec::new())
    }

    fn on_timer_with_drops(&mut self, now: SimTime, drops: Vec<PortDrops>) -> Vec<ControllerAction> {
        let bytes = postcard::to_allocvec(&wire::TimerEventW {
            now_ns: now.as_nanos() as u64,
            port_drops: drops
                .into_iter()
                .map(|d| wire::PortDropsW {
                    port: d.port.raw(),
                    dropped: d.dropped.min(u32::MAX as u64) as u32,
                    policy: d.policy.min(u32::MAX as u64) as u32,
                    queues: d.queues.iter().map(|q| (*q).min(u32::MAX as u64) as u32).collect(),
                })
                .collect(),
        })
        .expect("postcard encode");
        self.now_ns = now.as_nanos() as u64;
        self.invoke(WhichFn::Timer, bytes)
    }

    fn on_link_event(&mut self, event: LinkEvent) -> Vec<ControllerAction> {
        let w = match event {
            LinkEvent::Up { port } => wire::LinkEventW { kind: 0, port: port.raw() },
            LinkEvent::Down { port } => wire::LinkEventW { kind: 1, port: port.raw() },
        };
        let bytes = postcard::to_allocvec(&w).expect("postcard encode");
        self.invoke(WhichFn::Link, bytes)
    }

    fn failure(&self) -> Option<String> {
        self.failure.as_ref().map(|f| f.to_string())
    }
}

// ---------- conversions ----------

fn punt_reason_to_wire(r: PuntReason) -> wire::PuntReasonW {
    match r {
        PuntReason::NoRoute => wire::PuntReasonW::NoRoute,
        PuntReason::TtlExpired => wire::PuntReasonW::TtlExpired,
        PuntReason::Custom(c) => wire::PuntReasonW::Custom(c),
    }
}

fn punt_reason_from_wire(r: wire::PuntReasonW) -> PuntReason {
    match r {
        wire::PuntReasonW::NoRoute => PuntReason::NoRoute,
        wire::PuntReasonW::TtlExpired => PuntReason::TtlExpired,
        wire::PuntReasonW::Custom(c) => PuntReason::Custom(c),
    }
}

fn match_kind_from_wire(k: wire::MatchKindW) -> MatchKind {
    match k {
        wire::MatchKindW::Exact => MatchKind::Exact,
        wire::MatchKindW::Lpm => MatchKind::Lpm,
    }
}

fn field_from_wire(f: wire::PacketFieldW) -> PacketField {
    match f {
        wire::PacketFieldW::IpSrc => PacketField::IpSrc,
        wire::PacketFieldW::IpDst => PacketField::IpDst,
        wire::PacketFieldW::IpProto => PacketField::IpProto,
        wire::PacketFieldW::IpTtl => PacketField::IpTtl,
        wire::PacketFieldW::IpDscp => PacketField::IpDscp,
        wire::PacketFieldW::SrcPort => PacketField::SrcPort,
        wire::PacketFieldW::DstPort => PacketField::DstPort,
        wire::PacketFieldW::AppId => PacketField::AppId,
        wire::PacketFieldW::FlowId => PacketField::FlowId,
        wire::PacketFieldW::Custom(i) => PacketField::Custom(i),
        wire::PacketFieldW::Size => PacketField::Size,
        wire::PacketFieldW::Kind => PacketField::Kind,
        wire::PacketFieldW::LabelTop => PacketField::LabelTop,
        wire::PacketFieldW::LabelDepth => PacketField::LabelDepth,
    }
}

fn table_action_from_wire(a: wire::TableActionW) -> TableAction {
    match a {
        wire::TableActionW::SetMeta { key, value } => TableAction::SetMeta {
            key: MetaKey::new(key.0),
            value,
        },
        wire::TableActionW::SetEgress { port } => TableAction::SetEgress {
            port: PortId::new(port.0),
        },
        wire::TableActionW::SetQueue { queue } => TableAction::SetQueue {
            queue: QueueId::new(queue.0),
        },
        wire::TableActionW::Drop => TableAction::Drop,
        wire::TableActionW::Punt { reason } => TableAction::Punt {
            reason: punt_reason_from_wire(reason),
        },
    }
}

fn table_entry_from_wire(e: wire::TableEntryW) -> TableEntry {
    TableEntry {
        id: EntryId::new(e.id.0),
        key: e.key,
        prefix_len: e.prefix_len,
        priority: e.priority,
        action: table_action_from_wire(e.action),
    }
}

fn instr_from_wire(i: wire::InstrW) -> Instr {
    use crate::types::InstrIndex;
    match i {
        wire::InstrW::LoadField { dst, field } => Instr::LoadField {
            dst: Reg::new(dst.0),
            field: field_from_wire(field),
        },
        wire::InstrW::StoreField { field, src } => Instr::StoreField {
            field: field_from_wire(field),
            src: Reg::new(src.0),
        },
        wire::InstrW::LoadMeta { dst, key } => Instr::LoadMeta {
            dst: Reg::new(dst.0),
            key: MetaKey::new(key.0),
        },
        wire::InstrW::StoreMeta { key, src } => Instr::StoreMeta {
            key: MetaKey::new(key.0),
            src: Reg::new(src.0),
        },
        wire::InstrW::Const { dst, value } => Instr::Const {
            dst: Reg::new(dst.0),
            value,
        },
        wire::InstrW::Add { dst, a, b } => Instr::Add {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::Sub { dst, a, b } => Instr::Sub {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::And { dst, a, b } => Instr::And {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::Or { dst, a, b } => Instr::Or {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::Xor { dst, a, b } => Instr::Xor {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::Eq { dst, a, b } => Instr::Eq {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::Lt { dst, a, b } => Instr::Lt {
            dst: Reg::new(dst.0), a: Reg::new(a.0), b: Reg::new(b.0),
        },
        wire::InstrW::TableLookup { table_id, key_reg, result_meta } => Instr::TableLookup {
            table_id: TableId::new(table_id.0),
            key_reg: Reg::new(key_reg.0),
            result_meta: MetaKey::new(result_meta.0),
        },
        wire::InstrW::RegisterRead { array, index, dst } => Instr::RegisterRead {
            array: RegisterArrayId::new(array.0),
            index: Reg::new(index.0),
            dst: Reg::new(dst.0),
        },
        wire::InstrW::RegisterWrite { array, index, src } => Instr::RegisterWrite {
            array: RegisterArrayId::new(array.0),
            index: Reg::new(index.0),
            src: Reg::new(src.0),
        },
        wire::InstrW::CounterAdd { counter, index, value } => Instr::CounterAdd {
            counter: CounterArrayId::new(counter.0),
            index: Reg::new(index.0),
            value: Reg::new(value.0),
        },
        wire::InstrW::BranchIf { cond, target } => Instr::BranchIf {
            cond: Reg::new(cond.0),
            target: InstrIndex::new(target),
        },
        wire::InstrW::Drop => Instr::Drop,
        wire::InstrW::Punt { reason } => Instr::Punt {
            reason: punt_reason_from_wire(reason),
        },
        wire::InstrW::SetEgress { port } => Instr::SetEgress {
            port: PortId::new(port.0),
        },
        wire::InstrW::SetQueue { queue } => Instr::SetQueue {
            queue: QueueId::new(queue.0),
        },
        wire::InstrW::Recirculate => Instr::Recirculate,
        wire::InstrW::Noop => Instr::Noop,
        wire::InstrW::PushLabel { label } => Instr::PushLabel { label },
        wire::InstrW::PopLabel => Instr::PopLabel,
        wire::InstrW::SwapLabel { label } => Instr::SwapLabel { label },
        wire::InstrW::MarkTrace => Instr::MarkTrace,
    }
}

pub(crate) fn tiny_program_from_wire(p: wire::TinyProgramW) -> TinyProgram {
    TinyProgram {
        stages: p
            .stages
            .into_iter()
            .map(|s| StageProgram {
                instrs: s.instrs.into_iter().map(instr_from_wire).collect(),
                max_alu_ops: s.max_alu_ops,
                max_memory_accesses: s.max_memory_accesses,
            })
            .collect(),
    }
}

fn state_from_wire(
    setup: &wire::ProgramSetupW,
    limits: &WasmLimits,
) -> Result<TinyVmState, WasmError> {
    let too_big = |what: &str, got: u64, cap: u64| {
        WasmError::DeclarationTooLarge(format!("{what}: declared {got}, limit is {cap}"))
    };

    if setup.tables.len() > limits.max_tables {
        return Err(too_big("tables", setup.tables.len() as u64, limits.max_tables as u64));
    }
    if setup.registers.len() > limits.max_register_arrays {
        return Err(too_big(
            "register arrays",
            setup.registers.len() as u64,
            limits.max_register_arrays as u64,
        ));
    }
    if setup.counters.len() > limits.max_counter_arrays {
        return Err(too_big(
            "counter arrays",
            setup.counters.len() as u64,
            limits.max_counter_arrays as u64,
        ));
    }

    let mut state = TinyVmState::default();
    for t in &setup.tables {
        if t.max_entries > limits.max_table_entries {
            return Err(too_big(
                &format!("table {} entries", t.id.0),
                t.max_entries as u64,
                limits.max_table_entries as u64,
            ));
        }
        let mut table = MatchActionTable::new(
            TableId::new(t.id.0),
            match_kind_from_wire(t.kind),
            t.max_entries as usize,
        );
        for e in &t.initial_entries {
            let _ = table.install(table_entry_from_wire(e.clone()));
        }
        state.tables.push(table);
    }
    for r in &setup.registers {
        if r.size > limits.max_register_slots {
            return Err(too_big(
                &format!("register array {} slots", r.id.0),
                r.size as u64,
                limits.max_register_slots as u64,
            ));
        }
        state
            .registers
            .push(RegisterArray::new(RegisterArrayId::new(r.id.0), r.size as usize));
    }
    for c in &setup.counters {
        if c.size > limits.max_counter_slots {
            return Err(too_big(
                &format!("counter array {} slots", c.id.0),
                c.size as u64,
                limits.max_counter_slots as u64,
            ));
        }
        state
            .counters
            .push(CounterArray::new(CounterArrayId::new(c.id.0), c.size as usize));
    }
    Ok(state)
}

fn action_from_wire(a: wire::ActionW) -> Option<ControllerAction> {
    match a {
        wire::ActionW::InstallTableEntry { table_id, entry } => {
            Some(ControllerAction::InstallTableEntry {
                table_id: TableId::new(table_id.0),
                entry: table_entry_from_wire(entry),
            })
        }
        wire::ActionW::DeleteTableEntry { table_id, entry_id } => {
            Some(ControllerAction::DeleteTableEntry {
                table_id: TableId::new(table_id.0),
                entry_id: EntryId::new(entry_id.0),
            })
        }
        wire::ActionW::SetQueueConfig {
            port,
            discipline,
            queues,
        } => Some(ControllerAction::SetQueueConfig {
            port: PortId::new(port.0),
            config: QueueConfig {
                discipline: Discipline::from_u8(discipline),
                queues: queues
                    .iter()
                    .map(|q| QueueSpec {
                        max_packets: q.max_packets,
                        weight: q.weight.max(1),
                        drop_threshold_packets: (q.drop_threshold_packets > 0)
                            .then_some(q.drop_threshold_packets),
                    })
                    .collect(),
            },
        }),
        wire::ActionW::InjectPacket {
            port,
            ip_src,
            ip_dst,
            ip_proto,
            ip_ttl,
            kind,
            size_bytes,
            payload,
        } => Some(inject_from_wire(
            port, 0, ip_src, ip_dst, ip_proto, ip_ttl, kind, size_bytes, payload,
        )),
        wire::ActionW::InjectPacketQueued {
            port,
            queue,
            ip_src,
            ip_dst,
            ip_proto,
            ip_ttl,
            kind,
            size_bytes,
            payload,
        } => Some(inject_from_wire(
            port, queue.0, ip_src, ip_dst, ip_proto, ip_ttl, kind, size_bytes, payload,
        )),
        wire::ActionW::ScheduleTimer { delay_ns } => Some(ControllerAction::ScheduleTimer {
            delay: core::time::Duration::from_nanos(delay_ns),
        }),
        wire::ActionW::DeclareLink { port, up } => Some(ControllerAction::DeclareLink {
            port: PortId::new(port.0),
            up,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn inject_from_wire(
    port: wire::PortIdW,
    queue: u16,
    ip_src: u32,
    ip_dst: u32,
    ip_proto: u8,
    ip_ttl: u8,
    kind: u8,
    size_bytes: u64,
    payload: Vec<u8>,
) -> ControllerAction {
    let mut p = crate::packet::Packet::new(
        crate::types::PacketId::new(0),
        core::time::Duration::ZERO,
        size_bytes,
    );
    p.ip_src = crate::packet::IpAddr(ip_src);
    p.ip_dst = crate::packet::IpAddr(ip_dst);
    p.ip_proto = ip_proto;
    p.ip_ttl = ip_ttl;
    p.kind = crate::packet::PacketKind::from_u8(kind);
    p.payload = payload;
    ControllerAction::InjectPacket {
        packet: p,
        port: PortId::new(port.0),
        queue: crate::types::QueueId::new(queue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a WAT module that:
    ///  - returns `setup_bytes` from `init` (a fixed postcard `ProgramSetupW`)
    ///  - returns `punt_bytes`  from `on_punt` (a fixed postcard `Vec<ActionW>`)
    ///  - returns 0 from `on_timer` and `on_link_event`
    fn build_wat(setup_bytes: &[u8], punt_bytes: &[u8]) -> String {
        let setup_hex: String = setup_bytes.iter().map(|b| format!("\\{:02x}", b)).collect();
        let punt_hex: String = punt_bytes.iter().map(|b| format!("\\{:02x}", b)).collect();
        let setup_len = setup_bytes.len();
        let punt_len = punt_bytes.len();
        // Setup data at 16384, punt data at 24576.
        format!(
            r#"
            (module
              (memory (export "memory") 1)
              (data (i32.const 16384) "{setup_hex}")
              (data (i32.const 24576) "{punt_hex}")
              (func (export "init") (param i32 i32 i32 i32) (result i32)
                (memory.copy (local.get 2) (i32.const 16384) (i32.const {setup_len}))
                (i32.const {setup_len}))
              (func (export "on_punt") (param i32 i32 i32 i32) (result i32)
                (memory.copy (local.get 2) (i32.const 24576) (i32.const {punt_len}))
                (i32.const {punt_len}))
              (func (export "on_timer") (param i32 i32 i32 i32) (result i32)
                (i32.const 0))
              (func (export "on_link_event") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))
            "#
        )
    }

    fn sample_setup() -> wire::ProgramSetupW {
        wire::ProgramSetupW {
            program: wire::TinyProgramW {
                stages: vec![wire::StageProgramW {
                    instrs: vec![
                        wire::InstrW::LoadField {
                            dst: wire::RegW(0),
                            field: wire::PacketFieldW::IpDst,
                        },
                        wire::InstrW::TableLookup {
                            table_id: wire::TableIdW(1),
                            key_reg: wire::RegW(0),
                            result_meta: wire::MetaKeyW(0),
                        },
                    ],
                    max_alu_ops: 4,
                    max_memory_accesses: 1,
                }],
            },
            tables: vec![wire::TableDeclW {
                id: wire::TableIdW(1),
                kind: wire::MatchKindW::Exact,
                max_entries: 16,
                initial_entries: vec![],
            }],
            registers: vec![],
            counters: vec![],
        }
    }

    #[test]
    fn oversized_declarations_are_rejected() {
        let limits = WasmLimits::default();

        // A register array sized from the wire is a `vec![0; size]` on the
        // host. Unchecked, one line in a switch program allocates 32 GB.
        let mut setup = sample_setup();
        setup.registers = vec![wire::RegisterDeclW {
            id: wire::RegisterArrayIdW(0),
            size: 4_000_000_000,
        }];
        let err = state_from_wire(&setup, &limits).expect_err("must reject");
        assert!(
            matches!(err, WasmError::DeclarationTooLarge(ref m) if m.contains("register array")),
            "{err}"
        );

        let mut setup = sample_setup();
        setup.tables[0].max_entries = limits.max_table_entries + 1;
        let err = state_from_wire(&setup, &limits).expect_err("must reject");
        assert!(matches!(err, WasmError::DeclarationTooLarge(_)), "{err}");

        let mut setup = sample_setup();
        setup.counters = vec![wire::CounterDeclW {
            id: wire::CounterArrayIdW(0),
            size: limits.max_counter_slots + 1,
        }];
        assert!(state_from_wire(&setup, &limits).is_err());

        // And a declaration inside the limits still builds.
        assert!(state_from_wire(&sample_setup(), &limits).is_ok());
    }

    fn sample_punt_actions() -> Vec<wire::ActionW> {
        vec![wire::ActionW::InstallTableEntry {
            table_id: wire::TableIdW(1),
            entry: wire::TableEntryW {
                id: wire::EntryIdW(42),
                key: 0x0a000002,
                prefix_len: 32,
                priority: 0,
                action: wire::TableActionW::SetEgress { port: wire::PortIdW(1) },
            },
        }]
    }

    #[test]
    fn a_program_with_too_many_stages_is_refused_at_load() {
        let mut setup = sample_setup();
        let stage = setup.program.stages[0].clone();
        setup.program.stages = vec![stage; crate::tinyvm::MAX_STAGES + 1];
        let setup_bytes = postcard::to_allocvec(&setup).unwrap();
        let punt_bytes = postcard::to_allocvec(&sample_punt_actions()).unwrap();
        let wat = build_wat(&setup_bytes, &punt_bytes);
        let err = load_program(wat.as_bytes(), 7, vec![], vec![], vec![], WasmLimits::default())
            .err()
            .expect("nine stages must be refused");
        assert!(matches!(err, WasmError::TooManyStages(9)), "{err}");
        assert!(err.to_string().contains("recirculate"));

        // Exactly the limit is fine.
        let mut setup = sample_setup();
        let stage = setup.program.stages[0].clone();
        setup.program.stages = vec![stage; crate::tinyvm::MAX_STAGES];
        let setup_bytes = postcard::to_allocvec(&setup).unwrap();
        let wat = build_wat(&setup_bytes, &punt_bytes);
        assert!(load_program(wat.as_bytes(), 7, vec![], vec![], vec![], WasmLimits::default()).is_ok());
    }

    #[test]
    fn load_program_returns_setup_and_controller() {
        let setup_bytes = postcard::to_allocvec(&sample_setup()).unwrap();
        let punt_bytes = postcard::to_allocvec(&sample_punt_actions()).unwrap();
        let wat = build_wat(&setup_bytes, &punt_bytes);

        let mut loaded = load_program(wat.as_bytes(), 7, vec![], vec![], vec![], WasmLimits::default()).unwrap();
        assert_eq!(loaded.program.stages.len(), 1);
        assert_eq!(loaded.state.tables.len(), 1);

        // Drive a punt and see the action.
        let event = PuntEvent {
            now: std::time::Duration::from_micros(0),
            switch: crate::types::SwitchId::new(7),
            ingress_port: PortId::new(0),
            reason: PuntReason::NoRoute,
            packet: crate::packet::Packet::new(
                crate::types::PacketId::new(0),
                std::time::Duration::ZERO,
                100,
            ),
        };
        let actions = loaded.controller.on_punt(event);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ControllerAction::InstallTableEntry { table_id, entry } => {
                assert_eq!(table_id.raw(), 1);
                assert_eq!(entry.id.raw(), 42);
            }
            _ => panic!("unexpected action"),
        }
    }

    #[test]
    fn trap_marks_controller_failed() {
        let setup_bytes = postcard::to_allocvec(&sample_setup()).unwrap();
        let setup_hex: String = setup_bytes.iter().map(|b| format!("\\{:02x}", b)).collect();
        let setup_len = setup_bytes.len();
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1)
              (data (i32.const 16384) "{setup_hex}")
              (func (export "init") (param i32 i32 i32 i32) (result i32)
                (memory.copy (local.get 2) (i32.const 16384) (i32.const {setup_len}))
                (i32.const {setup_len}))
              (func (export "on_punt") (param i32 i32 i32 i32) (result i32)
                (unreachable))
              (func (export "on_timer") (param i32 i32 i32 i32) (result i32)
                (i32.const 0))
              (func (export "on_link_event") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))
            "#
        );
        let mut loaded = load_program(wat.as_bytes(), 0, vec![], vec![], vec![], WasmLimits::default()).unwrap();
        let event = PuntEvent {
            now: std::time::Duration::ZERO,
            switch: crate::types::SwitchId::new(0),
            ingress_port: PortId::new(0),
            reason: PuntReason::NoRoute,
            packet: crate::packet::Packet::new(
                crate::types::PacketId::new(0),
                std::time::Duration::ZERO,
                100,
            ),
        };
        let actions = loaded.controller.on_punt(event);
        assert!(actions.is_empty());
        assert!(loaded.controller.failed);
        let f = loaded.controller.failure.as_ref().expect("failure recorded");
        assert_eq!(f.during, "on_punt");
        assert!(
            f.cause.contains("trapped"),
            "a genuine trap must not be reported as fuel exhaustion: {}",
            f.cause
        );
    }

    #[test]
    fn growing_past_the_memory_ceiling_is_named_not_a_mystery_trap() {
        let setup_bytes = postcard::to_allocvec(&sample_setup()).unwrap();
        let setup_hex: String = setup_bytes.iter().map(|b| format!("\\{:02x}", b)).collect();
        let setup_len = setup_bytes.len();
        // Grow 16 pages at a time until `memory.grow` answers -1, then do
        // what a guest allocator does when it cannot get memory: panic.
        // That panic is an ordinary trap, which is exactly why the denial
        // has to be remembered rather than read off the trap.
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1)
              (data (i32.const 16384) "{setup_hex}")
              (func (export "init") (param i32 i32 i32 i32) (result i32)
                (memory.copy (local.get 2) (i32.const 16384) (i32.const {setup_len}))
                (i32.const {setup_len}))
              (func (export "on_punt") (param i32 i32 i32 i32) (result i32)
                (loop $g
                  (br_if $g (i32.ne (memory.grow (i32.const 16)) (i32.const -1))))
                (unreachable))
              (func (export "on_timer") (param i32 i32 i32 i32) (result i32)
                (i32.const 0))
              (func (export "on_link_event") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))
            "#
        );
        let limits = WasmLimits {
            max_memory_bytes: 4 * 1024 * 1024,
            ..WasmLimits::default()
        };
        let mut loaded = load_program(wat.as_bytes(), 0, vec![], vec![], vec![], limits).unwrap();
        let event = PuntEvent {
            now: std::time::Duration::ZERO,
            switch: crate::types::SwitchId::new(0),
            ingress_port: PortId::new(0),
            reason: PuntReason::NoRoute,
            packet: crate::packet::Packet::new(
                crate::types::PacketId::new(0),
                std::time::Duration::ZERO,
                100,
            ),
        };
        let actions = loaded.controller.on_punt(event);
        assert!(actions.is_empty());
        assert!(loaded.controller.failed, "the run must stop, not limp on");
        let f = loaded.controller.failure.as_ref().expect("failure recorded");
        assert!(
            f.cause.contains("linear memory") && f.cause.contains("4 MiB"),
            "a denied growth must be named and carry the ceiling, not read as a \
             bare trap: {}",
            f.cause
        );
        assert!(
            !f.cause.contains("divide by zero"),
            "must not be mistaken for a panic: {}",
            f.cause
        );
    }

    #[test]
    fn fuel_exhaustion_marks_controller_failed() {
        let setup_bytes = postcard::to_allocvec(&sample_setup()).unwrap();
        let setup_hex: String = setup_bytes.iter().map(|b| format!("\\{:02x}", b)).collect();
        let setup_len = setup_bytes.len();
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1)
              (data (i32.const 16384) "{setup_hex}")
              (func (export "init") (param i32 i32 i32 i32) (result i32)
                (memory.copy (local.get 2) (i32.const 16384) (i32.const {setup_len}))
                (i32.const {setup_len}))
              (func (export "on_punt") (param i32 i32 i32 i32) (result i32)
                (loop (br 0))
                (i32.const 0))
              (func (export "on_timer") (param i32 i32 i32 i32) (result i32)
                (i32.const 0))
              (func (export "on_link_event") (param i32 i32 i32 i32) (result i32)
                (i32.const 0)))
            "#
        );
        let limits = WasmLimits {
            fuel_per_call: 1000,
            ..WasmLimits::default()
        };
        let mut loaded = load_program(wat.as_bytes(), 0, vec![], vec![], vec![], limits).unwrap();
        let event = PuntEvent {
            now: std::time::Duration::ZERO,
            switch: crate::types::SwitchId::new(0),
            ingress_port: PortId::new(0),
            reason: PuntReason::NoRoute,
            packet: crate::packet::Packet::new(
                crate::types::PacketId::new(0),
                std::time::Duration::ZERO,
                100,
            ),
        };
        let actions = loaded.controller.on_punt(event);
        assert!(actions.is_empty());
        assert!(loaded.controller.failed);
        let f = loaded.controller.failure.as_ref().expect("failure recorded");
        assert_eq!(f.during, "on_punt");
        assert!(
            f.cause.contains("ran out of fuel"),
            "an endless loop must be reported as fuel exhaustion, not a bare trap: {}",
            f.cause
        );
        // The two failures a student is most likely to hit must not read
        // the same. This is the whole point of the split.
        assert!(!f.cause.contains("trapped"), "fuel exhaustion read as a trap");
    }
}
