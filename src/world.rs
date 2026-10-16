//! World files: a network as data, not code.
//!
//! A *world* is one TOML file holding everything needed to instantiate a
//! run: the topology, where customer apps sit and what prefixes they own,
//! the workload, and the run parameters. Everything downstream reads this
//! format — the world generator writes it, the simulator builds from it,
//! and the scorer reads it back to know what "correct" meant.
//!
//! The format is meant to grow. A2 adds workload variants and per-link
//! capacity, A3 adds telemetry, A4 adds AS membership and policy. Fields
//! are optional with defaults wherever possible so old worlds keep
//! parsing.
//!
//! Design decisions, fixed:
//! - **Explicit IDs.** Switches and apps carry the integer that appears in
//!   report cards, log excerpts, and quiz exhibits. Deleting a switch from
//!   a world never renumbers the others.
//! - **IPs belong to apps.** Switches have IDs and ports, no addresses.
//! - **Hierarchical prefixes.** Apps take sub-prefixes inside a per-AS
//!   block, so route aggregation is possible later.
//! - **Failures live elsewhere.** One world can host many failure
//!   schedules.

use crate::flow::FlowEngine;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::{App, TrafficPattern};
use crate::link::LinkConfig;
use crate::network::Node;
use crate::packet::IpAddr;
use crate::sim::Simulator;
use crate::types::{AppId, LinkId, PortId, SwitchId};

/// Port number apps attach on. Kept high and out of the way of the
/// inter-switch ports, which are numbered from 0.
pub const APP_PORT: u16 = 100;

// ---------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct World {
    pub world: WorldMeta,
    #[serde(default)]
    pub params: Params,
    #[serde(rename = "switch", default)]
    pub switches: Vec<SwitchSpec>,
    #[serde(rename = "link", default)]
    pub links: Vec<LinkSpec>,
    #[serde(rename = "app", default)]
    pub apps: Vec<AppSpec>,
    pub workload: Workload,
    /// The A2 grading floors. Absent means the A1 criteria apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floors: Option<Floors>,
}

/// The three floors of the A2 handout, with the numbers a world publishes
/// for them. Every value is one of the handout's bracketed constants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Floors {
    /// The collapse note: when first-time data across a bottleneck falls
    /// under this percentage of its capacity in an overloaded window, the
    /// card says so. Measured, never graded; the fairness floor is what
    /// grades the harm.
    #[serde(default = "default_goodput_pct")]
    pub goodput_pct: f64,
    /// [U]: in every window the network delivers at least this percentage
    /// of what the max-min fair allocation would deliver.
    #[serde(default = "default_utilization_pct")]
    pub utilization_pct: f64,
    /// [X]: every customer gets at least this percentage of its max-min
    /// fair share, in every window.
    #[serde(default = "default_share_pct")]
    pub share_pct: f64,
    /// [W]: the window the floors are checked over, in milliseconds.
    #[serde(default = "default_window_ms")]
    pub window_ms: u64,
    /// The fairness check skips windows that begin within this long of a
    /// burst starting: steady state is graded, not the moment of impact.
    #[serde(default = "default_settling_ms")]
    pub settling_ms: u64,
    /// The links the collapse floor watches, as `[a, b]` switch pairs.
    /// Each direction is judged on its own.
    #[serde(default)]
    pub bottlenecks: Vec<[u32; 2]>,
    /// How the fairness floor computes a customer's share. Absent: over
    /// the path its packets took, so only the queues along that path are
    /// on trial (Part 1). Present: over every route from its switch to
    /// its peer's no longer than the shortest plus this many hops, so a
    /// program that leaves a detour empty is judged against the detour
    /// (Part 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detour_hops: Option<u32>,
}

// The calibrated defaults: see scripts/calibrate-a2.sh. Set so the A2
// reference clears every floor with margin on the calibration set and the
// A1 reference fails at least one floor on every world in it.
fn default_goodput_pct() -> f64 {
    50.0
}
fn default_utilization_pct() -> f64 {
    55.0
}
fn default_share_pct() -> f64 {
    40.0
}
fn default_window_ms() -> u64 {
    1_000
}
fn default_settling_ms() -> u64 {
    // Two windows: one for an AIMD customer to recover from the moment a
    // burst hits a busy link, one for a program to move customers.
    2_000
}

