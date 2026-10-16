//! Browser-side replay of a `.simlog` file.
//!
//! Single `LogPlayer` struct exposed to JS via `wasm-bindgen`. Owns:
//!
//! * the parsed list of `LogFrame`s,
//! * a static topology snapshot (built once from the head of the log),
//! * a per-switch reconstructable state (`SwitchState`) folded forward
//!   from the most recent `ProgramInstalled` plus subsequent table
//!   deltas as the cursor moves,
//! * an in-flight packet table — packets that have left a switch and
//!   not yet arrived at the next hop.
//!
//! The JS layer asks for:
//!   - `topology()` once, after `new`.
//!   - `seek(t)` / `frame(now, dt)` per animation frame.
//!   - `inspect_switch(id)` when the user clicks while paused.

use std::collections::HashMap;
use wasm_bindgen::prelude::*;

use competitive_net_sim::sim_log::{self, LogEvent, LogFrame, NodeRefW};

/// `serde_wasm_bindgen` defaults to emitting JS `Map` objects for
/// structs, which means JS code can't access fields via dot notation.
/// Also force u64/i64 to JS BigInts (the default coerces them to
/// JS Numbers, which then fail to round-trip into wasm-bindgen u64
/// arguments — wasm-bindgen rejects non-BigInt with "Can't convert N
/// to BigInt").
fn to_js<T: serde::Serialize>(v: &T) -> JsValue {
    let s = serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_large_number_types_as_bigints(true);
    v.serialize(&s).unwrap()
}

#[wasm_bindgen(start)]
pub fn _start() {
    // Better panic messages in the browser console.
    std::panic::set_hook(Box::new(console_error_hook));
}

fn console_error_hook(info: &std::panic::PanicHookInfo<'_>) {
    let msg = format!("{info}");
    web_log(&msg);
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(s: &str);
}

fn web_log(s: &str) {
    console_error(s);
}

// ---------------------------------------------------------------- types

#[derive(serde::Serialize)]
struct TopologyOut {
    apps: Vec<AppNode>,
    switches: Vec<SwitchNode>,
    links: Vec<LinkEdge>,
    duration_ns: u64,
}

#[derive(serde::Serialize, Clone)]
struct AppNode {
    id: u32,
    ip: u32,
    dst_ip: u32,
}

#[derive(serde::Serialize, Clone)]
struct SwitchNode {
    id: u32,
    owner: Option<u32>,
    /// Set when this switch is the border for an AS in the BGP registry.
    bgp_speaker_ip: Option<u32>,
}

#[derive(serde::Serialize, Clone)]
struct LinkEdge {
    id: u32,
    a_kind: u8, // 0 = app, 1 = switch
    a_id: u32,
    a_port: u16,
    b_kind: u8,
    b_id: u32,
    b_port: u16,
    bandwidth_bps: u64,
    latency_ns: u64,
}

#[derive(serde::Serialize)]
struct FrameOut {
    /// Per-link summary stats, used when packet density is high.
    summaries: Vec<LinkSummary>,
    /// Individual moving packets, otherwise.
    packets: Vec<MovingPacket>,
    /// Static state at this instant: which links/switches are live.
    link_failed: Vec<u32>,
    switch_failed: Vec<u32>,
    cpu_failed: Vec<u32>,
}

#[derive(serde::Serialize)]
struct LinkSummary {
    link: u32,
    packets_per_sec: f64,
    bytes_per_sec: f64,
}

#[derive(serde::Serialize)]
struct MovingPacket {
    link: u32,
    /// Direction: did this packet enter from `a` (0) or from `b` (1)?
    forward: bool,
    /// Position along the link in [0.0, 1.0].
    progress: f64,
    packet_id: u64,
    size_bytes: u64,
    kind: u8,
}

#[derive(serde::Serialize)]
struct PacketInspectOut {
    packet_id: u64,
    kind: u8,
    size_bytes: u64,
    ip_src: u32,
    ip_dst: u32,
    ip_proto: u8,
    ip_ttl: u8,
    label_top: u32,
    label_depth: u32,
    /// Switches the packet has visited so far, oldest first.
    history: Vec<u32>,
}

