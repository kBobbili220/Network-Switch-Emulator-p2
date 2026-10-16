//! The Assignment 2 scorer: the three floors.
//!
//! A run passes if it clears every floor in every graded window. Each
//! floor blocks one disaster:
//!
//! - **Per-customer throughput.** Every customer gets at least [X]% of
//!   its max-min fair share, computed from what it asked for and its
//!   paths. In Part 1 that is the path its packets took; in Part 2
//!   (`floors.detour_hops`) it is every route the network offers from its
//!   switch to its peer's, so a program that funnels everyone down one
//!   link while a detour sits empty is judged against the detour.
//! - **Total throughput.** The judged customers together receive at
//!   least [U]% of what the max-min fair allocation would deliver.
//! - **No false link-down.** No link fails in A2, so any `link_down`
//!   declaration is a lie about the network.
//!
//! Loss and delay are measured and printed, not graded.
//!
//! Plus the A1 rules that never expire: no forwarding loops, no customer
//! left with nothing. Congestion collapse -- a bottleneck full of copies
//! -- is measured and printed as a note (first-time data as a share of
//! capacity in every overloaded window), not graded: the fairness floor
//! already catches it, since copies are not goodput.
//!
//! Everything is read from the event log; the world file supplies the
//! numbers and the customers' demand shapes.

use crate::sim_log::{DropReasonW, LogEvent, NodeRefW};
use crate::world::{AppSpec, Floors, Preset, World, WorldError};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// A window needs this many packets sent before a loss rate means anything.
const MIN_SENDS_FOR_LOSS: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Nothing to judge: no settled window, no bottleneck declared.
    Skipped,
    /// Measured and shown, never decides the grade.
    Advisory,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skipped => "n/a",
            Status::Advisory => "note",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Floor {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: String,
    pub status: Status,
    /// One line per piece of evidence, printed under "what went wrong".
    pub evidence: Vec<String>,
}

/// One customer's numbers over one window.
#[derive(Debug, Clone, Default)]
pub struct CustomerWindow {
    pub sends: u64,
    pub retransmits: u64,
    pub first_time_bytes: u64,
    pub deliveries: u64,
    pub duplicates: u64,
    pub drops: BTreeMap<String, u64>,
    pub p50_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_hops: u32,
    /// Demand in this window, bits per second. `None` = unlimited.
    pub demand_bps: Option<f64>,
    /// Network-wide max-min share in this window, bits per second, if
    /// the fairness floor graded it. This is the number the floor uses.
    pub share_bps: Option<f64>,
    /// Goodput as a fraction of `share_bps`.
    pub share_ratio: Option<f64>,
    /// The same share computed over the path the packets actually took.
    /// On the card for comparison: when it sits well above `share_bps`
    /// the program's routing, not its queues, lost the window.
    pub path_share_bps: Option<f64>,
    /// Packets dropped as a fraction of packets sent, in a window where
    /// the customer's fair share covered all it asked for, so nothing
    /// obliged the network to drop its packets. For information.
    pub loss_ratio: Option<f64>,
    /// The fairness floor judged this window and the customer's fair
    /// share covered everything it asked for: the customer is not the
    /// one filling the queue, so its loss and delay are what the network
    /// did to it. For information; no floor reads it.
    pub within_share: bool,
    /// Directed links the customer's delivered packets crossed this window.
    pub path: Vec<(u32, u32)>,
    /// Deliveries this window whose path was longer than the shortest
    /// route from the customer's switch to its peer's.
    pub detour_deliveries: u64,
}

#[derive(Debug, Clone)]
pub struct Customer {
    pub app: u32,
    pub preset: String,
    pub law: String,
    pub windows: Vec<CustomerWindow>,
    pub delivered_total: u64,
    pub first_time_total: u64,
    /// First-time bytes delivered per `FINE_BIN_NS` bin from t = 0, for
    /// burst-aligned views finer than a grading window.
    pub fine_first_time_bytes: Vec<u64>,
    /// When each of the customer's bursts begins, ms. Empty for steady ones.
    pub burst_starts_ms: Vec<u64>,
    /// Links on the shortest route from the customer's switch to its peer's.
    pub shortest_links: Option<u32>,
}

/// Bin width of `Customer::fine_first_time_bytes`.
pub const FINE_BIN_NS: u64 = 100_000_000;

/// One bottleneck direction over one window.
#[derive(Debug, Clone, Default)]
pub struct BottleneckWindow {
    pub first_time_bytes: u64,
    pub duplicate_bytes: u64,
    pub all_bytes: u64,
    pub offered_bps: f64,
    pub overloaded: bool,
}

#[derive(Debug, Clone)]
pub struct Bottleneck {
    pub from: u32,
    pub to: u32,
    pub capacity_bps: f64,
    pub windows: Vec<BottleneckWindow>,
    /// Bytes each customer put on this direction, per window (first-time
    /// and copies alike): who the link was given to.
    pub by_customer: BTreeMap<u32, Vec<u64>>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub at_ms: u64,
    pub switch: u32,
    pub port: u16,
    pub up: bool,
}

/// What one switch's buffer held, per `FINE_BIN_NS` bin from t = 0. A
/// packet the switch forwards is held from the moment it arrives until
/// its last bit is on the wire. Read off the log, so the switch's own
/// HELLOs and LSAs, which never arrive, are not counted.
#[derive(Debug, Clone)]
pub struct SwitchBuffer {
    pub switch: u32,
    /// The most bytes held at any moment in each bin.
    pub peak_bytes: Vec<u64>,
    /// At that moment, how many of them were each customer's data.
    pub by_customer: BTreeMap<u32, Vec<u64>>,
}

/// How quiet the control plane went, per window. A program hears its
/// neighbours through its own control packets; a long silence on a link
/// is what turns into a false `link_down`.
#[derive(Debug, Clone, Default)]
pub struct ControlHealth {
    /// Longest gap between two control packets arriving over any one
    /// inter-switch link direction, ms.
    pub worst_gap_ms: Vec<Option<f64>>,
    /// The median link's longest gap, ms.
    pub median_gap_ms: Vec<Option<f64>>,
    /// Control packets and customer acks dropped at switches.
    pub control_drops: Vec<u64>,
    pub ack_drops: Vec<u64>,
    /// The direction (from, to) with the longest gap of the run.
    pub worst_link: Option<(u32, u32)>,
}

/// One queue layout a program configured, and how many ports ended the
/// run with it. Queues are (cap, weight, early-drop line) in packets.
#[derive(Debug, Clone)]
pub struct QueueLayout {
    pub ports: usize,
    pub discipline: u8,
    pub unit_bytes: u64,
    pub queues: Vec<(u32, u32, u32)>,
    /// Bottleneck directions whose sending port has this layout.
    pub bottlenecks: Vec<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub world: String,
    pub program: Option<String>,
    pub load_scale: f64,
    pub run_ms: u64,
    pub warmup_ms: u64,
    pub window_ms: u64,
    pub windows: usize,
    pub floors_cfg: Floors,
    pub floors: Vec<Floor>,
    pub customers: Vec<Customer>,
    pub bottlenecks: Vec<Bottleneck>,
    pub declarations: Vec<Declaration>,
    pub link_events: usize,
    pub looping_packets: u64,
    /// Per inter-switch link direction: bytes per window.
    pub link_load: BTreeMap<(u32, u32), Vec<u64>>,
    pub link_capacity: BTreeMap<(u32, u32), f64>,
    /// Per inter-switch link direction and customer: bytes per window.
    pub link_by_customer: BTreeMap<(u32, u32), BTreeMap<u32, Vec<u64>>>,
    pub drops_by_reason: BTreeMap<String, u64>,
    /// The fullest switches' buffers over time, fullest first.
    pub buffers: Vec<SwitchBuffer>,
    /// The handout's [B] in packets, if the world sets it, and the size
    /// of one such packet.
    pub buffer_pool_packets: Option<u64>,
    pub packet_bytes: u64,
    pub control: ControlHealth,
    pub queue_layouts: Vec<QueueLayout>,
    /// Per window: goodput of the judged customers over the sum of their
    /// fair shares. `None` where no customer was judged.
    pub utilization: Vec<Option<f64>>,
}

impl Report {
    pub fn status(&self) -> Status {
        if self.floors.iter().any(|f| f.status == Status::Fail) {
            Status::Fail
        } else {
            Status::Pass
        }
    }

    /// The card as data, for the grading pipeline: the verdict, every
    /// floor's status and detail line, and per customer the run-long
    /// numbers the card's table shows. Stable keys; add, never rename.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        let window_s = self.window_ms as f64 / 1000.0;
        let customers: Vec<serde_json::Value> = self
            .customers
            .iter()
            .map(|c| {
                let first: u64 = c.windows.iter().map(|w| w.first_time_bytes).sum();
                let goodput_mbps = if self.windows > 0 {
                    (first * 8) as f64 / (window_s * self.windows as f64) / 1e6
                } else {
                    0.0
                };
                let min_share = c
                    .windows
                    .iter()
                    .filter_map(|w| w.share_ratio)
                    .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.min(v))));
                let worst_p99 = c
                    .windows
                    .iter()
                    .filter_map(|w| w.p99_ms)
                    .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))));
                json!({
                    "app": c.app,
                    "preset": c.preset,
                    "law": c.law,
                    "goodput_mbps": goodput_mbps,
                    "sent": c.windows.iter().map(|w| w.sends).sum::<u64>(),
                    "retransmits": c.windows.iter().map(|w| w.retransmits).sum::<u64>(),
                    "duplicates": c.windows.iter().map(|w| w.duplicates).sum::<u64>(),
                    "delivered": c.delivered_total,
                    "first_time": c.first_time_total,
                    "min_share_pct": min_share.map(|v| 100.0 * v),
                    "max_loss_pct": c.windows.iter().filter_map(|w| w.loss_ratio).fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v)))).map(|v| 100.0 * v),
                    "worst_p99_ms": worst_p99,
                })
            })
            .collect();
        json!({
            "assignment": "a2",
            "world": self.world,
            "program": self.program,
            "load_scale": self.load_scale,
            "run_ms": self.run_ms,
            "warmup_ms": self.warmup_ms,
            "window_ms": self.window_ms,
            "windows": self.windows,
            "status": self.status().label(),
            "passed": self.status() == Status::Pass,
            "floors": self.floors.iter().map(|f| json!({
                "id": f.id,
                "name": f.name,
                "status": f.status.label(),
                "passed": f.status != Status::Fail,
                "detail": f.detail,
                "evidence": f.evidence,
            })).collect::<Vec<_>>(),
            "limits": {
                "share_pct": self.floors_cfg.share_pct,
                "utilization_pct": self.floors_cfg.utilization_pct,
                "detour_hops": self.floors_cfg.detour_hops,
            },
            "customers": customers,
            "series": self.series_json(),
            "declarations": self.declarations.iter().map(|d| json!({
                "at_ms": d.at_ms, "switch": d.switch, "port": d.port, "up": d.up,
            })).collect::<Vec<_>>(),
            "looping_packets": self.looping_packets,
            "link_down_declarations": self.declarations.iter().filter(|d| !d.up).count(),
            "drops_by_reason": self.drops_by_reason,
        })
    }
}