impl Default for Floors {
    fn default() -> Self {
        Self {
            goodput_pct: default_goodput_pct(),
            utilization_pct: default_utilization_pct(),
            share_pct: default_share_pct(),
            window_ms: default_window_ms(),
            settling_ms: default_settling_ms(),
            bottlenecks: Vec::new(),
            detour_hops: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldMeta {
    pub name: String,
    /// Seed the generator used. Published for practice worlds, withheld
    /// for grading worlds.
    #[serde(default)]
    pub seed: u64,
    /// Format version. Bump only on a breaking change.
    #[serde(default = "one")]
    pub version: u32,
    /// Topology family this world came from, e.g. `ring_chords`. Empty for
    /// hand-written worlds.
    #[serde(default)]
    pub family: String,
    /// Longest shortest-path in hops. Convergence time scales with this,
    /// so the scorer uses it when it sets the warmup window.
    #[serde(default)]
    pub diameter: u32,
    /// Version of the generator that wrote this file. Bumped whenever the
    /// generator's output changes for a fixed seed.
    #[serde(default)]
    pub generator: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Params {
    /// Capacity of every link, unless a link overrides it.
    #[serde(default = "default_capacity")]
    pub link_capacity_mbps: u64,
    #[serde(default = "default_latency")]
    pub link_latency_us: u64,
    #[serde(default = "default_queue")]
    pub queue_capacity_bytes: u64,
    /// Grace period before scoring starts. Packets sent during warmup do
    /// not count for or against delivery.
    #[serde(default = "default_warmup")]
    pub warmup_ms: u64,
    /// Total simulated run length, warmup included.
    #[serde(default = "default_duration")]
    pub duration_ms: u64,
    /// Fraction of scored packets that must arrive, as a percentage. The
    /// world owns this because it is a property of the world: a run with
    /// failures loses packets that were already on the cable when it died,
    /// and no program can prevent that.
    #[serde(default = "default_delivery_floor")]
    pub delivery_floor_pct: f64,
    /// Shared buffer per switch, in packets of `packet_size_bytes`. This
    /// is the handout's [B]. Absent means the pool does not bind and only
    /// `queue_capacity_bytes` limits each port (A1 behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_budget_packets: Option<u64>,
    /// What one "packet" means when programs size queues and the buffer
    /// is set in packets. Absent means the workload's data packet size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_size_bytes: Option<u64>,
    /// The handout's [Δ]: how long a controller action (route install,
    /// queue config, declaration) takes to reach the data plane, as the
    /// config pipe's latency. Absent means the switch default (10 µs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_delay_us: Option<u64>,
}

fn default_capacity() -> u64 {
    1_000
}
fn default_latency() -> u64 {
    100
}
fn default_queue() -> u64 {
    1 << 16
}
fn default_warmup() -> u64 {
    crate::score::DEFAULT_WARMUP_MS
}
fn default_delivery_floor() -> f64 {
    100.0 * crate::score::DEFAULT_DELIVERY_FLOOR
}
fn default_duration() -> u64 {
    10_000
}

impl Default for Params {
    fn default() -> Self {
        Self {
            link_capacity_mbps: default_capacity(),
            link_latency_us: default_latency(),
            queue_capacity_bytes: default_queue(),
            warmup_ms: default_warmup(),
            duration_ms: default_duration(),
            delivery_floor_pct: default_delivery_floor(),
            buffer_budget_packets: None,
            packet_size_bytes: None,
            control_delay_us: None,
        }
    }
}

impl Params {
    pub fn link_config(&self) -> LinkConfig {
        LinkConfig {
            latency: Duration::from_micros(self.link_latency_us),
            bandwidth_bps: self.link_capacity_mbps * 1_000_000,
            queue_capacity_bytes: self.queue_capacity_bytes,
        }
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchSpec {
    pub id: u32,
    /// Which AS owns this switch. Single-AS in A1–A3; A4 uses it.
    #[serde(default)]
    pub as_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSpec {
    pub a: u32,
    pub b: u32,
    /// Per-link capacity override, in Mbps. A2 uses this to create
    /// bottlenecks; A1 leaves it unset.
    #[serde(default)]
    pub capacity_mbps: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSpec {
    pub id: u32,
    /// Switch this app hangs off.
    pub switch: u32,
    /// The prefix this app owns, in `a.b.c.d/len` form. The app's own
    /// address is the first host in it.
    pub prefix: String,

    // ---- A2 customer rows; ignored under `all_pairs_cbr` ----
    /// Which kind of customer this is. Absent means a pure receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<Preset>,
    /// How it reacts to the network. Absent means it does not (open loop
    /// at `rate_mbps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub law: Option<LawKind>,
    /// Offered rate. Required for every preset but `steam`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_mbps: Option<f64>,
    /// `amazon` only: burst length, gap and first-burst offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    /// The app this one sends to. Required for every preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<u32>,
    /// Law knobs. Defaults: initial = `rate_mbps` (or the link rate for
    /// `steam`), increment = a tenth of that, epoch 100 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub law_initial_mbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub law_increment_mbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub law_epoch_ms: Option<u64>,
    /// Size of this customer's data packets. Absent means the workload's
    /// `size_bytes`. Never larger than the world's packet unit, which is
    /// what buffers and queue caps count in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl AppSpec {
    /// A plain app row: no preset, so under `mixed` it only receives.
    pub fn new(id: u32, switch: u32, prefix: String) -> Self {
        Self {
            id,
            switch,
            prefix,
            preset: None,
            law: None,
            rate_mbps: None,
            on_ms: None,
            off_ms: None,
            start_ms: None,
            peer: None,
            law_initial_mbps: None,
            law_increment_mbps: None,
            law_epoch_ms: None,
            size_bytes: None,
        }
    }
}

/// The five customers of the A2 handout. Each is a demand shape; the law
/// is chosen separately so any customer can run either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// Steady video: constant rate.
    Netflix,
    /// Shopping bursts: on/off at `rate_mbps`.
    Amazon,
    /// A download: as fast as the law allows.
    Steam,
    /// Tiny, sparse, latency-critical: constant rate, small.
    JaneStreet,
    /// A game: constant rate, small, and a peer that talks back.
    Fortnite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LawKind {
    Aimd,
    RateMatch,
}

/// What traffic runs. A1 needs only the all-pairs shorthand; A2 lets
/// every `[[app]]` row say what it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Workload {
    /// Every app sends to every other app, round-robin, at a constant
    /// rate. One line instead of N*(N-1) flow entries.
    AllPairsCbr {
        interval_ms: u64,
        #[serde(default = "default_size")]
        size_bytes: u64,
    },
    /// Each app is a closed-loop customer described by its own row:
    /// `preset`, `law`, `rate_mbps`, `peer` and the on/off fields. Apps
    /// without a preset only receive. Every data packet is `size_bytes`
    /// unless its row gives its own.
    Mixed {
        #[serde(default = "default_size")]
        size_bytes: u64,
    },
}

impl Workload {
    /// The data packet size every app in this world uses.
    pub fn packet_size_bytes(&self) -> u64 {
        match *self {
            Workload::AllPairsCbr { size_bytes, .. } | Workload::Mixed { size_bytes } => size_bytes,
        }
    }
}

fn default_size() -> u64 {
    100
}

// ---------------------------------------------------------------------------
// Parsed prefixes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub addr: u32,
    pub len: u8,
}

impl Prefix {
    pub fn parse(s: &str) -> Result<Self, WorldError> {
        let (addr_s, len_s) = s
            .split_once('/')
            .ok_or_else(|| WorldError::Prefix(format!("`{s}` has no /len")))?;
        let octets: Vec<&str> = addr_s.split('.').collect();
        if octets.len() != 4 {
            return Err(WorldError::Prefix(format!("`{s}` is not dotted quad")));
        }
        let mut addr: u32 = 0;
        for o in octets {
            let v: u8 = o
                .parse()
                .map_err(|_| WorldError::Prefix(format!("`{s}` has a bad octet")))?;
            addr = (addr << 8) | v as u32;
        }
        let len: u8 = len_s
            .parse()
            .map_err(|_| WorldError::Prefix(format!("`{s}` has a bad prefix length")))?;
        if len > 32 {
            return Err(WorldError::Prefix(format!("`{s}` length exceeds 32")));
        }
        Ok(Self { addr, len })
    }

    fn mask(&self) -> u32 {
        if self.len == 0 {
            0
        } else {
            u32::MAX << (32 - self.len)
        }
    }

    /// First usable host address in the prefix — what the app answers to.
    pub fn first_host(&self) -> u32 {
        (self.addr & self.mask()) | 1
    }

    pub fn overlaps(&self, other: &Prefix) -> bool {
        let shorter = self.len.min(other.len);
        let m = if shorter == 0 {
            0
        } else {
            u32::MAX << (32 - shorter)
        };
        (self.addr & m) == (other.addr & m)
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}/{}",
            (self.addr >> 24) & 0xff,
            (self.addr >> 16) & 0xff,
            (self.addr >> 8) & 0xff,
            self.addr & 0xff,
            self.len
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum WorldError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Prefix(String),
    Invalid(String),
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorldError::Io(e) => write!(f, "reading world file: {e}"),
            WorldError::Parse(e) => write!(f, "parsing world file: {e}"),
            WorldError::Prefix(m) => write!(f, "bad prefix: {m}"),
            WorldError::Invalid(m) => write!(f, "invalid world: {m}"),
        }
    }
}

impl std::error::Error for WorldError {}

impl From<std::io::Error> for WorldError {
    fn from(e: std::io::Error) -> Self {
        WorldError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Load, validate, build
// ---------------------------------------------------------------------------

/// What `build` handed to the simulator, so callers can drive failures and
/// score the run without re-deriving anything.
#[derive(Debug, Clone, Default)]
pub struct WorldHandles {
    /// Link id for each `(a, b)` switch pair, in the order the world
    /// declared them. Both orderings are inserted.
    pub links: BTreeMap<(u32, u32), LinkId>,
    /// Every inter-switch link, in declaration order — the failure
    /// schedule indexes into this.
    pub link_order: Vec<LinkId>,
    /// The prefix each app owns.
    pub app_prefixes: BTreeMap<u32, Prefix>,
    /// Which switch each app hangs off.
    pub app_switch: BTreeMap<u32, u32>,
    /// Every AS in the world, in ascending order. A1–A3 have exactly one.
    pub as_ids: Vec<u32>,
}

impl World {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, WorldError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_str(&text)
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Self, WorldError> {
        let world: World = toml::from_str(text).map_err(WorldError::Parse)?;
        world.validate()?;
        Ok(world)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("world serializes")
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), WorldError> {
        std::fs::write(path, self.to_toml())?;
        Ok(())
    }

    /// Every check that must hold before a world is worth running. The
    /// generator relies on these too — it produces worlds and validates
    /// them rather than trusting itself.
    pub fn validate(&self) -> Result<(), WorldError> {
        let bad = |m: String| Err(WorldError::Invalid(m));

        if self.switches.is_empty() {
            return bad("no switches".into());
        }

        // Unique switch ids.
        let mut seen = BTreeSet::new();
        for s in &self.switches {
            if !seen.insert(s.id) {
                return bad(format!("switch {} declared twice", s.id));
            }
        }

        // Links refer to declared switches, no self-loops, no duplicates.
        let mut edges = BTreeSet::new();
        for l in &self.links {
            if !seen.contains(&l.a) || !seen.contains(&l.b) {
                return bad(format!("link {}-{} names an unknown switch", l.a, l.b));
            }
            if l.a == l.b {
                return bad(format!("switch {} has a link to itself", l.a));
            }
            let key = (l.a.min(l.b), l.a.max(l.b));
            if !edges.insert(key) {
                return bad(format!("link {}-{} declared twice", l.a, l.b));
            }
        }

        // Connected. A partitioned world is unscoreable: the handout
        // promises the network always stays connected.
        if !self.is_connected() {
            return bad("topology is not connected".into());
        }

        // Apps: unique ids, known switches, one app per switch (the app
        // port is fixed, so a second app would silently replace the
        // first), parseable and disjoint prefixes.
        let mut app_ids = BTreeSet::new();
        let mut app_switches = BTreeSet::new();
        let mut prefixes: Vec<(u32, Prefix)> = Vec::new();
        for a in &self.apps {
            if !app_ids.insert(a.id) {
                return bad(format!("app {} declared twice", a.id));
            }
            if !seen.contains(&a.switch) {
                return bad(format!("app {} sits on unknown switch {}", a.id, a.switch));
            }
            if !app_switches.insert(a.switch) {
                return bad(format!(
                    "switch {} has two apps; every switch has one app port",
                    a.switch
                ));
            }
            let p = Prefix::parse(&a.prefix)?;
            for (other_id, other) in &prefixes {
                if p.overlaps(other) {
                    return bad(format!(
                        "app {} prefix {} overlaps app {} prefix {}",
                        a.id, p, other_id, other
                    ));
                }
            }
            prefixes.push((a.id, p));
        }
        if self.apps.len() < 2 {
            return bad("a world needs at least two apps".into());
        }

        // Customer rows: a preset needs a peer that exists and is not
        // itself, a rate unless it is a download, and burst timing if it
        // is a shopper.
        if let Workload::Mixed { .. } = self.workload {
            let mut senders = 0;
            for a in &self.apps {
                let Some(preset) = a.preset else { continue };
                senders += 1;
                match a.peer {
                    None => return bad(format!("app {} has a preset but no peer", a.id)),
                    Some(p) if p == a.id => return bad(format!("app {} peers with itself", a.id)),
                    Some(p) if !app_ids.contains(&p) => {
                        return bad(format!("app {} peers with unknown app {p}", a.id))
                    }
                    _ => {}
                }
                if preset != Preset::Steam && !a.rate_mbps.is_some_and(|r| r > 0.0) {
                    return bad(format!("app {} ({preset:?}) needs rate_mbps > 0", a.id));
                }
                if preset == Preset::Amazon && (a.on_ms.unwrap_or(0) == 0 || a.off_ms.is_none()) {
                    return bad(format!("app {} (Amazon) needs on_ms > 0 and off_ms", a.id));
                }
                if let Some(s) = a.size_bytes {
                    let unit = self.params.packet_size_bytes.unwrap_or_else(|| self.workload.packet_size_bytes());
                    if s == 0 || s > unit {
                        return bad(format!(
                            "app {} size_bytes {s} must be between 1 and the world's packet size {unit}",
                            a.id
                        ));
                    }
                }
            }
            if senders == 0 {
                return bad("a mixed workload needs at least one app with a preset".into());
            }
        }

        if let Some(f) = &self.floors {
            for [a, b] in &f.bottlenecks {
                let key = (a.min(b), a.max(b));
                if !edges.contains(&(*key.0, *key.1)) {
                    return bad(format!("bottleneck {a}-{b} is not a declared link"));
                }
            }
            if f.window_ms == 0 {
                return bad("floors.window_ms must be positive".into());
            }
            if self.params.warmup_ms + f.window_ms > self.params.duration_ms {
                return bad("the run is too short for one grading window after warmup".into());
            }
        }

        if self.params.warmup_ms >= self.params.duration_ms {
            return bad("warmup is not shorter than the run".into());
        }

        Ok(())
    }

    /// Breadth-first reachability over the declared links.
    pub fn is_connected(&self) -> bool {
        let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for s in &self.switches {
            adj.entry(s.id).or_default();
        }
        for l in &self.links {
            adj.entry(l.a).or_default().push(l.b);
            adj.entry(l.b).or_default().push(l.a);
        }
        let start = match self.switches.first() {
            Some(s) => s.id,
            None => return false,
        };
        let mut seen = BTreeSet::new();
        let mut q = VecDeque::new();
        seen.insert(start);
        q.push_back(start);
        while let Some(n) = q.pop_front() {
            for &m in adj.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                if seen.insert(m) {
                    q.push_back(m);
                }
            }
        }
        seen.len() == self.switches.len()
    }

    /// Would the topology stay connected with these edges removed?
    /// The failure-schedule generator asks this before committing a cut.
    pub fn connected_without(&self, down: &BTreeSet<(u32, u32)>) -> bool {
        let norm = |a: u32, b: u32| (a.min(b), a.max(b));
        let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for s in &self.switches {
            adj.entry(s.id).or_default();
        }
        for l in &self.links {
            if down.contains(&norm(l.a, l.b)) {
                continue;
            }
            adj.entry(l.a).or_default().push(l.b);
            adj.entry(l.b).or_default().push(l.a);
        }
        let start = match self.switches.first() {
            Some(s) => s.id,
            None => return false,
        };
        let mut seen = BTreeSet::new();
        let mut q = VecDeque::new();
        seen.insert(start);
        q.push_back(start);
        while let Some(n) = q.pop_front() {
            for &m in adj.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                if seen.insert(m) {
                    q.push_back(m);
                }
            }
        }
        seen.len() == self.switches.len()
    }

    /// Every bridge (cut edge) in the topology, as normalised `(lo, hi)`
    /// pairs. A bridge is an edge whose removal partitions the network.
    ///
    /// This is the check that matters for Part 2: the handout promises the
    /// network stays connected while links die, so a world with a bridge is
    /// a world where the promise can be broken by a single failure.
    pub fn bridges(&self) -> Vec<(u32, u32)> {
        let ids: Vec<u32> = self.switches.iter().map(|s| s.id).collect();
        let index: BTreeMap<u32, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let n = ids.len();

        // adjacency as (neighbour index, edge index)
        let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for (e, l) in self.links.iter().enumerate() {
            let (a, b) = match (index.get(&l.a), index.get(&l.b)) {
                (Some(a), Some(b)) => (*a, *b),
                _ => continue,
            };
            adj[a].push((b, e));
            adj[b].push((a, e));
        }

        let mut disc = vec![usize::MAX; n];
        let mut low = vec![usize::MAX; n];
        let mut timer = 0usize;
        let mut found: Vec<usize> = Vec::new();

        fn dfs(
            u: usize,
            parent_edge: usize,
            adj: &[Vec<(usize, usize)>],
            disc: &mut [usize],
            low: &mut [usize],
            timer: &mut usize,
            found: &mut Vec<usize>,
        ) {
            disc[u] = *timer;
            low[u] = *timer;
            *timer += 1;
            for &(v, e) in &adj[u] {
                if e == parent_edge {
                    continue;
                }
                if disc[v] == usize::MAX {
                    dfs(v, e, adj, disc, low, timer, found);
                    low[u] = low[u].min(low[v]);
                    if low[v] > disc[u] {
                        found.push(e);
                    }
                } else {
                    low[u] = low[u].min(disc[v]);
                }
            }
        }

        for s in 0..n {
            if disc[s] == usize::MAX {
                dfs(s, usize::MAX, &adj, &mut disc, &mut low, &mut timer, &mut found);
            }
        }

        found.sort_unstable();
        found
            .into_iter()
            .map(|e| {
                let l = &self.links[e];
                (l.a.min(l.b), l.a.max(l.b))
            })
            .collect()
    }

    /// True when the topology survives any single link failure. Connected
    /// is not enough — a tree is connected and every one of its edges is a
    /// bridge.
    pub fn is_two_edge_connected(&self) -> bool {
        self.is_connected() && self.bridges().is_empty()
    }

    /// Longest shortest-path between any two switches, in hops.
    pub fn diameter(&self) -> u32 {
        let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for s in &self.switches {
            adj.entry(s.id).or_default();
        }
        for l in &self.links {
            adj.entry(l.a).or_default().push(l.b);
            adj.entry(l.b).or_default().push(l.a);
        }
        let mut best = 0u32;
        for s in &self.switches {
            let mut dist: BTreeMap<u32, u32> = BTreeMap::new();
            dist.insert(s.id, 0);
            let mut q = VecDeque::new();
            q.push_back(s.id);
            while let Some(n) = q.pop_front() {
                let d = dist[&n];
                for &m in adj.get(&n).map(|v| v.as_slice()).unwrap_or(&[]) {
                    if !dist.contains_key(&m) {
                        dist.insert(m, d + 1);
                        best = best.max(d + 1);
                        q.push_back(m);
                    }
                }
            }
        }
        best
    }

    /// Instantiate the world into a fresh simulator.
    ///
    /// Ports are assigned deterministically: inter-switch links take the
    /// next free port on each end, counting from 0, in declaration order.
    /// Apps attach at `APP_PORT`. A student program sees exactly this and
    /// nothing more.
    pub fn build(&self, sim: &mut Simulator) -> Result<WorldHandles, WorldError> {
        self.build_with_load(sim, 1.0)
    }

    /// `build`, with every app's offered load multiplied by `load_scale`
    /// (the `--load-scale` knob: the same world at 0.5×, 1×, 2× demand).
    pub fn build_with_load(
        &self,
        sim: &mut Simulator,
        load_scale: f64,
    ) -> Result<WorldHandles, WorldError> {
        use crate::controller::NoopController;
        use crate::switch::{Switch, SwitchConfig};
        use crate::tinyvm::{TinyProgram, TinyVmState};

        if !(load_scale > 0.0 && load_scale.is_finite()) {
            return Err(WorldError::Invalid(format!("load scale {load_scale} is not positive")));
        }
        let lc = self.params.link_config();
        let mut handles = WorldHandles::default();
        handles.as_ids = self
            .switches
            .iter()
            .map(|s| s.as_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        // How many ports each switch needs: one per link, plus the app
        // port. Ports are dense from 0 so `local_ports` reads cleanly.
        let mut degree: BTreeMap<u32, u16> = BTreeMap::new();
        for l in &self.links {
            *degree.entry(l.a).or_insert(0) += 1;
            *degree.entry(l.b).or_insert(0) += 1;
        }

        let size = self.workload.packet_size_bytes();
        let packet_size_bytes = self.params.packet_size_bytes.unwrap_or(size).max(1);

        for s in &self.switches {
            let ports = (*degree.get(&s.id).unwrap_or(&0) as usize) + 1 + APP_PORT as usize;
            // Every switch is owned by its AS. `install_program` targets an
            // owner, so a single program lands on the whole AS — which is
            // exactly the A1 premise: one program, every switch.
            let mut cfg = SwitchConfig::defaults(SwitchId::new(s.id));
            if let Some(us) = self.params.control_delay_us {
                cfg.config_pipe_latency = Duration::from_micros(us);
            }
            let mut switch = Switch::new(
                cfg,
                TinyProgram {
                    stages: Vec::new(),
                },
                TinyVmState::default(),
                Box::new(NoopController),
                ports,
                self.params.queue_capacity_bytes,
            )
            .with_owner(crate::types::OwnerId::new(s.as_id))
            .with_packet_size_bytes(packet_size_bytes);
            if let Some(b) = self.params.buffer_budget_packets {
                switch = switch.with_buffer_budget_packets(b);
            }
            sim.add_switch(switch);
        }

        // Inter-switch links.
        let mut next_port: BTreeMap<u32, u16> = BTreeMap::new();
        for l in &self.links {
            let pa = *next_port.entry(l.a).or_insert(0);
            let pb = *next_port.entry(l.b).or_insert(0);
            next_port.insert(l.a, pa + 1);
            next_port.insert(l.b, pb + 1);

            let mut cfg = lc;
            if let Some(mbps) = l.capacity_mbps {
                cfg.bandwidth_bps = mbps * 1_000_000;
            }
            let id = sim.connect(
                Node::Switch(SwitchId::new(l.a)),
                PortId::new(pa),
                Node::Switch(SwitchId::new(l.b)),
                PortId::new(pb),
                cfg,
            );
            handles.links.insert((l.a, l.b), id);
            handles.links.insert((l.b, l.a), id);
            handles.link_order.push(id);
        }

        // Apps. Every app owns a prefix and answers on its first host
        // address; the workload decides who it talks to.
        let mut parsed: Vec<(u32, u32, Prefix)> = Vec::new();
        for a in &self.apps {
            let p = Prefix::parse(&a.prefix)?;
            parsed.push((a.id, a.switch, p));
            handles.app_prefixes.insert(a.id, p);
            handles.app_switch.insert(a.id, a.switch);
        }

        for (id, switch, prefix) in &parsed {
            let app = match self.workload {
                Workload::AllPairsCbr { interval_ms, .. } => {
                    let peers: Vec<IpAddr> = parsed
                        .iter()
                        .filter(|(other, _, _)| other != id)
                        .map(|(_, _, p)| IpAddr(p.first_host()))
                        .collect();
                    // validate() guarantees at least two apps, so peers is non-empty.
                    let mut app = App::new(
                        AppId::new(*id),
                        IpAddr(prefix.first_host()),
                        prefix.len,
                        peers[0],
                        prefix.len,
                        TrafficPattern::ConstantBitrate {
                            interval: Duration::from_millis(interval_ms).div_f64(load_scale),
                            size_bytes: size,
                        },
                    );
                    for peer in &peers[1..] {
                        app.add_destination(*peer);
                    }
                    app
                }
                Workload::Mixed { .. } => {
                    let spec = self.apps.iter().find(|a| a.id == *id).expect("parsed from apps");
                    let peer_ip = spec
                        .peer
                        .and_then(|p| parsed.iter().find(|(o, _, _)| *o == p))
                        .map(|(_, _, p)| IpAddr(p.first_host()))
                        .unwrap_or(IpAddr(prefix.first_host()));
                    // The pattern never sends: a customer's traffic is
                    // its flow, and a receiver has none. Every app does
                    // announce itself once, so a customer that starts
                    // late is still learnable while no routes exist.
                    let silent = TrafficPattern::BulkTransfer {
                        total_bytes: 0,
                        sent_bytes: 0,
                        pacing: Duration::from_secs(1),
                        size_bytes: 0,
                    };
                    let mut app = App::new(
                        AppId::new(*id),
                        IpAddr(prefix.first_host()),
                        prefix.len,
                        peer_ip,
                        prefix.len,
                        silent,
                    )
                    .with_announce();
                    if let Some(flow) = self.flow_for(spec, size, load_scale) {
                        app = app.with_flow(flow);
                    }
                    app
                }
            };
            sim.add_app(app);
            sim.connect(
                Node::App(AppId::new(*id)),
                PortId::new(0),
                Node::Switch(SwitchId::new(*switch)),
                PortId::new(APP_PORT),
                lc,
            );
        }

        sim.customer_contract = self.customer_contract(load_scale)?;
        Ok(handles)
    }

    /// The customer contract: one row for every `[[app]]` with a preset
    /// and a sender law, in file order. The rate is what the customer
    /// asks for while it is sending, at this load; `steam` has no limit.
    pub fn customer_contract(
        &self,
        load_scale: f64,
    ) -> Result<Vec<switch_program_types::CustomerW>, WorldError> {
        use switch_program_types::{CcaW, CustomerW};
        let mut rows = Vec::new();
        for spec in &self.apps {
            let (Some(preset), Some(law)) = (spec.preset, spec.law) else { continue };
            let prefix = Prefix::parse(&spec.prefix)?;
            rows.push(CustomerW {
                prefix_addr: prefix.addr,
                prefix_len: prefix.len,
                cca: match law {
                    LawKind::Aimd => CcaW::Aimd,
                    LawKind::RateMatch => CcaW::RateMatch,
                },
                ask_bps: match preset {
                    Preset::Steam => None,
                    _ => Some((spec.rate_mbps.unwrap_or(0.0) * 1_000_000.0 * load_scale).round() as u64),
                },
            });
        }
        Ok(rows)
    }

    /// The customer engine one `[[app]]` row asks for, or `None` for a
    /// pure receiver.
    fn flow_for(&self, spec: &AppSpec, size: u64, load_scale: f64) -> Option<FlowEngine> {
        use crate::flow::{Demand, Law};
        let preset = spec.preset?;
        let mbps = |m: f64| m * 1_000_000.0;
        let rate = mbps(spec.rate_mbps.unwrap_or(0.0)) * load_scale;
        let demand = match preset {
            Preset::Netflix | Preset::JaneStreet | Preset::Fortnite => Demand::Constant { bps: rate },
            Preset::Amazon => Demand::OnOff {
                bps: rate,
                on: Duration::from_millis(spec.on_ms.unwrap_or(0)),
                off: Duration::from_millis(spec.off_ms.unwrap_or(0)),
                start: Duration::from_millis(spec.start_ms.unwrap_or(0)),
            },
            Preset::Steam => Demand::Unlimited,
        };
        // A download's natural ceiling is its own link.
        let link_bps = (self.params.link_capacity_mbps * 1_000_000) as f64;
        let initial = spec
            .law_initial_mbps
            .map(mbps)
            .unwrap_or(if preset == Preset::Steam { link_bps } else { rate });
        let law = match spec.law {
            None => Law::Fixed {
                bps: if preset == Preset::Steam { link_bps } else { rate },
            },
            Some(LawKind::Aimd) => Law::Aimd {
                initial_bps: initial,
                increment_bps: spec.law_increment_mbps.map(mbps).unwrap_or(initial / 10.0),
                epoch: Duration::from_millis(spec.law_epoch_ms.unwrap_or(100)),
            },
            Some(LawKind::RateMatch) => Law::RateMatch { initial_bps: initial },
        };
        Some(FlowEngine::new(demand, law, spec.size_bytes.unwrap_or(size)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[world]
name = "t"
seed = 1

[params]
duration_ms = 1000
warmup_ms = 100

[[switch]]
id = 0

[[switch]]
id = 1

[[link]]
a = 0
b = 1

[[app]]
id = 10
switch = 0
prefix = "10.1.0.0/24"

[[app]]
id = 11
switch = 1
prefix = "10.1.1.0/24"

[workload]
kind = "all_pairs_cbr"
interval_ms = 10
size_bytes = 100
"#;

    #[test]
    fn parses_sample() {
        let w = World::from_str(SAMPLE).expect("parses");
        assert_eq!(w.switches.len(), 2);
        assert_eq!(w.links.len(), 1);
        assert_eq!(w.apps.len(), 2);
        assert_eq!(w.params.duration_ms, 1000);
    }

    #[test]
    fn a2_params_reach_the_switches() {
        let text = SAMPLE.replace(
            "[params]\n",
            "[params]\nbuffer_budget_packets = 40\npacket_size_bytes = 250\ncontrol_delay_us = 5000\n",
        );
        let w = World::from_str(&text).expect("parses");
        let mut sim = Simulator::new();
        w.build(&mut sim).unwrap();
        for sw in sim.switches.values() {
            assert_eq!(sw.packet_size_bytes, 250);
            assert_eq!(sw.buffer_budget, 40 * 250);
            assert_eq!(sw.config.config_pipe_latency, Duration::from_millis(5));
        }
        // Absent, the buffer does not bind and the packet size is the
        // workload's.
        let w = World::from_str(SAMPLE).unwrap();
        let mut sim = Simulator::new();
        w.build(&mut sim).unwrap();
        let sw = sim.switches.values().next().unwrap();
        assert_eq!(sw.packet_size_bytes, 100);
        assert_eq!(sw.buffer_budget, w.params.queue_capacity_bytes * sw.egress.len() as u64);
        assert_eq!(sw.config.config_pipe_latency, Duration::from_micros(10));
        // And the new fields survive a round trip through TOML.
        let again = World::from_str(&World::from_str(&text).unwrap().to_toml()).unwrap();
        assert_eq!(again.params.control_delay_us, Some(5000));
        assert_eq!(again.params.buffer_budget_packets, Some(40));
    }

    #[test]
    fn rows_carry_their_own_packet_size() {
        let mixed = |size: &str| {
            SAMPLE
                .replace(
                    "prefix = \"10.1.0.0/24\"\n",
                    &format!("prefix = \"10.1.0.0/24\"\npreset = \"jane_street\"\nrate_mbps = 0.2\npeer = 11\nsize_bytes = {size}\n"),
                )
                .replace(
                    "prefix = \"10.1.1.0/24\"\n",
                    "prefix = \"10.1.1.0/24\"\npreset = \"netflix\"\nrate_mbps = 3.0\npeer = 10\n",
                )
                .replace("kind = \"all_pairs_cbr\"\ninterval_ms = 10\nsize_bytes = 100\n", "kind = \"mixed\"\nsize_bytes = 1000\n")
        };
        let w = World::from_str(&mixed("200")).expect("parses");
        let mut sim = Simulator::new();
        w.build(&mut sim).unwrap();
        let mut sizes: Vec<u64> = sim.apps.values().filter_map(|a| a.flow.as_ref()).map(|f| f.packet_size).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![200, 1000], "the trader's row, and the workload's default");
        // The switch's unit stays the full-size packet: queue caps and the
        // buffer count in those whatever a customer sends.
        for sw in sim.switches.values() {
            assert_eq!(sw.packet_size_bytes, 1000);
        }
        assert!(World::from_str(&w.to_toml()).unwrap().to_toml().contains("size_bytes = 200"));
        // Larger than the unit, or zero, is refused.
        for bad in ["5000", "0"] {
            assert!(World::from_str(&mixed(bad)).and_then(|w| w.validate().map(|_| w)).is_err(), "size {bad}");
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let w = World::from_str(SAMPLE).unwrap();
        let again = World::from_str(&w.to_toml()).expect("re-parses");
        assert_eq!(again.switches.len(), w.switches.len());
        assert_eq!(again.apps.len(), w.apps.len());
    }

    #[test]
    fn prefix_parsing() {
        let p = Prefix::parse("10.1.7.0/24").unwrap();
        assert_eq!(p.len, 24);
        assert_eq!(p.first_host(), 0x0a01_0701);
        assert_eq!(p.to_string(), "10.1.7.0/24");
        assert!(Prefix::parse("10.1.7.0").is_err());
        assert!(Prefix::parse("10.1.7/24").is_err());
        assert!(Prefix::parse("10.1.7.0/33").is_err());
    }

    #[test]
    fn overlapping_prefixes_rejected() {
        let bad = SAMPLE.replace(r#"prefix = "10.1.1.0/24""#, r#"prefix = "10.1.0.0/16""#);
        let err = World::from_str(&bad).unwrap_err();
        assert!(format!("{err}").contains("overlaps"), "{err}");
    }

    #[test]
    fn disconnected_rejected() {
        let bad = SAMPLE.replace("[[link]]\na = 0\nb = 1\n", "");
        let err = World::from_str(&bad).unwrap_err();
        assert!(format!("{err}").contains("not connected"), "{err}");
    }

    #[test]
    fn duplicate_switch_rejected() {
        let bad = SAMPLE.replace("[[switch]]\nid = 1", "[[switch]]\nid = 0");
        let err = World::from_str(&bad).unwrap_err();
        assert!(format!("{err}").contains("twice"), "{err}");
    }

    #[test]
    fn unknown_switch_for_app_rejected() {
        let bad = SAMPLE.replace("id = 11\nswitch = 1", "id = 11\nswitch = 9");
        let err = World::from_str(&bad).unwrap_err();
        assert!(format!("{err}").contains("unknown switch"), "{err}");
    }

    #[test]
    fn connected_without_detects_a_cut() {
        let w = World::from_str(SAMPLE).unwrap();
        let mut down = BTreeSet::new();
        assert!(w.connected_without(&down));
        down.insert((0, 1));
        assert!(!w.connected_without(&down));
    }

    #[test]
    fn builds_into_a_simulator() {
        let w = World::from_str(SAMPLE).unwrap();
        let mut sim = Simulator::new();
        let h = w.build(&mut sim).expect("builds");
        assert_eq!(h.link_order.len(), 1);
        assert_eq!(h.app_prefixes.len(), 2);
        // Each app should target the other.
        sim.run_until(Duration::from_millis(50));
        let sent: u64 = sim.apps.values().map(|a| a.metrics.packets_sent).sum();
        assert!(sent > 0, "apps generated no traffic");
    }
}