#[derive(serde::Serialize)]
struct InspectOut {
    switch: u32,
    bgp_speaker_ip: Option<u32>,
    ports: Vec<PortOut>,
    program_text: String,
    tables: Vec<TableOut>,
    registers: Vec<RegOut>,
    counters: Vec<CounterOut>,
}

#[derive(serde::Serialize)]
struct PortOut {
    port: u16,
    link: u32,
    peer_kind: u8,    // 0 = app, 1 = switch
    peer_id: u32,
    peer_port: u16,
    peer_ip: Option<u32>,   // app's ip, or peer-switch's BGP speaker IP
}

#[derive(serde::Serialize)]
struct TableOut {
    table_id: u32,
    kind: String, // "Exact" | "Lpm"
    max_entries: u32,
    entries: Vec<TableEntryOut>,
}

#[derive(serde::Serialize)]
struct TableEntryOut {
    id: u64,
    key: u64,
    prefix_len: u8,
    priority: i32,
    action: String,
}

#[derive(serde::Serialize)]
struct RegOut {
    array_id: u32,
    size: u32,
}

#[derive(serde::Serialize)]
struct CounterOut {
    array_id: u32,
    size: u32,
}

// ---- Internal state ----

#[derive(Clone, Default)]
struct SwitchState {
    program: Option<switch_program_types::TinyProgramW>,
    tables: Vec<sim_log::TableSnapshotW>,
    registers: Vec<sim_log::RegSnapshotW>,
    counters: Vec<sim_log::CounterSnapshotW>,
    failed: bool,
    cpu_failed: bool,
}

#[derive(Clone, Copy)]
struct InFlight {
    link: u32,
    forward: bool,
    start_ns: u64,
    arrive_ns: u64,
    packet_id: u64,
    size_bytes: u64,
    kind: u8,
}

#[wasm_bindgen]
pub struct LogPlayer {
    frames: Vec<LogFrame>,
    apps: Vec<AppNode>,
    switches: Vec<SwitchNode>,
    links: Vec<LinkEdge>,
    /// Map from switch_id -> reconstructed state at `cursor`.
    switch_states: HashMap<u32, SwitchState>,
    /// Map from link_id -> failed flag at `cursor`.
    link_failed: HashMap<u32, bool>,
    /// Cursor: the index of the next frame to apply when stepping forward.
    cursor: usize,
    cursor_ns: u64,
    duration_ns: u64,
}