impl Report {
    /// Everything over time, for plotting: per customer and window, per
    /// bottleneck and customer, per link, per switch buffer. Kept apart from `customers` so the grading
    /// pipeline, which copies that list into result.json, stays small.
    fn series_json(&self) -> serde_json::Value {
        use serde_json::json;
        let window_s = self.window_ms as f64 / 1000.0;
        let mbps = |bytes: u64, secs: f64| (bytes * 8) as f64 / secs / 1e6;
        let t_ms = |k: usize| self.warmup_ms + k as u64 * self.window_ms;
        let fine_s = FINE_BIN_NS as f64 / 1e9;
        json!({
            "fine_bin_ms": FINE_BIN_NS / 1_000_000,
            "utilization_pct": self.utilization.iter().map(|u| u.map(|v| 100.0 * v)).collect::<Vec<_>>(),
            "customers": self.customers.iter().map(|c| json!({
                "app": c.app,
                "shortest_links": c.shortest_links,
                "burst_starts_ms": c.burst_starts_ms,
                "goodput_fine_mbps": c.fine_first_time_bytes.iter().map(|b| mbps(*b, fine_s)).collect::<Vec<_>>(),
                "windows": c.windows.iter().enumerate().map(|(k, w)| json!({
                    "t_ms": t_ms(k),
                    "sends": w.sends,
                    "retransmits": w.retransmits,
                    "deliveries": w.deliveries,
                    "duplicates": w.duplicates,
                    "detour_deliveries": w.detour_deliveries,
                    "goodput_mbps": mbps(w.first_time_bytes, window_s),
                    "drops": w.drops,
                    "p50_ms": w.p50_ms,
                    "p99_ms": w.p99_ms,
                    "max_hops": w.max_hops,
                    "demand_mbps": w.demand_bps.map(|d| d / 1e6),
                    "share_mbps": w.share_bps.map(|s| s / 1e6),
                    "path_share_mbps": w.path_share_bps.map(|s| s / 1e6),
                    "share_pct": w.share_ratio.map(|r| 100.0 * r),
                    "loss_pct": w.loss_ratio.map(|r| 100.0 * r),
                    "within_share": w.within_share,
                    "path": w.path,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "bottlenecks": self.bottlenecks.iter().map(|b| json!({
                "from": b.from,
                "to": b.to,
                "capacity_mbps": b.capacity_bps / 1e6,
                "windows": b.windows.iter().enumerate().map(|(k, w)| json!({
                    "t_ms": t_ms(k),
                    "first_time_pct": 100.0 * mbps(w.first_time_bytes, window_s) * 1e6 / b.capacity_bps,
                    "duplicate_pct": 100.0 * mbps(w.duplicate_bytes, window_s) * 1e6 / b.capacity_bps,
                    "offered_pct": 100.0 * w.offered_bps / b.capacity_bps,
                    "overloaded": w.overloaded,
                })).collect::<Vec<_>>(),
                "by_customer_mbps": b.by_customer.iter().map(|(app, v)| (
                    app.to_string(),
                    v.iter().map(|x| mbps(*x, window_s)).collect::<Vec<_>>(),
                )).collect::<BTreeMap<_, _>>(),
            })).collect::<Vec<_>>(),
            "buffers": {
                "pool_packets": self.buffer_pool_packets,
                "packet_bytes": self.packet_bytes,
                "switches": self.buffers.iter().map(|b| json!({
                    "switch": b.switch,
                    "peak_packets": b.peak_bytes.iter().map(|x| *x as f64 / self.packet_bytes as f64).collect::<Vec<_>>(),
                    "by_customer_packets": b.by_customer.iter().map(|(app, v)| (
                        app.to_string(),
                        v.iter().map(|x| *x as f64 / self.packet_bytes as f64).collect::<Vec<_>>(),
                    )).collect::<BTreeMap<_, _>>(),
                })).collect::<Vec<_>>(),
            },
            "control": {
                "worst_gap_ms": self.control.worst_gap_ms,
                "median_gap_ms": self.control.median_gap_ms,
                "control_drops": self.control.control_drops,
                "ack_drops": self.control.ack_drops,
                "worst_link": self.control.worst_link,
            },
            "queue_layouts": self.queue_layouts.iter().map(|q| json!({
                "ports": q.ports,
                "discipline": q.discipline,
                "unit_bytes": q.unit_bytes,
                "queues": q.queues,
                "bottlenecks": q.bottlenecks,
            })).collect::<Vec<_>>(),
            "links": self.link_load.iter().map(|((from, to), v)| {
                let cap = self.link_capacity.get(&(*from, *to)).copied().unwrap_or(0.0);
                json!({
                    "from": from,
                    "to": to,
                    "capacity_mbps": cap / 1e6,
                    "load_pct": v.iter().map(|b| if cap > 0.0 { 100.0 * mbps(*b, window_s) * 1e6 / cap } else { 0.0 }).collect::<Vec<_>>(),
                    "by_customer_mbps": self.link_by_customer.get(&(*from, *to)).into_iter().flatten().map(|(app, v)| (
                        app.to_string(),
                        (0..self.windows).map(|k| mbps(v.get(k).copied().unwrap_or(0), window_s)).collect::<Vec<_>>(),
                    )).collect::<BTreeMap<_, _>>(),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------
// Demand, from the world file
// ---------------------------------------------------------------------------

/// Offered load of one customer over `[lo, hi)` ns, in bits per second.
/// `None` means unlimited.
fn demand_over(spec: &AppSpec, lo: u64, hi: u64, load_scale: f64) -> Option<f64> {
    let preset = spec.preset?;
    let rate = spec.rate_mbps.unwrap_or(0.0) * 1_000_000.0 * load_scale;
    match preset {
        Preset::Steam => None,
        Preset::Netflix | Preset::JaneStreet | Preset::Fortnite => Some(rate),
        Preset::Amazon => {
            let on = spec.on_ms.unwrap_or(0) * 1_000_000;
            let off = spec.off_ms.unwrap_or(0) * 1_000_000;
            let start = spec.start_ms.unwrap_or(0) * 1_000_000;
            let period = on + off;
            if period == 0 || hi <= lo {
                return Some(rate);
            }
            // Overlap of the ON intervals with [lo, hi).
            let mut on_ns = 0u64;
            let first = if lo > start { (lo - start) / period } else { 0 };
            let mut k = first;
            loop {
                let s = start + k * period;
                if s >= hi {
                    break;
                }
                let e = s + on;
                let a = s.max(lo);
                let b = e.min(hi);
                if b > a {
                    on_ns += b - a;
                }
                k += 1;
            }
            Some(rate * on_ns as f64 / (hi - lo) as f64)
        }
    }
}

/// Times at which a customer's burst begins, within `[0, run_ns]`.
fn burst_starts(spec: &AppSpec, run_ns: u64) -> Vec<u64> {
    let mut v = Vec::new();
    if spec.preset == Some(Preset::Amazon) {
        let on = spec.on_ms.unwrap_or(0) * 1_000_000;
        let off = spec.off_ms.unwrap_or(0) * 1_000_000;
        let period = on + off;
        let mut t = spec.start_ms.unwrap_or(0) * 1_000_000;
        while t <= run_ns {
            v.push(t);
            if period == 0 {
                break;
            }
            t += period;
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Max-min fairness by water filling
// ---------------------------------------------------------------------------

/// Progressive filling. `demands[i]` is `None` for an unlimited flow;
/// `paths[i]` lists the directed link indices flow `i` uses; `cap[l]` is
/// each link's capacity. Returns each flow's max-min share.
pub fn max_min_shares(demands: &[Option<f64>], paths: &[Vec<usize>], cap: &[f64]) -> Vec<f64> {
    let n = demands.len();
    let mut share = vec![0.0f64; n];
    let mut remaining = cap.to_vec();
    let mut active: Vec<bool> = (0..n)
        .map(|i| demands[i].map_or(true, |d| d > 0.0))
        .collect();
    // A flow with no links is bounded only by its demand.
    for i in 0..n {
        if paths[i].is_empty() && active[i] {
            share[i] = demands[i].unwrap_or(f64::INFINITY);
            active[i] = false;
        }
    }
    let eps = 1e-6;
    for _round in 0..(n + cap.len() + 2) {
        if !active.iter().any(|a| *a) {
            break;
        }
        // Flows per link among the active ones.
        let mut count = vec![0usize; cap.len()];
        for i in 0..n {
            if active[i] {
                for &l in &paths[i] {
                    count[l] += 1;
                }
            }
        }
        let mut inc = f64::INFINITY;
        for l in 0..cap.len() {
            if count[l] > 0 {
                inc = inc.min(remaining[l] / count[l] as f64);
            }
        }
        for i in 0..n {
            if active[i] {
                if let Some(d) = demands[i] {
                    inc = inc.min(d - share[i]);
                }
            }
        }
        if !(inc > eps) {
            // Some link is exhausted: freeze everything on it.
            for l in 0..cap.len() {
                if count[l] > 0 && remaining[l] <= eps {
                    for i in 0..n {
                        if active[i] && paths[i].contains(&l) {
                            active[i] = false;
                        }
                    }
                }
            }
            for i in 0..n {
                if active[i] {
                    if let Some(d) = demands[i] {
                        if d - share[i] <= eps {
                            active[i] = false;
                        }
                    }
                }
            }
            continue;
        }
        for i in 0..n {
            if active[i] {
                share[i] += inc;
            }
        }
        for l in 0..cap.len() {
            remaining[l] -= inc * count[l] as f64;
        }
        // Retire satisfied flows and everything on a now-full link.
        for i in 0..n {
            if active[i] {
                if let Some(d) = demands[i] {
                    if share[i] >= d - eps {
                        active[i] = false;
                    }
                }
            }
        }
        for l in 0..cap.len() {
            if count[l] > 0 && remaining[l] <= eps {
                for i in 0..n {
                    if active[i] && paths[i].contains(&l) {
                        active[i] = false;
                    }
                }
            }
        }
    }
    share
}

/// Max-min shares when every flow may use any one path on its menu.
/// `menus[i]` lists flow `i`'s candidate paths, each a list of directed
/// link indices into `cap`; an empty menu means the flow is bounded only
/// by its demand. Each flow is given one path -- what a router can do --
/// and the shares are then water-filled over the paths given.
///
/// Paths are handed out greedily, in a fixed order: flows with the
/// fewest choices first, then the unlimited, then the biggest demands,
/// then by index. Each takes the path whose thinnest link, shared with
/// everyone already placed on it, leaves it the most; the shortest wins
/// ties. The result is what the customers could have had with the
/// routing done well; how the program actually routed them does not
/// enter.
pub fn network_shares(demands: &[Option<f64>], menus: &[Vec<Vec<usize>>], cap: &[f64]) -> Vec<f64> {
    let paths = assign_paths(demands, menus, cap);
    max_min_shares(demands, &paths, cap)
}

/// The greedy path assignment behind [`network_shares`]: one path per
/// flow (empty when the flow has no menu).
pub fn assign_paths(demands: &[Option<f64>], menus: &[Vec<Vec<usize>>], cap: &[f64]) -> Vec<Vec<usize>> {
    let n = demands.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let choices = |i: usize| menus[i].len().max(1);
        let size = |i: usize| demands[i].unwrap_or(f64::INFINITY);
        choices(a)
            .cmp(&choices(b))
            .then_with(|| size(b).partial_cmp(&size(a)).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.cmp(&b))
    });
    let mut on_link = vec![0usize; cap.len()];
    let mut chosen: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in order {
        if menus[i].is_empty() {
            continue;
        }
        let mut best: Option<(f64, usize)> = None;
        for (pi, p) in menus[i].iter().enumerate() {
            let room = p
                .iter()
                .map(|&l| cap[l] / (on_link[l] + 1) as f64)
                .fold(f64::INFINITY, f64::min);
            if best.map_or(true, |(br, _)| room > br + 1e-9) {
                best = Some((room, pi));
            }
        }
        let pi = best.map(|b| b.1).unwrap_or(0);
        for &l in &menus[i][pi] {
            on_link[l] += 1;
        }
        chosen[i] = menus[i][pi].clone();
    }
    chosen
}

/// Every simple path from `from` to `to` over `adj` no longer than the
/// shortest plus `slack` hops, as lists of directed `(a, b)` edges,
/// shortest first and then by node order. Empty when `to` is unreachable;
/// a single empty path when `from == to`.
pub fn path_menu(adj: &BTreeMap<u32, Vec<u32>>, from: u32, to: u32, slack: usize) -> Vec<Vec<(u32, u32)>> {
    if from == to {
        return vec![Vec::new()];
    }
    // Distances to `to` prune the search.
    let mut dist: BTreeMap<u32, usize> = BTreeMap::new();
    let mut frontier = std::collections::VecDeque::from([to]);
    dist.insert(to, 0);
    while let Some(v) = frontier.pop_front() {
        let d = dist[&v];
        for &u in adj.get(&v).map(|v| v.as_slice()).unwrap_or(&[]) {
            if !dist.contains_key(&u) {
                dist.insert(u, d + 1);
                frontier.push_back(u);
            }
        }
    }
    let Some(&shortest) = dist.get(&from) else { return Vec::new() };
    let limit = shortest + slack;
    let mut out: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut path: Vec<(u32, u32)> = Vec::new();
    let mut on_path: BTreeSet<u32> = BTreeSet::from([from]);
    fn walk(
        adj: &BTreeMap<u32, Vec<u32>>,
        dist: &BTreeMap<u32, usize>,
        at: u32,
        to: u32,
        limit: usize,
        path: &mut Vec<(u32, u32)>,
        on_path: &mut BTreeSet<u32>,
        out: &mut Vec<Vec<(u32, u32)>>,
    ) {
        if at == to {
            out.push(path.clone());
            return;
        }
        let mut next: Vec<u32> = adj.get(&at).cloned().unwrap_or_default();
        next.sort_unstable();
        for u in next {
            if on_path.contains(&u) {
                continue;
            }
            let Some(&rest) = dist.get(&u) else { continue };
            if path.len() + 1 + rest > limit {
                continue;
            }
            on_path.insert(u);
            path.push((at, u));
            walk(adj, dist, u, to, limit, path, on_path, out);
            path.pop();
            on_path.remove(&u);
        }
    }
    walk(adj, &dist, from, to, limit, &mut path, &mut on_path, &mut out);
    out.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    out
}

// ---------------------------------------------------------------------------
// The pass over the log
// ---------------------------------------------------------------------------

struct PktRec {
    flow: u32,
    seq: u64,
    size: u64,
    first_ns: u64,
    hops: u32,
    visited: u64,
    looped: bool,
    /// Directed links crossed so far: (from switch, to switch).
    links: Vec<(u32, u32)>,
}

#[derive(Default)]
struct FlowAcc {
    windows: Vec<WindowAcc>,
    sent_seqs: HashSet<u64>,
    delivered_seqs: HashSet<u64>,
    last_path: BTreeSet<(u32, u32)>,
    delivered_total: u64,
    first_time_total: u64,
    fine: Vec<u64>,
    shortest_links: Option<usize>,
}

#[derive(Default)]
struct WindowAcc {
    sends: u64,
    retransmits: u64,
    first_time_bytes: u64,
    deliveries: u64,
    duplicates: u64,
    drops: BTreeMap<String, u64>,
    latencies_ns: Vec<u64>,
    max_hops: u32,
    path: BTreeSet<(u32, u32)>,
    detour_deliveries: u64,
}

/// Links on a shortest route between two switches, by breadth-first search.
fn hop_distance(adj: &BTreeMap<u32, Vec<u32>>, from: u32, to: u32) -> Option<usize> {
    let mut dist: BTreeMap<u32, usize> = BTreeMap::from([(from, 0)]);
    let mut q = std::collections::VecDeque::from([from]);
    while let Some(u) = q.pop_front() {
        if u == to {
            return Some(dist[&u]);
        }
        for &v in adj.get(&u).into_iter().flatten() {
            if !dist.contains_key(&v) {
                dist.insert(v, dist[&u] + 1);
                q.push_back(v);
            }
        }
    }
    None
}

fn grow<T: Default>(v: &mut Vec<T>, k: usize) -> &mut T {
    while v.len() <= k {
        v.push(T::default());
    }
    &mut v[k]
}

fn percentile_ms(sorted_ns: &[u64], pct: usize) -> Option<f64> {
    if sorted_ns.is_empty() {
        return None;
    }
    let idx = (sorted_ns.len() * pct / 100).min(sorted_ns.len() - 1);
    Some(sorted_ns[idx] as f64 / 1e6)
}

fn drop_label(r: &DropReasonW) -> &'static str {
    match r {
        DropReasonW::QueueFull => "queue full",
        DropReasonW::Policy => "dropped by program",
        DropReasonW::Ttl => "ttl expired",
        DropReasonW::NoRoute => "no route",
        DropReasonW::NoEgressLink => "no egress link",
        DropReasonW::LinkDrop => "link down",
        DropReasonW::SwitchFailed => "switch failed",
        DropReasonW::Recirculation => "recirculation limit",
    }
}

/// `PacketKind::Ack` as the log writes it.
const PACKET_KIND_ACK: u8 = 4;
/// How many switches' buffers the report keeps.
const BUFFERS_KEPT: usize = 2;

#[derive(Clone, Copy, PartialEq)]
enum HeldClass {
    Data(u32),
    Ack,
    Control,
}

/// A packet a switch has received and not yet sent, dropped or punted.
struct Held {
    size: u64,
    class: HeldClass,
    at_ns: u64,
}

impl Held {
    fn flow(&self) -> Option<u32> {
        match self.class {
            HeldClass::Data(f) => Some(f),
            _ => None,
        }
    }
}

/// One switch's buffer over time from its arrivals and departures.
fn buffer_series(switch: u32, mut events: Vec<(u64, i64, Option<u32>)>, bins: usize, flows: &[u32]) -> SwitchBuffer {
    // Departures before arrivals at the same instant, so a packet that
    // replaces another is not counted beside it.
    events.sort_unstable_by_key(|e| (e.0, e.1));
    let mut peak = vec![0u64; bins];
    let mut by: BTreeMap<u32, Vec<u64>> = flows.iter().map(|f| (*f, vec![0u64; bins])).collect();
    let mut now: BTreeMap<u32, i64> = flows.iter().map(|f| (*f, 0)).collect();
    let mut total = 0i64;
    let mut bin = 0usize;
    for (t, delta, flow) in events {
        let b = (t / FINE_BIN_NS) as usize;
        if b >= bins {
            break;
        }
        // What was already there carries into every bin up to this one.
        while bin < b {
            bin += 1;
            peak[bin] = total.max(0) as u64;
            for (f, v) in &now {
                by.get_mut(f).unwrap()[bin] = (*v).max(0) as u64;
            }
        }
        total += delta;
        if let Some(f) = flow {
            *now.get_mut(&f).unwrap() += delta;
        }
        if total.max(0) as u64 > peak[bin] {
            peak[bin] = total as u64;
            for (f, v) in &now {
                by.get_mut(f).unwrap()[bin] = (*v).max(0) as u64;
            }
        }
    }
    while bin + 1 < bins {
        bin += 1;
        peak[bin] = total.max(0) as u64;
        for (f, v) in &now {
            by.get_mut(f).unwrap()[bin] = (*v).max(0) as u64;
        }
    }
    SwitchBuffer {
        switch,
        peak_bytes: peak,
        by_customer: by,
    }
}

/// Score a `.simlog` of a mixed-workload world against its floors.
pub fn score_file(
    log: &Path,
    world: &World,
    program: Option<String>,
    load_scale: f64,
) -> Result<Report, WorldError> {
    let cfg = world.floors.clone().unwrap_or_default();
    let warmup_ns = world.params.warmup_ms * 1_000_000;
    let window_ns = cfg.window_ms.max(1) * 1_000_000;
    let window_of = |t: u64| -> Option<usize> {
        if t < warmup_ns {
            None
        } else {
            Some(((t - warmup_ns) / window_ns) as usize)
        }
    };

    // Who is a customer: apps with a preset. Flow id == app id.
    let customers: Vec<&AppSpec> = world.apps.iter().filter(|a| a.preset.is_some()).collect();
    let mut flows: BTreeMap<u32, FlowAcc> = customers.iter().map(|a| (a.id, FlowAcc::default())).collect();
    {
        // Each customer's shortest route, to tell a detour from the
        // default path at delivery time.
        let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for l in &world.links {
            adj.entry(l.a).or_default().push(l.b);
            adj.entry(l.b).or_default().push(l.a);
        }
        let switch_of: BTreeMap<u32, u32> = world.apps.iter().map(|a| (a.id, a.switch)).collect();
        for c in &customers {
            if let Some(&dst) = c.peer.and_then(|p| switch_of.get(&p)) {
                flows.get_mut(&c.id).unwrap().shortest_links = hop_distance(&adj, c.switch, dst);
            }
        }
    }
    let mut bn_by_flow: BTreeMap<((u32, u32), u32), Vec<u64>> = BTreeMap::new();

    // Link directions.
    let default_cap = (world.params.link_capacity_mbps * 1_000_000) as f64;
    let mut cap_of: BTreeMap<(u32, u32), f64> = BTreeMap::new();
    for l in &world.links {
        let c = l.capacity_mbps.map(|m| (m * 1_000_000) as f64).unwrap_or(default_cap);
        cap_of.insert((l.a, l.b), c);
        cap_of.insert((l.b, l.a), c);
    }
    let mut link_ends: HashMap<u32, (u32, u32)> = HashMap::new(); // log link id -> (a, b)
    let mut link_load: BTreeMap<(u32, u32), Vec<u64>> = BTreeMap::new();
    let mut link_by_customer: BTreeMap<(u32, u32), BTreeMap<u32, Vec<u64>>> = BTreeMap::new();
    let bottleneck_dirs: Vec<(u32, u32)> = cfg
        .bottlenecks
        .iter()
        .flat_map(|[a, b]| [(*a, *b), (*b, *a)])
        .collect();
    let mut bn_windows: BTreeMap<(u32, u32), Vec<BottleneckWindow>> =
        bottleneck_dirs.iter().map(|d| (*d, Vec::new())).collect();
    let mut bn_seen: HashSet<(u32, u32, u32, u64)> = HashSet::new(); // (from, to, flow, seq)

    // For the time series: what each switch held, how quiet each link's
    // control traffic went, and the queues each port was given.
    let mut link_latency: HashMap<u32, u64> = HashMap::new();
    let mut port_peer: HashMap<(u32, u16), u32> = HashMap::new();
    let mut held: HashMap<(u32, u64), Held> = HashMap::new();
    let mut buf_events: BTreeMap<u32, Vec<(u64, i64, Option<u32>)>> = BTreeMap::new();
    let mut ctl_last: HashMap<(u32, u16), u64> = HashMap::new();
    let mut ctl_gap: BTreeMap<(u32, u16), Vec<u64>> = BTreeMap::new();
    let mut ctl_drops: Vec<u64> = Vec::new();
    let mut ack_drops: Vec<u64> = Vec::new();
    let mut layouts: BTreeMap<(u32, u16), (u8, u64, Vec<(u32, u32, u32)>)> = BTreeMap::new();

    let mut pkts: HashMap<u64, PktRec> = HashMap::new();
    let mut declarations = Vec::new();
    let mut link_events = 0usize;
    let mut looping = 0u64;
    let mut drops_by_reason: BTreeMap<String, u64> = BTreeMap::new();
    let mut run_ns = 0u64;

    let read = |e: crate::sim_log::writer::ReadError| {
        WorldError::Invalid(format!("reading {}: {e:?}", log.display()))
    };

    crate::sim_log::writer::for_each_frame(log, |frame| {
        run_ns = run_ns.max(frame.at_ns);
        match frame.event {
            LogEvent::LinkAdded { id, a, a_port, b, b_port, config } => {
                link_latency.insert(id, config.latency_ns);
                if let (NodeRefW::Switch(x), NodeRefW::Switch(y)) = (a, b) {
                    link_ends.insert(id, (x, y));
                    port_peer.insert((x, a_port), y);
                    port_peer.insert((y, b_port), x);
                }
            }
            LogEvent::PortQueuesConfigured { switch, port, discipline, unit_bytes, queues } => {
                let q = queues.iter().map(|q| (q.max_packets, q.weight, q.drop_threshold_packets)).collect();
                layouts.insert((switch, port), (discipline, unit_bytes, q));
            }
            LogEvent::PacketPunted { switch, packet_id, .. } => {
                held.remove(&(switch, packet_id));
            }
            LogEvent::LinkFailed { .. } | LogEvent::LinkRestored { .. } => link_events += 1,
            LogEvent::LinkDeclared { switch, port, up } => declarations.push(Declaration {
                at_ms: frame.at_ns / 1_000_000,
                switch,
                port,
                up,
            }),
            LogEvent::PacketIngress { switch, port, packet: snap } => {
                let class = if snap.kind == PACKET_KIND_ACK {
                    HeldClass::Ack
                } else if snap.kind == 0 && snap.seq.is_some() && flows.contains_key(&(snap.flow_id as u32)) {
                    HeldClass::Data(snap.flow_id as u32)
                } else {
                    HeldClass::Control
                };
                held.insert((switch, snap.id), Held { size: snap.size_bytes, class, at_ns: frame.at_ns });
                if class == HeldClass::Control && port_peer.contains_key(&(switch, port)) {
                    if let Some(prev) = ctl_last.insert((switch, port), frame.at_ns) {
                        if let Some(k) = window_of(frame.at_ns) {
                            let g = grow(ctl_gap.entry((switch, port)).or_default(), k);
                            *g = (*g).max(frame.at_ns - prev);
                        }
                    }
                }
                if snap.kind != 0 {
                    return;
                }
                let Some(seq) = snap.seq else { return };
                let flow = snap.flow_id as u32;
                if !flows.contains_key(&flow) {
                    return;
                }
                let bit = if switch <= 63 { 1u64 << switch } else { 0 };
                let rec = pkts.entry(snap.id).or_insert_with(|| {
                    // First sight of this copy: a send.
                    let f = flows.get_mut(&flow).unwrap();
                    if let Some(k) = window_of(frame.at_ns) {
                        let w = grow(&mut f.windows, k);
                        w.sends += 1;
                        if !f.sent_seqs.insert(seq) {
                            w.retransmits += 1;
                        }
                    } else {
                        f.sent_seqs.insert(seq);
                    }
                    PktRec {
                        flow,
                        seq,
                        size: snap.size_bytes,
                        first_ns: frame.at_ns,
                        hops: 0,
                        visited: 0,
                        looped: false,
                        links: Vec::new(),
                    }
                });
                if bit != 0 && rec.visited & bit != 0 {
                    rec.looped = true;
                }
                rec.visited |= bit;
                rec.hops += 1;
            }
            LogEvent::PacketEgress {
                switch,
                link,
                packet_id,
                size_bytes,
                arrive_at_ns,
                ..
            } => {
                if let Some(h) = held.remove(&(switch, packet_id)) {
                    // Only a packet that is sent ever sat in the buffer:
                    // from its arrival until its last bit is on the wire.
                    // One that was refused, dropped or punted never did.
                    let gone = arrive_at_ns
                        .saturating_sub(link_latency.get(&link).copied().unwrap_or(0))
                        .max(frame.at_ns);
                    let ev = buf_events.entry(switch).or_default();
                    ev.push((h.at_ns, h.size as i64, h.flow()));
                    ev.push((gone, -(h.size as i64), h.flow()));
                }
                let Some(&(a, b)) = link_ends.get(&link) else { return };
                let to = if switch == a { b } else { a };
                let dir = (switch, to);
                if let Some(k) = window_of(frame.at_ns) {
                    *grow(link_load.entry(dir).or_default(), k) += size_bytes;
                }
                let Some(rec) = pkts.get_mut(&packet_id) else { return };
                rec.links.push(dir);
                if let Some(k) = window_of(frame.at_ns) {
                    *grow(link_by_customer.entry(dir).or_default().entry(rec.flow).or_default(), k) += size_bytes;
                }
                if let Some(v) = bn_windows.get_mut(&dir) {
                    if let Some(k) = window_of(frame.at_ns) {
                        *grow(bn_by_flow.entry((dir, rec.flow)).or_default(), k) += size_bytes;
                        let w = grow(v, k);
                        w.all_bytes += size_bytes;
                        if bn_seen.insert((dir.0, dir.1, rec.flow, rec.seq)) {
                            w.first_time_bytes += size_bytes;
                        } else {
                            w.duplicate_bytes += size_bytes;
                        }
                    }
                }
            }
            LogEvent::PacketDelivered { packet_id, .. } => {
                let Some(rec) = pkts.remove(&packet_id) else { return };
                if rec.looped {
                    looping += 1;
                }
                let f = flows.get_mut(&rec.flow).unwrap();
                f.delivered_total += 1;
                let first = f.delivered_seqs.insert(rec.seq);
                if first {
                    f.first_time_total += 1;
                    *grow(&mut f.fine, (frame.at_ns / FINE_BIN_NS) as usize) += rec.size;
                }
                f.last_path = rec.links.iter().copied().collect();
                let detour = f.shortest_links.is_some_and(|s| rec.links.len() > s);
                if let Some(k) = window_of(frame.at_ns) {
                    let w = grow(&mut f.windows, k);
                    w.deliveries += 1;
                    if detour {
                        w.detour_deliveries += 1;
                    }
                    if first {
                        w.first_time_bytes += rec.size;
                    } else {
                        w.duplicates += 1;
                    }
                    w.latencies_ns.push(frame.at_ns.saturating_sub(rec.first_ns));
                    w.max_hops = w.max_hops.max(rec.hops);
                    w.path.extend(rec.links.iter().copied());
                }
            }
            LogEvent::PacketDropped { at, reason, packet_id } => {
                if let NodeRefW::Switch(s) = at {
                    // A packet a switch drops without ever having received
                    // it is one its own program injected: control traffic.
                    let class = held.remove(&(s, packet_id)).map_or(HeldClass::Control, |h| h.class);
                    if let Some(k) = window_of(frame.at_ns) {
                        match class {
                            HeldClass::Control => *grow(&mut ctl_drops, k) += 1,
                            HeldClass::Ack => *grow(&mut ack_drops, k) += 1,
                            HeldClass::Data(_) => {}
                        }
                    }
                }
                let Some(rec) = pkts.remove(&packet_id) else { return };
                if rec.looped {
                    looping += 1;
                }
                let label = drop_label(&reason);
                *drops_by_reason.entry(label.to_string()).or_insert(0) += 1;
                if let Some(k) = window_of(frame.at_ns) {
                    let f = flows.get_mut(&rec.flow).unwrap();
                    *grow(&mut f.windows, k).drops.entry(label.to_string()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    })
    .map_err(read)?;

    // Whole windows only.
    let windows = if run_ns > warmup_ns {
        ((run_ns - warmup_ns) / window_ns) as usize
    } else {
        0
    };
    let bounds = |k: usize| (warmup_ns + k as u64 * window_ns, warmup_ns + (k + 1) as u64 * window_ns);

    // ---- per-customer windows ----
    let spec_of: BTreeMap<u32, &AppSpec> = customers.iter().map(|a| (a.id, *a)).collect();
    let mut out_customers: Vec<Customer> = Vec::new();
    for (id, acc) in &mut flows {
        let spec = spec_of[id];
        let mut cw = Vec::with_capacity(windows);
        for k in 0..windows {
            let (lo, hi) = bounds(k);
            let w = grow(&mut acc.windows, k);
            w.latencies_ns.sort_unstable();
            cw.push(CustomerWindow {
                sends: w.sends,
                retransmits: w.retransmits,
                first_time_bytes: w.first_time_bytes,
                deliveries: w.deliveries,
                duplicates: w.duplicates,
                drops: w.drops.clone(),
                p50_ms: percentile_ms(&w.latencies_ns, 50),
                p99_ms: percentile_ms(&w.latencies_ns, 99),
                max_hops: w.max_hops,
                demand_bps: demand_over(spec, lo, hi, load_scale),
                share_bps: None,
                share_ratio: None,
                path_share_bps: None,
                loss_ratio: None,
                within_share: false,
                path: w.path.iter().copied().collect(),
                detour_deliveries: w.detour_deliveries,
            });
        }
        let mut fine = acc.fine.clone();
        fine.resize((run_ns / FINE_BIN_NS) as usize, 0);
        out_customers.push(Customer {
            app: *id,
            preset: format!("{:?}", spec.preset.unwrap()).to_lowercase(),
            law: spec.law.map(|l| format!("{l:?}").to_lowercase()).unwrap_or_else(|| "fixed".into()),
            windows: cw,
            delivered_total: acc.delivered_total,
            first_time_total: acc.first_time_total,
            fine_first_time_bytes: fine,
            burst_starts_ms: burst_starts(spec, run_ns).into_iter().map(|t| t / 1_000_000).collect(),
            shortest_links: acc.shortest_links.map(|s| s as u32),
        });
    }

    // ---- bottlenecks: offered load and overload per window ----
    let mut out_bottlenecks = Vec::new();
    for dir in &bottleneck_dirs {
        let capacity = cap_of.get(dir).copied().unwrap_or(default_cap);
        let mut ws = Vec::with_capacity(windows);
        let acc = bn_windows.get_mut(dir).unwrap();
        for k in 0..windows {
            let (lo, hi) = bounds(k);
            let w = grow(acc, k);
            // Offered: every customer whose packets used this direction
            // in this window (or, if it delivered nothing here, on its
            // last known path) adds its demand. Unlimited counts as the
            // whole capacity: it takes whatever is left.
            let mut offered = 0.0;
            for c in &out_customers {
                let f = &flows[&c.app];
                let path = if f.windows[k].path.is_empty() { &f.last_path } else { &f.windows[k].path };
                if !path.contains(dir) {
                    continue;
                }
                match demand_over(spec_of[&c.app], lo, hi, load_scale) {
                    None => offered += capacity,
                    Some(d) => offered += d,
                }
            }
            ws.push(BottleneckWindow {
                first_time_bytes: w.first_time_bytes,
                duplicate_bytes: w.duplicate_bytes,
                all_bytes: w.all_bytes,
                offered_bps: offered,
                overloaded: offered >= capacity,
            });
        }
        let by_customer = out_customers
            .iter()
            .map(|c| {
                let mut v = bn_by_flow.get(&(*dir, c.app)).cloned().unwrap_or_default();
                v.resize(windows, 0);
                (c.app, v)
            })
            .collect();
        out_bottlenecks.push(Bottleneck {
            from: dir.0,
            to: dir.1,
            capacity_bps: capacity,
            windows: ws,
            by_customer,
        });
    }

    // ---- fairness: max-min shares per window ----
    // The network's paths: every directed inter-switch link gets an index,
    // and every customer a menu of routes from its switch to its peer's.
    let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut all_dirs: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for l in &world.links {
        adj.entry(l.a).or_default().push(l.b);
        adj.entry(l.b).or_default().push(l.a);
        for d in [(l.a, l.b), (l.b, l.a)] {
            let n = all_dirs.len();
            all_dirs.entry(d).or_insert(n);
        }
    }
    let mut all_caps = vec![0.0; all_dirs.len()];
    for (d, i) in &all_dirs {
        all_caps[*i] = cap_of.get(d).copied().unwrap_or(default_cap);
    }
    let switch_of: BTreeMap<u32, u32> = world.apps.iter().map(|a| (a.id, a.switch)).collect();
    let menu_of: BTreeMap<u32, Vec<Vec<usize>>> = customers
        .iter()
        .map(|c| {
            let menu = match (cfg.detour_hops, c.peer.and_then(|p| switch_of.get(&p))) {
                (Some(slack), Some(&dst)) => path_menu(&adj, c.switch, dst, slack as usize)
                    .into_iter()
                    .map(|p| p.iter().map(|d| all_dirs[d]).collect())
                    .collect(),
                _ => Vec::new(),
            };
            (c.id, menu)
        })
        .collect();
    let settling_ns = cfg.settling_ms * 1_000_000;
    let mut bursts: Vec<u64> = customers.iter().flat_map(|a| burst_starts(a, run_ns)).collect();
    bursts.sort_unstable();
    let settled = |k: usize| -> bool {
        let (lo, hi) = bounds(k);
        !bursts.iter().any(|&b| b < hi && lo < b + settling_ns)
    };
    let mut utilization: Vec<Option<f64>> = vec![None; windows];
    for k in 0..windows {
        if !settled(k) {
            continue;
        }
        let (lo, hi) = bounds(k);
        // Directed links in play this window.
        let mut link_index: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        let mut paths: Vec<Vec<usize>> = Vec::new();
        let mut demands: Vec<Option<f64>> = Vec::new();
        let mut who: Vec<usize> = Vec::new();
        for (ci, c) in out_customers.iter().enumerate() {
            let f = &flows[&c.app];
            let path = if f.windows[k].path.is_empty() { &f.last_path } else { &f.windows[k].path };
            if path.is_empty() {
                continue; // never delivered anything: no path to judge on
            }
            let d = demand_over(spec_of[&c.app], lo, hi, load_scale);
            if d == Some(0.0) {
                continue; // silent this window by design
            }
            // A bursty customer is judged only in windows it was sending
            // throughout. Max-min fairness is defined on the demands present
            // while everyone is sending; a window the burst was on for part
            // of has a demand averaged over silence, and the "fair share"
            // that gives is one the customer could not have reached even
            // alone (2026-09-30: every failing window of the reference was
            // such a window). Steady customers always send; unlimited ones
            // have no rate to compare.
            if let (Some(d), Some(peak)) = (d, spec_of[&c.app].rate_mbps) {
                if d < peak * 1_000_000.0 * load_scale * (1.0 - 1e-9) {
                    continue;
                }
            }
            let mut p = Vec::new();
            for l in path {
                let n = link_index.len();
                let idx = *link_index.entry(*l).or_insert(n);
                p.push(idx);
            }
            paths.push(p);
            demands.push(d);
            who.push(ci);
        }
        let mut caps = vec![0.0; link_index.len()];
        for (l, i) in &link_index {
            caps[*i] = cap_of.get(l).copied().unwrap_or(default_cap);
        }
        let path_shares = max_min_shares(&demands, &paths, &caps);
        // The floor's number when the world sets `detour_hops`: the same
        // customers and demands, but each may use any path the network
        // offers. A customer without a menu (no `detour_hops`, or no peer
        // on record) is judged on its path share.
        let menus: Vec<Vec<Vec<usize>>> = who
            .iter()
            .map(|ci| menu_of[&out_customers[*ci].app].clone())
            .collect();
        let net_shares = network_shares(&demands, &menus, &all_caps);
        let (mut got_total, mut share_total) = (0.0f64, 0.0f64);
        for (j, ci) in who.iter().enumerate() {
            let w = &mut out_customers[*ci].windows[k];
            let goodput = (w.first_time_bytes * 8) as f64 / (window_ns as f64 / 1e9);
            let share = if menus[j].is_empty() { path_shares[j] } else { net_shares[j] };
            w.path_share_bps = Some(path_shares[j]);
            w.share_bps = Some(share);
            w.share_ratio = Some(if share > 0.0 { goodput / share } else { 1.0 });
            // Loss and delay are read only where the share covers the
            // whole demand. A customer held below what it asks for fills
            // its own queue and must lose packets -- that is how its
            // sender finds its rate -- and the throughput floor is what
            // judges it.
            if let Some(d) = demands[j] {
                w.within_share = share >= d * (1.0 - 1e-9);
                if w.within_share && w.sends >= MIN_SENDS_FOR_LOSS {
                    let dropped: u64 = w.drops.values().sum();
                    w.loss_ratio = Some((dropped as f64 / w.sends as f64).min(1.0));
                }
            }
            got_total += goodput;
            share_total += share;
        }
        if share_total > 0.0 {
            utilization[k] = Some(got_total / share_total);
        }
    }

    // ---- the floors ----
    let mut floors_out = Vec::new();

    // F1 favorites.
    {
        let mut worst: Option<(f64, u32, usize)> = None;
        let mut evidence = Vec::new();
        let mut judged = 0usize;
        for c in &out_customers {
            for (k, w) in c.windows.iter().enumerate() {
                let Some(ratio) = w.share_ratio else { continue };
                judged += 1;
                if worst.map_or(true, |(r, ..)| ratio < r) {
                    worst = Some((ratio, c.app, k));
                }
                if ratio * 100.0 < cfg.share_pct && evidence.len() < 6 {
                    let goodput = (w.first_time_bytes * 8) as f64 / (window_ns as f64 / 1e9);
                    evidence.push(format!(
                        "app {} ({}) window at {} ms: goodput {:.2} Mbit/s, fair share {:.2} Mbit/s network-wide ({:.2} on the path it took), demand {}",
                        c.app,
                        c.preset,
                        bounds(k).0 / 1_000_000,
                        goodput / 1e6,
                        w.share_bps.unwrap_or(0.0) / 1e6,
                        w.path_share_bps.unwrap_or(0.0) / 1e6,
                        match w.demand_bps {
                            None => "unlimited".to_string(),
                            Some(d) => format!("{:.2} Mbit/s", d / 1e6),
                        }
                    ));
                }
            }
        }
        let (status, detail) = match worst {
            None => (Status::Skipped, "no settled window to judge".to_string()),
            Some((r, app, k)) => (
                if r * 100.0 >= cfg.share_pct { Status::Pass } else { Status::Fail },
                format!(
                    "worst customer got {:.0}% of its fair share (app {app}, window at {} ms), floor {:.0}%, {judged} customer-windows judged",
                    100.0 * r,
                    bounds(k).0 / 1_000_000,
                    cfg.share_pct
                ),
            ),
        };
        floors_out.push(Floor {
            id: "F1",
            name: "per-customer throughput",
            detail,
            status,
            evidence,
        });
    }

    // F2 total throughput.
    {
        let mut worst: Option<(f64, usize)> = None;
        let mut evidence = Vec::new();
        let mut judged = 0usize;
        for (k, u) in utilization.iter().enumerate() {
            let Some(u) = *u else { continue };
            judged += 1;
            if worst.map_or(true, |(r, _)| u < r) {
                worst = Some((u, k));
            }
            if u * 100.0 < cfg.utilization_pct && evidence.len() < 6 {
                evidence.push(format!(
                    "window at {} ms: the network delivered {:.0}% of what a fair allocation delivers",
                    bounds(k).0 / 1_000_000,
                    100.0 * u
                ));
            }
        }
        let (status, detail) = match worst {
            None => (Status::Skipped, "no settled window to judge".to_string()),
            Some((r, k)) => (
                if r * 100.0 >= cfg.utilization_pct { Status::Pass } else { Status::Fail },
                format!(
                    "worst window delivered {:.0}% of the fair allocation (window at {} ms), floor {:.0}%, {judged} windows judged",
                    100.0 * r,
                    bounds(k).0 / 1_000_000,
                    cfg.utilization_pct
                ),
            ),
        };
        floors_out.push(Floor {
            id: "F2",
            name: "total throughput",
            detail,
            status,
            evidence,
        });
    }

    // F3 false link-down.
    {
        let downs: Vec<&Declaration> = declarations.iter().filter(|d| !d.up).collect();
        let evidence: Vec<String> = downs
            .iter()
            .take(8)
            .map(|d| {
                format!(
                    "t={} ms: switch {} declared port {} down; no link failed in this run",
                    d.at_ms, d.switch, d.port
                )
            })
            .collect();
        floors_out.push(Floor {
            id: "F3",
            name: "no false link-down",
            detail: format!(
                "{} link-down declaration{}, {} link event{} in truth",
                downs.len(),
                if downs.len() == 1 { "" } else { "s" },
                link_events,
                if link_events == 1 { "" } else { "s" }
            ),
            status: if downs.is_empty() { Status::Pass } else { Status::Fail },
            evidence,
        });
    }

    // Collapse, as a note: the fairness floor already grades the harm.
    {
        let mut worst: Option<(f64, usize, u32, u32)> = None; // (ratio, window, from, to)
        let mut overload_windows = 0usize;
        let mut evidence = Vec::new();
        for b in &out_bottlenecks {
            for (k, w) in b.windows.iter().enumerate() {
                if !w.overloaded {
                    continue;
                }
                overload_windows += 1;
                let first_bps = (w.first_time_bytes * 8) as f64 / (window_ns as f64 / 1e9);
                let ratio = first_bps / b.capacity_bps;
                if worst.map_or(true, |(r, ..)| ratio < r) {
                    worst = Some((ratio, k, b.from, b.to));
                }
                if ratio * 100.0 < cfg.goodput_pct && evidence.len() < 6 {
                    // Shown under the note when first-time data falls
                    // under the collapse line; never fails the run.
                    let (lo, _) = bounds(k);
                    evidence.push(format!(
                        "link {}->{} window at {} ms: first-time {:.1}% of capacity, duplicates {:.1}%, offered {:.0}%",
                        b.from,
                        b.to,
                        lo / 1_000_000,
                        100.0 * ratio,
                        100.0 * (w.duplicate_bytes * 8) as f64 / (window_ns as f64 / 1e9) / b.capacity_bps,
                        100.0 * w.offered_bps / b.capacity_bps
                    ));
                }
            }
        }
        let (status, detail) = match worst {
            None if out_bottlenecks.is_empty() => (Status::Skipped, "no bottleneck declared".to_string()),
            None => (Status::Skipped, "no window was overloaded".to_string()),
            Some((r, k, from, to)) => {
                (
                    Status::Advisory,
                    format!(
                        "worst overload window: first-time data {:.1}% of link {from}->{to} (window at {} ms), collapse line {:.0}%, {overload_windows} overload window{}",
                        100.0 * r,
                        bounds(k).0 / 1_000_000,
                        cfg.goodput_pct,
                        if overload_windows == 1 { "" } else { "s" }
                    ),
                )
            }
        };
        floors_out.push(Floor {
            id: "--",
            name: "collapse",
            detail,
            status,
            evidence,
        });
    }

    // A1 rules.
    floors_out.push(Floor {
        id: "A1",
        name: "no loops",
        detail: format!("{looping} packets revisited a switch"),
        status: if looping == 0 { Status::Pass } else { Status::Fail },
        evidence: Vec::new(),
    });
    {
        let starved: Vec<&Customer> = out_customers.iter().filter(|c| c.first_time_total == 0).collect();
        floors_out.push(Floor {
            id: "A1",
            name: "no starvation",
            detail: format!(
                "{}/{} customers delivered at least one packet",
                out_customers.len() - starved.len(),
                out_customers.len()
            ),
            status: if starved.is_empty() { Status::Pass } else { Status::Fail },
            evidence: starved
                .iter()
                .map(|c| format!("app {} ({}) delivered nothing all run", c.app, c.preset))
                .collect(),
        });
    }

    let link_capacity: BTreeMap<(u32, u32), f64> = link_load.keys().map(|d| (*d, cap_of.get(d).copied().unwrap_or(default_cap))).collect();

    // ---- time series: buffers, control silence, queue layouts ----
    let flow_ids: Vec<u32> = flows.keys().copied().collect();
    let bins = (run_ns / FINE_BIN_NS) as usize;
    let mut buffers: Vec<SwitchBuffer> = buf_events
        .into_iter()
        .map(|(s, ev)| buffer_series(s, ev, bins, &flow_ids))
        .collect();
    buffers.sort_by_key(|b| std::cmp::Reverse(b.peak_bytes.iter().sum::<u64>()));
    buffers.truncate(BUFFERS_KEPT);

    let mut control = ControlHealth::default();
    let mut worst_overall = 0u64;
    for k in 0..windows {
        let mut gaps: Vec<u64> = ctl_gap.values().filter_map(|v| v.get(k).copied()).filter(|g| *g > 0).collect();
        gaps.sort_unstable();
        control.worst_gap_ms.push(gaps.last().map(|g| *g as f64 / 1e6));
        control.median_gap_ms.push(gaps.get(gaps.len() / 2).map(|g| *g as f64 / 1e6));
        control.control_drops.push(ctl_drops.get(k).copied().unwrap_or(0));
        control.ack_drops.push(ack_drops.get(k).copied().unwrap_or(0));
    }
    for ((switch, port), v) in &ctl_gap {
        let g = v.iter().take(windows).copied().max().unwrap_or(0);
        if g > worst_overall {
            worst_overall = g;
            control.worst_link = Some((port_peer[&(*switch, *port)], *switch));
        }
    }

    let mut queue_layouts: Vec<QueueLayout> = Vec::new();
    for ((switch, port), (discipline, unit_bytes, queues)) in &layouts {
        let dir = port_peer.get(&(*switch, *port)).map(|peer| (*switch, *peer));
        let at = match queue_layouts
            .iter()
            .position(|q| q.discipline == *discipline && q.unit_bytes == *unit_bytes && q.queues == *queues)
        {
            Some(i) => i,
            None => {
                queue_layouts.push(QueueLayout {
                    ports: 0,
                    discipline: *discipline,
                    unit_bytes: *unit_bytes,
                    queues: queues.clone(),
                    bottlenecks: Vec::new(),
                });
                queue_layouts.len() - 1
            }
        };
        queue_layouts[at].ports += 1;
        if let Some(d) = dir.filter(|d| bottleneck_dirs.contains(d)) {
            queue_layouts[at].bottlenecks.push(d);
        }
    }
    queue_layouts.sort_by_key(|q| std::cmp::Reverse(q.ports));

    Ok(Report {
        world: world.world.name.clone(),
        program,
        load_scale,
        run_ms: run_ns / 1_000_000,
        warmup_ms: world.params.warmup_ms,
        window_ms: cfg.window_ms,
        windows,
        floors_cfg: cfg,
        floors: floors_out,
        customers: out_customers,
        bottlenecks: out_bottlenecks,
        declarations,
        link_events,
        looping_packets: looping,
        link_load,
        link_capacity,
        link_by_customer,
        drops_by_reason,
        buffers,
        buffer_pool_packets: world.params.buffer_budget_packets,
        packet_bytes: world.params.packet_size_bytes.unwrap_or_else(|| world.workload.packet_size_bytes()).max(1),
        control,
        queue_layouts,
        utilization,
    })
}

// ---------------------------------------------------------------------------
// The report card
// ---------------------------------------------------------------------------

const CARD_WIDTH: usize = 100;
/// Seconds in one block of a per-second table.
const BLOCK: usize = 20;
/// A link direction appears in the link-load table when it passed this
/// share of its capacity in some window.
const BUSY_LINK_PCT: f64 = 50.0;

fn push(o: &mut String, s: &str) {
    o.push_str(s.trim_end());
    o.push('\n');
}

fn mbps(bytes: u64, window_ms: u64) -> f64 {
    (bytes * 8) as f64 / (window_ms as f64 / 1000.0) / 1e6
}

/// A duration in seconds, without a decimal part when it has none.
fn secs(ms: u64) -> String {
    let s = format!("{:.1}", ms as f64 / 1000.0);
    format!("{} s", s.strip_suffix(".0").unwrap_or(&s))
}

/// `[3, 4, 5, 9]` as `3-5, 9`.
fn ranges(labels: &[u64]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < labels.len() {
        let mut j = i;
        while j + 1 < labels.len() && labels[j + 1] == labels[j] + 1 {
            j += 1;
        }
        out.push(if j == i { format!("{}", labels[i]) } else { format!("{}-{}", labels[i], labels[j]) });
        i = j + 1;
    }
    out.join(", ")
}

/// `lo-hi`, or one number when the two print the same.
fn span(lo: f64, hi: f64, decimals: usize) -> String {
    let a = format!("{lo:.decimals$}");
    let b = format!("{hi:.decimals$}");
    if a == b { a } else { format!("{a}-{b}") }
}

fn law_name(law: &str) -> &str {
    match law {
        "aimd" => "AIMD",
        "ratematch" => "rate match",
        other => other,
    }
}

impl Report {
    /// The name a window carries on the card: the second it starts at.
    /// A run whose windows are not whole seconds falls back to the index.
    fn window_label(&self, k: usize) -> u64 {
        if self.warmup_ms % 1000 == 0 && self.window_ms % 1000 == 0 {
            (self.warmup_ms + k as u64 * self.window_ms) / 1000
        } else {
            k as u64
        }
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let p = &mut o;
        let heavy = "=".repeat(CARD_WIDTH);
        let thin = format!(" {}", "-".repeat(CARD_WIDTH - 2));
        let part = if self.floors_cfg.detour_hops.is_some() { 2 } else { 1 };
        let share_floor = self.floors_cfg.share_pct;
        let total_floor = self.floors_cfg.utilization_pct;
        let label = |k: usize| self.window_label(k);
        let name_of = |app: u32| {
            self.customers
                .iter()
                .find(|c| c.app == app)
                .map_or(String::new(), |c| format!(" ({})", c.preset))
        };

        // ---- header ----
        push(p, &heavy);
        let left = format!(" REPORT CARD   {}   (Assignment 2, Part {part})", self.world);
        let right = format!("RESULT: {}", self.status().label());
        let gap = CARD_WIDTH.saturating_sub(left.chars().count() + right.len() + 1).max(3);
        push(p, &format!("{left}{}{right}", " ".repeat(gap)));
        push(p, &heavy);
        if let Some(prog) = &self.program {
            let file = std::path::Path::new(prog)
                .file_name()
                .map_or(prog.clone(), |f| f.to_string_lossy().into_owned());
            push(p, &format!(" program      {file}"));
        }
        push(p, &format!(" load         x{}", self.load_scale));
        push(
            p,
            &format!(
                " run          {}; graded from {}, in {} windows of {}",
                secs(self.run_ms),
                secs(self.warmup_ms),
                self.windows,
                secs(self.window_ms)
            ),
        );
        push(
            p,
            &format!(
                " not graded   the {} after an Amazon burst begins, and a window with a part of a burst",
                secs(self.floors_cfg.settling_ms)
            ),
        );
        push(
            p,
            &match self.floors_cfg.detour_hops {
                Some(h) => format!(" fair share   over every path up to {h} hops longer than the shortest"),
                None => " fair share   on the path that each customer's packets took".to_string(),
            },
        );
        push(p, " seconds      \"second 3\" is the window that starts 3 s into the run");
        push(p, "");

        // ---- floors ----
        push(p, " FLOORS");
        push(p, &thin);
        push(p, &format!("    {:<26} {:<6} {:>6}   {:<7} {}", "floor", "result", "worst", "needs", "where"));
        for f in &self.floors {
            let (worst, needs, place) = match (f.id, f.name) {
                ("F1", _) => {
                    let mut w: Option<(f64, u32, usize)> = None;
                    for c in &self.customers {
                        for (k, cw) in c.windows.iter().enumerate() {
                            if let Some(r) = cw.share_ratio {
                                if w.map_or(true, |(x, ..)| r < x) {
                                    w = Some((r, c.app, k));
                                }
                            }
                        }
                    }
                    match w {
                        Some((r, app, k)) => (
                            format!("{:.0}%", 100.0 * r),
                            format!(">= {share_floor:.0}%"),
                            format!("app {app}{}, second {}", name_of(app), label(k)),
                        ),
                        None => ("-".into(), format!(">= {share_floor:.0}%"), "no graded window".into()),
                    }
                }
                ("F2", _) => {
                    let mut w: Option<(f64, usize)> = None;
                    for (k, u) in self.utilization.iter().enumerate() {
                        if let Some(u) = *u {
                            if w.map_or(true, |(x, _)| u < x) {
                                w = Some((u, k));
                            }
                        }
                    }
                    match w {
                        Some((r, k)) => (
                            format!("{:.0}%", 100.0 * r),
                            format!(">= {total_floor:.0}%"),
                            format!("second {}", label(k)),
                        ),
                        None => ("-".into(), format!(">= {total_floor:.0}%"), "no graded window".into()),
                    }
                }
                ("F3", _) => {
                    let downs: Vec<&Declaration> = self.declarations.iter().filter(|d| !d.up).collect();
                    (
                        downs.len().to_string(),
                        "0".into(),
                        downs.first().map_or(String::new(), |d| {
                            format!("first: switch {} port {} at {} ms", d.switch, d.port, d.at_ms)
                        }),
                    )
                }
                ("A1", "no loops") => (self.looping_packets.to_string(), "0".into(), String::new()),
                ("A1", _) => {
                    let fed = self.customers.iter().filter(|c| c.first_time_total > 0).count();
                    (format!("{fed}/{}", self.customers.len()), "all".into(), String::new())
                }
                _ => {
                    // The collapse note: first-time data on a declared
                    // bottleneck in its worst overloaded window.
                    let mut w: Option<(f64, usize, u32, u32)> = None;
                    for b in &self.bottlenecks {
                        for (k, bw) in b.windows.iter().enumerate() {
                            if !bw.overloaded {
                                continue;
                            }
                            let r = mbps(bw.first_time_bytes, self.window_ms) * 1e6 / b.capacity_bps;
                            if w.map_or(true, |(x, ..)| r < x) {
                                w = Some((r, k, b.from, b.to));
                            }
                        }
                    }
                    match w {
                        Some((r, k, from, to)) => (
                            format!("{:.0}%", 100.0 * r),
                            "-".into(),
                            format!("first-time data on link {from}->{to}, second {} (not graded)", label(k)),
                        ),
                        None => ("-".into(), "-".into(), "no bottleneck was overloaded".into()),
                    }
                }
            };
            push(
                p,
                &format!(" {:<2} {:<26} {:<6} {:>6}   {:<7} {}", f.id, f.name, f.status.label(), worst, needs, place),
            );
        }
        push(p, &thin);
        push(p, &format!(" OVERALL  {}", self.status().label()));
        push(p, "");

        // ---- customers ----
        push(p, " CUSTOMERS   (whole run; goodput is first-time data)");
        push(p, &thin);
        push(
            p,
            "   app  customer     rule           asks  goodput  worst     sent  resent     p50     p99    loss",
        );
        push(
            p,
            "                                 Mbit/s   Mbit/s  share     pkts    pkts      ms      ms",
        );
        for c in &self.customers {
            let sends: u64 = c.windows.iter().map(|w| w.sends).sum();
            let retx: u64 = c.windows.iter().map(|w| w.retransmits).sum();
            let first: u64 = c.windows.iter().map(|w| w.first_time_bytes).sum();
            let goodput = if self.windows > 0 {
                mbps(first, self.window_ms * self.windows as u64)
            } else {
                0.0
            };
            let fold_max = |it: &mut dyn Iterator<Item = f64>| it.fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))));
            let asks = fold_max(&mut c.windows.iter().filter_map(|w| w.demand_bps));
            let p50 = fold_max(&mut c.windows.iter().filter_map(|w| w.p50_ms));
            let p99 = fold_max(&mut c.windows.iter().filter_map(|w| w.p99_ms));
            let max_loss = fold_max(&mut c.windows.iter().filter_map(|w| w.loss_ratio));
            let min_share = c
                .windows
                .iter()
                .filter_map(|w| w.share_ratio)
                .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.min(v))));
            push(
                p,
                &format!(
                    "   {:<4} {:<12} {:<10} {:>8} {:>8.2} {:>6} {:>8} {:>7} {:>7} {:>7} {:>7}",
                    c.app,
                    c.preset,
                    law_name(&c.law),
                    asks.map_or("no limit".into(), |v| format!("{:.2}", v / 1e6)),
                    goodput,
                    min_share.map_or("-".into(), |v| format!("{:.0}%", 100.0 * v)),
                    sends,
                    retx,
                    p50.map_or("-".into(), |v| format!("{v:.1}")),
                    p99.map_or("-".into(), |v| format!("{v:.1}")),
                    max_loss.map_or("-".into(), |v| format!("{:.1}%", 100.0 * v)),
                ),
            );
        }
        push(p, &thin);
        push(p, "   asks         the largest rate the customer asks for in a window, at this load");
        push(p, &format!("   worst share  the worst graded window, as % of the fair share (F1 needs {share_floor:.0}%)"));
        push(p, "   p50, p99     one-way delay in the worst window; not graded");
        push(p, "   loss         the worst graded window; not graded; \"-\" = the customer is held below");
        push(p, "                what it asks for, so loss is expected");
        push(p, "");

        // ---- fair share by second ----
        if self.windows > 0 && !self.customers.is_empty() {
            push(
                p,
                &format!(" FAIR SHARE BY SECOND   (% of fair share;  . = not graded;  ! = under the {share_floor:.0}% floor)"),
            );
            push(p, &thin);
            let mut start = 0;
            while start < self.windows {
                let end = (start + BLOCK).min(self.windows);
                let header: String = (start..end).map(|k| format!("{:>3} ", label(k))).collect();
                push(p, &format!("   second {header}"));
                for c in &self.customers {
                    let cells: String = (start..end)
                        .map(|k| match c.windows.get(k).and_then(|w| w.share_ratio) {
                            Some(r) => format!(
                                "{:>3.0}{}",
                                100.0 * r.min(9.99),
                                if 100.0 * r < share_floor { "!" } else { " " }
                            ),
                            None => format!("{:>3} ", "."),
                        })
                        .collect();
                    push(p, &format!("   {:<6} {cells}", c.app));
                }
                let total: String = (start..end)
                    .map(|k| match self.utilization.get(k).copied().flatten() {
                        Some(u) => format!(
                            "{:>3.0}{}",
                            100.0 * u.min(9.99),
                            if 100.0 * u < total_floor { "!" } else { " " }
                        ),
                        None => format!("{:>3} ", "."),
                    })
                    .collect();
                push(p, &format!("   {:<6} {total}", "total"));
                push(p, "");
                start = end;
            }
            push(p, &format!("   total = all graded customers together, as % of the fair total (F2 needs {total_floor:.0}%)"));
            push(p, "");
        }

        // ---- bottlenecks ----
        if !self.bottlenecks.is_empty() && self.windows > 0 {
            push(p, " BOTTLENECKS   (the links that the world names; % of capacity)");
            push(p, &thin);
            push(p, "   link      capacity   overloaded   first-time data    duplicates   all traffic");
            push(p, "                                       mean    worst          mean          mean");
            for b in &self.bottlenecks {
                let cap = b.capacity_bps / 1e6;
                let n = b.windows.len().max(1) as f64;
                let pct = |bytes: u64| 100.0 * mbps(bytes, self.window_ms) / cap;
                let over = b.windows.iter().filter(|w| w.overloaded).count();
                let first_mean = b.windows.iter().map(|w| pct(w.first_time_bytes)).sum::<f64>() / n;
                let dup_mean = b.windows.iter().map(|w| pct(w.duplicate_bytes)).sum::<f64>() / n;
                let all_mean = b.windows.iter().map(|w| pct(w.all_bytes)).sum::<f64>() / n;
                let first_worst = b
                    .windows
                    .iter()
                    .filter(|w| w.overloaded)
                    .map(|w| pct(w.first_time_bytes))
                    .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.min(v))));
                push(
                    p,
                    &format!(
                        "   {:<9} {:>4.0} Mb/s   {:>3} of {:<3}   {:>6.0}%  {:>6}   {:>10.0}%   {:>10.0}%",
                        format!("{}->{}", b.from, b.to),
                        cap,
                        over,
                        b.windows.len(),
                        first_mean,
                        first_worst.map_or("-".into(), |v| format!("{v:.0}%")),
                        dup_mean,
                        all_mean
                    ),
                );
            }
            push(p, "   overloaded = windows in which the customers asked for more than the link carries;");
            push(p, "   worst = the lowest first-time data in an overloaded window");
            push(p, "");
        }

        // ---- link load ----
        if !self.link_load.is_empty() && self.windows > 0 {
            let bottleneck_dirs: BTreeSet<(u32, u32)> = self.bottlenecks.iter().map(|b| (b.from, b.to)).collect();
            let mut rows: Vec<((u32, u32), f64, Vec<f64>)> = Vec::new();
            let mut quiet = 0usize;
            for (dir, loads) in &self.link_load {
                let cap = self.link_capacity.get(dir).copied().unwrap_or(1.0) / 1e6;
                let pcts: Vec<f64> = (0..self.windows)
                    .map(|k| 100.0 * mbps(loads.get(k).copied().unwrap_or(0), self.window_ms) / cap)
                    .collect();
                let peak = pcts.iter().copied().fold(0.0, f64::max);
                if peak >= BUSY_LINK_PCT || bottleneck_dirs.contains(dir) {
                    rows.push((*dir, cap, pcts));
                } else {
                    quiet += 1;
                }
            }
            rows.sort_by(|a, b| {
                let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len().max(1) as f64;
                mean(&b.2).partial_cmp(&mean(&a.2)).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
            });
            push(p, " LINK LOAD BY SECOND   (all traffic, % of capacity;  * = a bottleneck that the world names)");
            push(p, &thin);
            let mut start = 0;
            while start < self.windows && !rows.is_empty() {
                let end = (start + BLOCK).min(self.windows);
                let header: String = (start..end).map(|k| format!("{:>4}", label(k))).collect();
                push(p, &format!("   {:<15}{header}", "second"));
                for (dir, cap, pcts) in &rows {
                    let cells: String = pcts[start..end].iter().map(|v| format!("{:>4.0}", v.min(999.0))).collect();
                    push(
                        p,
                        &format!(
                            "   {:<6}{}{:>3.0} Mb/s{cells}",
                            format!("{}->{}", dir.0, dir.1),
                            if bottleneck_dirs.contains(dir) { "*" } else { " " },
                            cap
                        ),
                    );
                }
                push(p, "");
                start = end;
            }
            push(
                p,
                &format!(
                    "   {quiet} other link direction{} never passed {BUSY_LINK_PCT:.0}% (all are in the JSON)",
                    if quiet == 1 { "" } else { "s" }
                ),
            );
            push(p, "");
        }

        if !self.drops_by_reason.is_empty() {
            push(p, " WHERE CUSTOMER PACKETS STOPPED");
            push(p, &thin);
            let mut reasons: Vec<(&String, &u64)> = self.drops_by_reason.iter().collect();
            reasons.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (r, n) in reasons {
                push(p, &format!("   {n:>8}  {r}"));
            }
            push(p, "");
        }

        if !self.declarations.is_empty() {
            push(p, " BELIEF vs TRUTH");
            push(p, &thin);
            for d in self.declarations.iter().take(12) {
                push(
                    p,
                    &format!(
                        "   t={:>7} ms  switch {:>2} port {:>3}: declared {:<4}  truth: up",
                        d.at_ms,
                        d.switch,
                        d.port,
                        if d.up { "up" } else { "DOWN" }
                    ),
                );
            }
            if self.declarations.len() > 12 {
                push(p, &format!("   ... and {} more", self.declarations.len() - 12));
            }
            push(p, "");
        }

        // ---- what went wrong ----
        let failed: Vec<&Floor> = self.floors.iter().filter(|f| f.status == Status::Fail).collect();
        if !failed.is_empty() {
            push(p, " WHAT WENT WRONG");
            push(p, &thin);
            for f in failed {
                push(p, &format!("   {} {}", f.id, f.name));
                match f.id {
                    "F1" => self.explain_share(p),
                    "F2" => {
                        let mut at: Vec<u64> = Vec::new();
                        let (mut lo, mut hi) = (f64::MAX, 0.0f64);
                        for (k, u) in self.utilization.iter().enumerate() {
                            if let Some(u) = *u {
                                if 100.0 * u < total_floor {
                                    at.push(label(k));
                                    lo = lo.min(100.0 * u);
                                    hi = hi.max(100.0 * u);
                                }
                            }
                        }
                        push(
                            p,
                            &format!(
                                "      {} window{} under the floor: second{} {}",
                                at.len(),
                                if at.len() == 1 { "" } else { "s" },
                                if at.len() == 1 { "" } else { "s" },
                                ranges(&at)
                            ),
                        );
                        push(
                            p,
                            &format!(
                                "         all customers together got {}% of the fair total (needs {total_floor:.0}%)",
                                span(lo, hi, 0)
                            ),
                        );
                    }
                    _ => {
                        push(p, &format!("      {}", f.detail));
                        for e in &f.evidence {
                            push(p, &format!("      {e}"));
                        }
                    }
                }
            }
            push(p, "");
        }

        push(p, " More data:  --json FILE   every number above for each window, and buffer use");
        push(p, "             --log FILE    every packet event; open it in the viewer");
        push(p, &heavy);
        o
    }

    /// The F1 part of "what went wrong": for each customer under the
    /// floor, the windows, what it got, and who on its path got more
    /// than a fair share in those windows.
    fn explain_share(&self, p: &mut String) {
        let floor = self.floors_cfg.share_pct;
        struct Miss {
            app: u32,
            preset: String,
            at: Vec<usize>,
            lo: f64,
            hi: f64,
            worst: usize,
        }
        let mut misses: Vec<Miss> = Vec::new();
        for c in &self.customers {
            let mut m = Miss { app: c.app, preset: c.preset.clone(), at: Vec::new(), lo: f64::MAX, hi: 0.0, worst: 0 };
            for (k, w) in c.windows.iter().enumerate() {
                let Some(r) = w.share_ratio else { continue };
                if 100.0 * r < floor {
                    if r < m.lo {
                        m.lo = r;
                        m.worst = k;
                    }
                    m.hi = m.hi.max(r);
                    m.at.push(k);
                }
            }
            if !m.at.is_empty() {
                misses.push(m);
            }
        }
        misses.sort_by(|a, b| a.lo.partial_cmp(&b.lo).unwrap_or(std::cmp::Ordering::Equal).then(a.app.cmp(&b.app)));
        for m in &misses {
            let c = self.customers.iter().find(|c| c.app == m.app).expect("a customer that missed");
            let labels: Vec<u64> = m.at.iter().map(|k| self.window_label(*k)).collect();
            push(
                p,
                &format!(
                    "      app {} ({}): {} window{} under the floor: second{} {}",
                    m.app,
                    m.preset,
                    m.at.len(),
                    if m.at.len() == 1 { "" } else { "s" },
                    if m.at.len() == 1 { "" } else { "s" },
                    ranges(&labels)
                ),
            );
            let w = &c.windows[m.worst];
            push(
                p,
                &format!(
                    "         got {}% of its fair share (needs {floor:.0}%); worst in second {}: {:.2} of {:.2} Mbit/s",
                    span(100.0 * m.lo, 100.0 * m.hi, 0),
                    self.window_label(m.worst),
                    mbps(w.first_time_bytes, self.window_ms),
                    w.share_bps.unwrap_or(0.0) / 1e6
                ),
            );
            // Part 2: the path it was on could not carry the share that
            // another path could.
            if self.floors_cfg.detour_hops.is_some() {
                if let (Some(net), Some(path)) = (w.share_bps, w.path_share_bps) {
                    if path < 0.9 * net {
                        push(
                            p,
                            &format!(
                                "         -> on the path it took, its fair share was {:.2} Mbit/s; another path gives {:.2}",
                                path / 1e6,
                                net / 1e6
                            ),
                        );
                    }
                }
            }
            // Who shared a link with it in those windows and got more
            // than a fair share.
            let mut over: BTreeMap<u32, f64> = BTreeMap::new();
            for k in &m.at {
                let mine: BTreeSet<(u32, u32)> = c.windows[*k].path.iter().copied().collect();
                for other in &self.customers {
                    if other.app == c.app {
                        continue;
                    }
                    let Some(ow) = other.windows.get(*k) else { continue };
                    let Some(r) = ow.share_ratio else { continue };
                    if r > 1.1 && ow.path.iter().any(|l| mine.contains(l)) {
                        let e = over.entry(other.app).or_insert(0.0);
                        *e = e.max(r);
                    }
                }
            }
            for (app, r) in over {
                let preset = self.customers.iter().find(|x| x.app == app).map_or("", |x| x.preset.as_str());
                push(
                    p,
                    &format!(
                        "         -> it shares a link with app {app} ({preset}), which got up to {:.0}% of its fair share",
                        100.0 * r
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn water_filling_splits_a_shared_link_and_caps_at_demand() {
        // Three flows on one 10 Mb/s link: demands 2, unlimited, unlimited.
        let shares = max_min_shares(&[Some(2.0), None, None], &[vec![0], vec![0], vec![0]], &[10.0]);
        assert_eq!(shares, vec![2.0, 4.0, 4.0]);
    }

    #[test]
    fn water_filling_over_two_links() {
        // Flow 0 uses links 0 and 1; flow 1 uses link 0 only; flow 2 uses
        // link 1 only. Caps 10 and 4. Flow 0 is capped by link 1 at 2,
        // flow 2 gets the other 2, flow 1 gets the rest of link 0.
        let shares = max_min_shares(&[None, None, None], &[vec![0, 1], vec![0], vec![1]], &[10.0, 4.0]);
        assert!((shares[0] - 2.0).abs() < 1e-6, "{shares:?}");
        assert!((shares[1] - 8.0).abs() < 1e-6, "{shares:?}");
        assert!((shares[2] - 2.0).abs() < 1e-6, "{shares:?}");
    }

    #[test]
    fn water_filling_small_customer_gets_all_it_asks() {
        let shares = max_min_shares(
            &[Some(0.2), Some(4.0), None],
            &[vec![0], vec![0], vec![0]],
            &[10.0],
        );
        assert!((shares[0] - 0.2).abs() < 1e-6);
        assert!((shares[1] - 4.0).abs() < 1e-6);
        assert!((shares[2] - 5.8).abs() < 1e-6, "{shares:?}");
    }

    fn adj(edges: &[(u32, u32)]) -> BTreeMap<u32, Vec<u32>> {
        let mut m: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &(a, b) in edges {
            m.entry(a).or_default().push(b);
            m.entry(b).or_default().push(a);
        }
        m
    }

    #[test]
    fn path_menu_lists_routes_up_to_the_slack() {
        // A 4-ring 0-1-2-3-0 plus a chord 1-3. From 0 to 2: two shortest
        // (0-1-2, 0-3-2), and with slack 2 the chord routes 0-1-3-2 and
        // 0-3-1-2 as well. Nothing longer exists.
        let a = adj(&[(0, 1), (1, 2), (2, 3), (3, 0), (1, 3)]);
        let m = path_menu(&a, 0, 2, 0);
        assert_eq!(m, vec![vec![(0, 1), (1, 2)], vec![(0, 3), (3, 2)]]);
        let m = path_menu(&a, 0, 2, 2);
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!(m[2], vec![(0, 1), (1, 3), (3, 2)]);
        assert_eq!(path_menu(&a, 2, 2, 2), vec![Vec::<(u32, u32)>::new()]);
        assert!(path_menu(&a, 0, 9, 2).is_empty());
    }

    #[test]
    fn network_share_finds_the_detour_the_program_did_not() {
        // Two unlimited flows from A to B. Path 0 (link 0) and path 1
        // (link 1), each 10. Routed together down link 0 they would share
        // 5 each; the network offers 10 each.
        let menus = vec![vec![vec![0], vec![1]], vec![vec![0], vec![1]]];
        let s = network_shares(&[None, None], &menus, &[10.0, 10.0]);
        assert!((s[0] - 10.0).abs() < 1e-6 && (s[1] - 10.0).abs() < 1e-6, "{s:?}");
        // On the path they took, the old number is 5 each.
        let p = max_min_shares(&[None, None], &[vec![0], vec![0]], &[10.0, 10.0]);
        assert_eq!(p, vec![5.0, 5.0]);
    }

    #[test]
    fn network_share_caps_at_demand_and_shares_the_common_link() {
        // Flow 0 wants 2 and can only use link 0 (cap 10). Flow 1 is
        // unlimited and may use link 0 or link 1 (cap 4). Flow 2 is
        // unlimited on link 1 only. Flows 0 and 2 have one choice and are
        // placed first; flow 1 then sees link 0 at 10/2 and link 1 at
        // 4/2 and takes link 0. Water filling: 2, 8, 4.
        let menus = vec![vec![vec![0]], vec![vec![0], vec![1]], vec![vec![1]]];
        let s = network_shares(&[Some(2.0), None, None], &menus, &[10.0, 4.0]);
        assert!((s[0] - 2.0).abs() < 1e-6, "{s:?}");
        assert!((s[1] - 8.0).abs() < 1e-6, "{s:?}");
        assert!((s[2] - 4.0).abs() < 1e-6, "{s:?}");
    }

    #[test]
    fn network_share_gives_each_flow_one_path_and_places_the_stuck_first() {
        // Flow 0 may use link 0 (cap 4) or link 1 (cap 4); flow 1 is stuck
        // on link 0. Flow 1 is placed first; flow 0 then takes the empty
        // link 1. Each has a link to itself: 4 and 4 -- not the 6 a flow
        // spread over both links could take, because no router does that.
        let menus = vec![vec![vec![0], vec![1]], vec![vec![0]]];
        let s = network_shares(&[None, None], &menus, &[4.0, 4.0]);
        assert_eq!(assign_paths(&[None, None], &menus, &[4.0, 4.0]), vec![vec![1], vec![0]]);
        assert!((s[0] - 4.0).abs() < 1e-6, "{s:?}");
        assert!((s[1] - 4.0).abs() < 1e-6, "{s:?}");
        // No path at all: bounded by demand only.
        let s = network_shares(&[Some(3.0), None], &[vec![], vec![vec![0]]], &[4.0]);
        assert_eq!(s, vec![3.0, 4.0]);
        // Among equal choices the first (shortest) path wins.
        let menus = vec![vec![vec![0], vec![1, 2]]];
        assert_eq!(assign_paths(&[None], &menus, &[4.0, 4.0, 4.0]), vec![vec![0]]);
    }

    #[test]
    fn amazon_demand_is_the_on_fraction_of_the_window() {
        let spec = AppSpec {
            preset: Some(Preset::Amazon),
            rate_mbps: Some(8.0),
            on_ms: Some(200),
            off_ms: Some(300),
            start_ms: Some(100),
            ..AppSpec::new(1, 0, "10.0.0.0/24".into())
        };
        let ms = 1_000_000u64;
        // Window [0, 1000): on during [100,300) and [600,800) = 400 ms.
        let d = demand_over(&spec, 0, 1000 * ms, 1.0).unwrap();
        assert!((d - 8e6 * 0.4).abs() < 1.0, "{d}");
        // Window [300, 600): silent.
        assert_eq!(demand_over(&spec, 300 * ms, 600 * ms, 1.0), Some(0.0));
        // Load scale doubles it.
        let d2 = demand_over(&spec, 0, 1000 * ms, 2.0).unwrap();
        assert!((d2 - 2.0 * d).abs() < 1.0);
        assert_eq!(burst_starts(&spec, 1200 * ms), vec![100 * ms, 600 * ms, 1100 * ms]);
    }
}