#[wasm_bindgen]
impl LogPlayer {
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8]) -> Result<LogPlayer, JsValue> {
        let frames = sim_log::writer::decode_all(bytes)
            .map_err(|e| JsValue::from_str(&format!("decode: {:?}", e)))?;
        let mut apps = Vec::new();
        let mut switches = Vec::new();
        let mut links = Vec::new();
        let mut duration_ns = 0u64;
        for f in &frames {
            duration_ns = duration_ns.max(f.at_ns);
            match &f.event {
                LogEvent::AppAdded { id, ip, dst_ip } => {
                    apps.push(AppNode { id: *id, ip: *ip, dst_ip: *dst_ip });
                }
                LogEvent::SwitchAdded { id, owner, .. } => {
                    switches.push(SwitchNode {
                        id: *id,
                        owner: *owner,
                        bgp_speaker_ip: None,
                    });
                }
                LogEvent::BgpConfigured { ases } => {
                    for info in ases {
                        if let Some(s) = switches.iter_mut().find(|s| s.id == info.border_switch) {
                            s.bgp_speaker_ip = Some(info.bgp_speaker_ip);
                        }
                    }
                }
                LogEvent::LinkAdded { id, a, a_port, b, b_port, config } => {
                    let (a_kind, a_id) = node_ref_to_kind_id(a);
                    let (b_kind, b_id) = node_ref_to_kind_id(b);
                    links.push(LinkEdge {
                        id: *id,
                        a_kind, a_id, a_port: *a_port,
                        b_kind, b_id, b_port: *b_port,
                        bandwidth_bps: config.bandwidth_bps,
                        latency_ns: config.latency_ns,
                    });
                }
                _ => {}
            }
        }
        let mut switch_states = HashMap::new();
        for s in &switches {
            switch_states.insert(s.id, SwitchState::default());
        }
        Ok(LogPlayer {
            frames,
            apps,
            switches,
            links,
            switch_states,
            link_failed: HashMap::new(),
            cursor: 0,
            cursor_ns: 0,
            duration_ns,
        })
    }

    #[wasm_bindgen(getter)]
    pub fn duration_ns(&self) -> u64 {
        self.duration_ns
    }

    pub fn topology(&self) -> JsValue {
        let t = TopologyOut {
            apps: self.apps.clone(),
            switches: self.switches.clone(),
            links: self.links.clone(),
            duration_ns: self.duration_ns,
        };
        to_js(&t)
    }

    /// Timestamp of the first event strictly after `at_ns`, or
    /// `duration_ns` if none.
    pub fn next_event_ns(&self, at_ns: u64) -> u64 {
        for f in &self.frames {
            if f.at_ns > at_ns {
                return f.at_ns;
            }
        }
        self.duration_ns
    }

    /// Timestamp of the last event strictly before `at_ns`, or 0.
    pub fn prev_event_ns(&self, at_ns: u64) -> u64 {
        let mut best = 0u64;
        for f in &self.frames {
            if f.at_ns >= at_ns {
                break;
            }
            best = f.at_ns;
        }
        best
    }

    /// Move the cursor to `at_ns`. Forward seeks step events; backward
    /// seeks rebuild from scratch.
    pub fn seek(&mut self, at_ns: u64) {
        if at_ns < self.cursor_ns {
            // Reset.
            for s in self.switch_states.values_mut() {
                *s = SwitchState::default();
            }
            self.link_failed.clear();
            self.cursor = 0;
            self.cursor_ns = 0;
        }
        while self.cursor < self.frames.len() && self.frames[self.cursor].at_ns <= at_ns {
            let frame = self.frames[self.cursor].clone();
            self.apply(&frame);
            self.cursor += 1;
            self.cursor_ns = frame.at_ns;
        }
        self.cursor_ns = at_ns;
    }

    /// Compute what's animating in the half-open interval (now-dt, now].
    /// `summary_threshold` is the per-link packet count above which we
    /// emit a `LinkSummary` instead of individual moving packets.
    pub fn frame(
        &self,
        now_ns: u64,
        dt_ns: u64,
        summary_threshold: u32,
        min_visible_ns: u64,
    ) -> JsValue {
        // Visibility window. Real link traversal is often microseconds,
        // way below an animation frame's `dt_ns`, so without padding the
        // dots blink in and out faster than the user can click. We
        // stretch each packet's lifetime to at least `min_visible_ns` of
        // simulated time and add one `dt_ns` of slack so a fast packet
        // is visible for at least one or two wall-clock frames.
        let from_ns = now_ns.saturating_sub(dt_ns);

        // Walk all PacketEgress events. For each one, check whether
        // (now_ns) is within its visibility window.
        let mut per_link: HashMap<u32, Vec<InFlight>> = HashMap::new();
        let mut byte_rate: HashMap<u32, u64> = HashMap::new();
        for f in &self.frames {
            if f.at_ns > now_ns { break; }
            if let LogEvent::PacketEgress { link, packet_id, size_bytes, arrive_at_ns, switch, .. } = f.event {
                let stretched_arrive = arrive_at_ns
                    .max(f.at_ns.saturating_add(min_visible_ns));
                let visible_until = stretched_arrive.saturating_add(dt_ns);
                if visible_until < now_ns {
                    if arrive_at_ns >= from_ns {
                        *byte_rate.entry(link).or_insert(0) += size_bytes;
                    }
                    continue;
                }
                let forward = self.link_endpoint_is_a(link, switch);
                let inflight = InFlight {
                    link,
                    forward,
                    start_ns: f.at_ns,
                    arrive_ns: stretched_arrive,
                    packet_id,
                    size_bytes,
                    kind: 0,
                };
                per_link.entry(link).or_default().push(inflight);
            }
        }

        let mut packets: Vec<MovingPacket> = Vec::new();
        let mut summaries: Vec<LinkSummary> = Vec::new();
        for (link_id, items) in &per_link {
            if items.len() as u32 > summary_threshold {
                let bytes = byte_rate.get(link_id).copied().unwrap_or(0)
                    + items.iter().map(|p| p.size_bytes).sum::<u64>();
                let dt_s = (dt_ns.max(1)) as f64 / 1e9;
                summaries.push(LinkSummary {
                    link: *link_id,
                    packets_per_sec: items.len() as f64 / dt_s,
                    bytes_per_sec: bytes as f64 / dt_s,
                });
            } else {
                for p in items {
                    let span = p.arrive_ns.saturating_sub(p.start_ns).max(1) as f64;
                    let elapsed = now_ns.saturating_sub(p.start_ns) as f64;
                    let mut progress = elapsed / span;
                    if progress < 0.0 { progress = 0.0; }
                    if progress > 1.0 { progress = 1.0; }
                    packets.push(MovingPacket {
                        link: p.link,
                        forward: p.forward,
                        progress,
                        packet_id: p.packet_id,
                        size_bytes: p.size_bytes,
                        kind: p.kind,
                    });
                }
            }
        }

        let link_failed: Vec<u32> = self
            .link_failed
            .iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| *k)
            .collect();
        let switch_failed: Vec<u32> = self
            .switch_states
            .iter()
            .filter(|(_, s)| s.failed)
            .map(|(k, _)| *k)
            .collect();
        let cpu_failed: Vec<u32> = self
            .switch_states
            .iter()
            .filter(|(_, s)| s.cpu_failed)
            .map(|(k, _)| *k)
            .collect();

        let out = FrameOut { summaries, packets, link_failed, switch_failed, cpu_failed };
        to_js(&out)
    }

    /// Every packet that egressed `link_id` between `from_ns` and
    /// `to_ns` (inclusive). Used when the user clicks a high-density
    /// link summary to drill in. Returns a JS array of
    /// `{ packet_id, size_bytes, at_ns, arrive_at_ns }` objects.
    pub fn inspect_link(&self, link_id: u32, from_ns: u64, to_ns: u64) -> JsValue {
        #[derive(serde::Serialize)]
        struct LinkPacketOut {
            packet_id: u64,
            size_bytes: u64,
            at_ns: u64,
            arrive_at_ns: u64,
        }
        let mut out: Vec<LinkPacketOut> = Vec::new();
        for f in &self.frames {
            if f.at_ns > to_ns { break; }
            if f.at_ns < from_ns { continue; }
            if let LogEvent::PacketEgress { link, packet_id, size_bytes, arrive_at_ns, .. } = f.event {
                if link == link_id {
                    out.push(LinkPacketOut {
                        packet_id,
                        size_bytes,
                        at_ns: f.at_ns,
                        arrive_at_ns,
                    });
                }
            }
        }
        to_js(&out)
    }

    /// Find the most recent `PacketIngress` for `packet_id` at or before
    /// `at_ns`. Used by the JS layer when the user clicks an in-flight
    /// packet dot. Returns `JsValue::NULL` if not found.
    pub fn inspect_packet(&self, packet_id: u64, at_ns: u64) -> JsValue {
        let mut latest: Option<&sim_log::PacketSnapshotW> = None;
        let mut history: Vec<u32> = Vec::new();
        for f in &self.frames {
            if f.at_ns > at_ns {
                break;
            }
            if let LogEvent::PacketIngress { switch, packet, .. } = &f.event {
                if packet.id == packet_id {
                    latest = Some(packet);
                    history.push(*switch);
                }
            }
        }
        let p = match latest {
            Some(p) => p,
            None => return JsValue::NULL,
        };
        let out = PacketInspectOut {
            packet_id: p.id,
            kind: p.kind,
            size_bytes: p.size_bytes,
            ip_src: p.ip_src,
            ip_dst: p.ip_dst,
            ip_proto: p.ip_proto,
            ip_ttl: p.ip_ttl,
            label_top: p.label_top,
            label_depth: p.label_depth,
            history,
        };
        to_js(&out)
    }

    /// Return the live state of a switch at the cursor, for the
    /// inspector pane (only called while playback is paused).
    pub fn inspect_switch(&self, switch_id: u32) -> JsValue {
        let st = match self.switch_states.get(&switch_id) {
            Some(s) => s,
            None => return JsValue::NULL,
        };
        let bgp_speaker_ip = self
            .switches
            .iter()
            .find(|s| s.id == switch_id)
            .and_then(|s| s.bgp_speaker_ip);
        // Build a ports list: every link with this switch as one endpoint.
        let mut ports: Vec<PortOut> = Vec::new();
        for l in &self.links {
            let (mine, peer_kind, peer_id, peer_port_raw) =
                if l.a_kind == 1 && l.a_id == switch_id {
                    (l.a_port, l.b_kind, l.b_id, l.b_port)
                } else if l.b_kind == 1 && l.b_id == switch_id {
                    (l.b_port, l.a_kind, l.a_id, l.a_port)
                } else {
                    continue;
                };
            let peer_ip = if peer_kind == 0 {
                self.apps.iter().find(|a| a.id == peer_id).map(|a| a.ip)
            } else {
                self.switches.iter().find(|s| s.id == peer_id).and_then(|s| s.bgp_speaker_ip)
            };
            ports.push(PortOut {
                port: mine,
                link: l.id,
                peer_kind,
                peer_id,
                peer_port: peer_port_raw,
                peer_ip,
            });
        }
        ports.sort_by_key(|p| p.port);
        let program_text = st.program.as_ref()
            .map(format_program)
            .unwrap_or_else(|| "(no program installed)".to_string());
        let tables = st.tables.iter().map(|t| TableOut {
            table_id: t.table_id,
            kind: format!("{:?}", t.kind),
            max_entries: t.max_entries,
            entries: t.entries.iter().map(|e| TableEntryOut {
                id: e.id.0,
                key: e.key,
                prefix_len: e.prefix_len,
                priority: e.priority,
                action: format!("{:?}", e.action),
            }).collect(),
        }).collect();
        let registers = st.registers.iter().map(|r| RegOut {
            array_id: r.array_id, size: r.size,
        }).collect();
        let counters = st.counters.iter().map(|c| CounterOut {
            array_id: c.array_id, size: c.size,
        }).collect();
        let out = InspectOut {
            switch: switch_id,
            bgp_speaker_ip,
            ports,
            program_text,
            tables,
            registers,
            counters,
        };
        to_js(&out)
    }
}

impl LogPlayer {
    fn apply(&mut self, frame: &LogFrame) {
        match &frame.event {
            LogEvent::LinkFailed { id } => { self.link_failed.insert(*id, true); }
            LogEvent::LinkRestored { id } => { self.link_failed.insert(*id, false); }
            LogEvent::SwitchFailed { id } => {
                if let Some(s) = self.switch_states.get_mut(id) { s.failed = true; }
            }
            LogEvent::CpuFailed { id } => {
                if let Some(s) = self.switch_states.get_mut(id) { s.cpu_failed = true; }
            }
            LogEvent::ProgramInstalled { switch, program, tables, registers, counters } => {
                let s = self.switch_states.entry(*switch).or_default();
                s.program = Some(program.clone());
                s.tables = tables.clone();
                s.registers = registers.clone();
                s.counters = counters.clone();
                s.cpu_failed = false; // install_program resets cpu state
            }
            LogEvent::TableEntryInstalled { switch, table, entry } => {
                if let Some(s) = self.switch_states.get_mut(switch) {
                    if let Some(t) = s.tables.iter_mut().find(|t| t.table_id == *table) {
                        // Replace if id already present; otherwise append.
                        if let Some(idx) = t.entries.iter().position(|e| e.id.0 == entry.id.0) {
                            t.entries[idx] = entry.clone();
                        } else {
                            t.entries.push(entry.clone());
                        }
                    }
                }
            }
            LogEvent::TableEntryDeleted { switch, table, entry_id } => {
                if let Some(s) = self.switch_states.get_mut(switch) {
                    if let Some(t) = s.tables.iter_mut().find(|t| t.table_id == *table) {
                        t.entries.retain(|e| e.id.0 != *entry_id);
                    }
                }
            }
            _ => {}
        }
    }

    fn link_endpoint_is_a(&self, link_id: u32, switch_id: u32) -> bool {
        if let Some(l) = self.links.iter().find(|l| l.id == link_id) {
            l.a_kind == 1 && l.a_id == switch_id
        } else {
            true
        }
    }
}

fn node_ref_to_kind_id(n: &NodeRefW) -> (u8, u32) {
    match n {
        NodeRefW::App(id) => (0, *id),
        NodeRefW::Switch(id) => (1, *id),
    }
}

// Pretty-printer for TinyVM programs. Mirrors the text format the
// simulator already uses, so the inspector is readable.
fn format_program(p: &switch_program_types::TinyProgramW) -> String {
    use switch_program_types::InstrW as I;
    let mut s = String::new();
    for stage in &p.stages {
        s.push_str(&format!(
            "stage alu={} mem={}\n",
            stage.max_alu_ops, stage.max_memory_accesses
        ));
        for instr in &stage.instrs {
            s.push_str("    ");
            match instr {
                I::LoadField { dst, field } => {
                    s.push_str(&format!("load    r{}, {:?}\n", dst.0, field));
                }
                I::StoreField { field, src } => {
                    s.push_str(&format!("store   {:?}, r{}\n", field, src.0));
                }
                I::LoadMeta { dst, key } => {
                    s.push_str(&format!("loadm   r{}, m{}\n", dst.0, key.0));
                }
                I::StoreMeta { key, src } => {
                    s.push_str(&format!("storem  m{}, r{}\n", key.0, src.0));
                }
                I::Const { dst, value } => {
                    s.push_str(&format!("const   r{}, {}\n", dst.0, value));
                }
                I::Add { dst, a, b } => s.push_str(&format!("add     r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::Sub { dst, a, b } => s.push_str(&format!("sub     r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::And { dst, a, b } => s.push_str(&format!("and     r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::Or  { dst, a, b } => s.push_str(&format!("or      r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::Xor { dst, a, b } => s.push_str(&format!("xor     r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::Eq  { dst, a, b } => s.push_str(&format!("eq      r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::Lt  { dst, a, b } => s.push_str(&format!("lt      r{}, r{}, r{}\n", dst.0, a.0, b.0)),
                I::TableLookup { table_id, key_reg, result_meta } => {
                    s.push_str(&format!("table   t{}, r{} -> m{}\n", table_id.0, key_reg.0, result_meta.0));
                }
                I::RegisterRead { array, index, dst } => {
                    s.push_str(&format!("rread   a{}[r{}] -> r{}\n", array.0, index.0, dst.0));
                }
                I::RegisterWrite { array, index, src } => {
                    s.push_str(&format!("rwrite  a{}[r{}], r{}\n", array.0, index.0, src.0));
                }
                I::CounterAdd { counter, index, value } => {
                    s.push_str(&format!("counter c{}[r{}], r{}\n", counter.0, index.0, value.0));
                }
                I::BranchIf { cond, target } => s.push_str(&format!("bif     r{} -> {}\n", cond.0, target)),
                I::Drop => s.push_str("drop\n"),
                I::Punt { reason } => s.push_str(&format!("punt    {:?}\n", reason)),
                I::SetEgress { port } => s.push_str(&format!("egress  {}\n", port.0)),
                I::SetQueue { queue } => s.push_str(&format!("queue   {}\n", queue.0)),
                I::Recirculate => s.push_str("recirc\n"),
                I::Noop => s.push_str("noop\n"),
                I::PushLabel { label } => s.push_str(&format!("push_label {}\n", label)),
                I::PopLabel => s.push_str("pop_label\n"),
                I::SwapLabel { label } => s.push_str(&format!("swap_label {}\n", label)),
                I::MarkTrace => s.push_str("mark_trace\n"),
            }
        }
    }
    s
}
