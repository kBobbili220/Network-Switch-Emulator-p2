//! Component 5 of the Rig: the scorer and the report card.
//!
//! Everything before this component makes a run happen. This one decides
//! what the run was worth, and — the part that matters more — tells the
//! student *why*.
//!
//! # What counts
//!
//! The scorer reads a `.simlog` written by the simulator, not anything the
//! student's program produced. The program cannot write to it.
//!
//! A **workload packet** is a `Data` packet whose source and destination
//! addresses are both app addresses declared by the world file. Control
//! traffic a program invents, whatever form it takes, is addressed to
//! something else and never lands in the count. A program
//! that forges packets between two real app addresses would inflate the
//! numerator and the denominator together; the integrity check at the
//! bottom of the report card compares the observed send count against the
//! count the workload spec implies, so that shows up as a warning.
//!
//! **Sent** is the first frame that mentions a packet id. For an
//! app-originated packet that is its ingress at its own edge switch, or —
//! if the access link is down — its drop. **Delivered** is a
//! `PacketDelivered` frame naming the app that owns the destination
//! address. A packet delivered to the wrong app is a misdelivery, not a
//! delivery.
//!
//! # The window
//!
//! Packets sent before `warmup_ms` are ignored: a program is allowed to
//! not know the topology yet. Packets sent in the last `tail_guard_ms` are
//! ignored too, because the run stopped and anything still in flight was
//! never going to arrive. Neither is a property of the solution.
//!
//! # Recovery
//!
//! Whole-run delivery percentage is a weak criterion — the same program on
//! the same world with the same failures scores 99.43% over 26 s and
//! 99.73% over 60 s, because the extra clean time dilutes the loss. The
//! criterion that *is* a property of the solution is per-event: after each
//! link goes down, how long until traffic flows again. Every undelivered
//! packet is attributed to the most recent link event at or before it was
//! sent; the recovery time for that event is how long the losses kept
//! coming.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::failures::Schedule;
use crate::sim_log::{DropReasonW, LogEvent, NodeRefW};
use crate::world::{Prefix, World, WorldError};

/// Grace period at the start of a run. Packets sent inside it are not
/// scored, so a program is not punished for not knowing the topology yet.
/// Worlds may override it; this is the fallback.
pub const DEFAULT_WARMUP_MS: u64 = 200;

/// Nothing in these worlds stays in flight for more than a few hundred
/// microseconds. A tenth of a second of slack at the end of the run is
/// generous and keeps the last tick from being scored as loss.
pub const DEFAULT_TAIL_GUARD_MS: u64 = 100;

/// A floor, not a target. The per-event recovery criterion carries the
/// weight; this one only catches a program that is broadly broken.
pub const DEFAULT_DELIVERY_FLOOR: f64 = 0.95;

/// A packet sent this soon before a link event was still in flight when
/// the link died, so its loss belongs to that event and not to the quiet
/// stretch before it.
const IN_FLIGHT_GRACE_MS: u64 = 5;

/// How far the observed send count may drift from the workload spec
/// before the report card says something.
const INTEGRITY_TOLERANCE: f64 = 0.05;

/// Loop detection uses one bit per switch. Every generated world is at
/// most 15 switches; a hand-written world with a higher switch id falls
/// back to TTL drops alone, and the report card says so.
const MAX_TRACKED_SWITCH: u32 = 63;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScoreParams {
    pub warmup_ms: u64,
    pub tail_guard_ms: u64,
    pub delivery_floor: f64,
    pub recovery_budget_ms: u64,
}

impl Default for ScoreParams {
    fn default() -> Self {
        Self {
            warmup_ms: DEFAULT_WARMUP_MS,
            tail_guard_ms: DEFAULT_TAIL_GUARD_MS,
            delivery_floor: DEFAULT_DELIVERY_FLOOR,
            recovery_budget_ms: crate::failures::DEFAULT_RECOVERY_BUDGET_MS,
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// The run did not exercise this criterion. A world with no failure
    /// schedule cannot be judged on recovery.
    Skipped,
    /// Measured and reported, but it does not decide the grade. A1 does
    /// not grade capacity, so a loop is a warning here and a cost later.
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
pub struct Criterion {
    pub id: &'static str,
    pub name: &'static str,
    /// The measurement, in the form the student should argue with.
    pub detail: String,
    pub status: Status,
}

/// One link going down or coming back, and what the program did about it.
#[derive(Debug, Clone)]
pub struct LinkEvent {
    pub at_ms: u64,
    pub link: (u32, u32),
    pub down: bool,
    /// When loss started, relative to the event.
    pub silence_began_ms: Option<u64>,
    /// When loss stopped, relative to the event. This is the recovery
    /// time: the last packet the program lost before it found a way
    /// around.
    pub recovered_ms: Option<u64>,
    /// How many packets it cost.
    pub cost: u64,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct PairStat {
    pub src: u32,
    pub dst: u32,
    pub sent: u64,
    pub delivered: u64,
}

/// A worked example the report card shows instead of asserting something
/// abstract happened.
#[derive(Debug, Clone)]
pub struct PacketTrace {
    pub id: u64,
    pub src_app: u32,
    pub dst_app: u32,
    pub sent_ms: u64,
    /// `(time in ms, switch id)`, in order.
    pub hops: Vec<(u64, u32)>,
    pub outcome: String,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub world: String,
    pub family: String,
    pub switches: usize,
    pub links: usize,
    pub apps: usize,
    pub diameter: u32,
    pub schedule: Option<String>,
    pub program: Option<String>,

    pub run_ms: u64,
    pub window_ms: (u64, u64),

    pub sent: u64,
    pub delivered: u64,
    pub misdelivered: u64,
    pub delivery_rate: f64,

    pub looping_packets: u64,
    pub ttl_drops: u64,
    pub loop_detection_exact: bool,

    pub pairs: Vec<PairStat>,
    pub unreachable: Vec<PairStat>,

    pub events: Vec<LinkEvent>,
    pub baseline_loss: u64,

    pub drops_by_reason: BTreeMap<String, u64>,

    pub criteria: Vec<Criterion>,
    pub integrity: Vec<String>,

    pub example_loop: Option<PacketTrace>,
    pub example_loss: Option<PacketTrace>,
}

impl Report {
    pub fn status(&self) -> Status {
        if self.criteria.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else {
            Status::Pass
        }
    }

    /// The first criterion that failed, which is the one worth reading.
    pub fn first_failure(&self) -> Option<&Criterion> {
        self.criteria.iter().find(|c| c.status == Status::Fail)
    }
}

// ---------------------------------------------------------------------------
// Per-packet state, accumulated in one streaming pass
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pkt {
    src_app: u32,
    dst_app: u32,
    first_ns: u64,
    delivered: bool,
    misdelivered: bool,
    looped: bool,
    visited: u64,
    hops: u32,
    last_switch: u32,
    drop: Option<&'static str>,
    punted: Option<&'static str>,
}

/// A punt is not a drop, but for a data packet it is a dead end unless
/// the program does something with it. When a packet's last recorded
/// event is a punt, that is where it stopped.
fn punt_name(code: u32) -> &'static str {
    match code {
        0 => "punted: no route",
        1 => "punted: TTL expired",
        _ => "punted: program-defined reason",
    }
}

fn drop_name(r: &DropReasonW) -> &'static str {
    match r {
        DropReasonW::Ttl => "ttl expired",
        DropReasonW::NoRoute => "no route",
        DropReasonW::NoEgressLink => "no egress link",
        DropReasonW::LinkDrop => "link down",
        DropReasonW::SwitchFailed => "switch failed",
        DropReasonW::Recirculation => "recirculation limit",
        DropReasonW::QueueFull => "queue full",
        DropReasonW::Policy => "dropped by program",
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Score a `.simlog` against the world it was produced from.
///
/// `schedule` is optional. Without it the recovery criterion is skipped
/// and the run is judged as Part 1.
pub fn score_file(
    log: &Path,
    world: &World,
    schedule: Option<&Schedule>,
    program: Option<String>,
    p: &ScoreParams,
) -> Result<Report, WorldError> {
    // App address -> app id. The world file is the ground truth for who
    // is allowed to be a source or a destination.
    let mut addr_app: HashMap<u32, u32> = HashMap::new();
    for a in &world.apps {
        let pfx = Prefix::parse(&a.prefix)?;
        addr_app.insert(pfx.first_host(), a.id);
    }

    let mut pkts: HashMap<u64, Pkt> = HashMap::new();
    let mut link_ends: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut raw_events: Vec<(u64, u32, bool)> = Vec::new();
    let mut run_ns: u64 = 0;
    let mut exact_loops = true;

    let read =
        |e: crate::sim_log::writer::ReadError| WorldError::Invalid(format!("reading {}: {e:?}", log.display()));

    crate::sim_log::writer::for_each_frame(log, |frame| {
        run_ns = run_ns.max(frame.at_ns);
        match frame.event {
            LogEvent::LinkAdded { id, a, b, .. } => {
                if let (NodeRefW::Switch(x), NodeRefW::Switch(y)) = (a, b) {
                    link_ends.insert(id, (x.min(y), x.max(y)));
                }
            }
            LogEvent::LinkFailed { id } => raw_events.push((frame.at_ns, id, true)),
            LogEvent::LinkRestored { id } => raw_events.push((frame.at_ns, id, false)),

            LogEvent::PacketIngress {
                switch,
                packet: snap,
                ..
            } => {
                // Data only, and only between two declared app addresses.
                if snap.kind != 0 {
                    return;
                }
                let (src, dst) = match (addr_app.get(&snap.ip_src), addr_app.get(&snap.ip_dst)) {
                    (Some(s), Some(d)) if s != d => (*s, *d),
                    _ => return,
                };
                if switch > MAX_TRACKED_SWITCH {
                    exact_loops = false;
                }
                let bit = if switch <= MAX_TRACKED_SWITCH {
                    1u64 << switch
                } else {
                    0
                };
                let e = pkts.entry(snap.id).or_insert_with(|| Pkt {
                    src_app: src,
                    dst_app: dst,
                    first_ns: frame.at_ns,
                    delivered: false,
                    misdelivered: false,
                    looped: false,
                    visited: 0,
                    hops: 0,
                    last_switch: switch,
                    drop: None,
                    punted: None,
                });
                if bit != 0 && e.visited & bit != 0 {
                    e.looped = true;
                }
                e.visited |= bit;
                e.hops += 1;
                e.last_switch = switch;
            }

            LogEvent::PacketDelivered { app, packet_id } => {
                if let Some(e) = pkts.get_mut(&packet_id) {
                    if app == e.dst_app {
                        e.delivered = true;
                    } else {
                        e.misdelivered = true;
                    }
                }
            }

            LogEvent::PacketPunted {
                switch,
                reason,
                packet_id,
            } => {
                if let Some(e) = pkts.get_mut(&packet_id) {
                    e.punted = Some(punt_name(reason));
                    e.last_switch = switch;
                }
            }

            LogEvent::PacketDropped {
                at,
                reason,
                packet_id,
            } => {
                if let Some(e) = pkts.get_mut(&packet_id) {
                    e.drop = Some(drop_name(&reason));
                    if let NodeRefW::Switch(s) = at {
                        e.last_switch = s;
                    }
                }
            }
            _ => {}
        }
    })
    .map_err(read)?;

    let run_ms = run_ns / 1_000_000;
    let lo = p.warmup_ms;
    let hi = run_ms.saturating_sub(p.tail_guard_ms);

    // ---- link events, in time order, from the log itself ----
    raw_events.sort_by_key(|(t, _, _)| *t);
    let mut events: Vec<LinkEvent> = raw_events
        .iter()
        .filter_map(|(ns, id, down)| {
            link_ends.get(id).map(|link| LinkEvent {
                at_ms: ns / 1_000_000,
                link: *link,
                down: *down,
                silence_began_ms: None,
                recovered_ms: None,
                cost: 0,
                ok: true,
            })
        })
        .collect();

    // ---- roll the packets up ----
    let mut sent = 0u64;
    let mut delivered = 0u64;
    let mut misdelivered = 0u64;
    let mut looping = 0u64;
    let mut ttl_drops = 0u64;
    let mut pair: BTreeMap<(u32, u32), (u64, u64)> = BTreeMap::new();
    let mut drops: BTreeMap<String, u64> = BTreeMap::new();
    let mut baseline_loss = 0u64;
    // Per event: (count, first loss ms, last loss ms) in absolute time.
    let mut ev_loss: Vec<(u64, u64, u64)> = vec![(0, u64::MAX, 0); events.len()];
    let mut loop_ids: Vec<(u64, u64)> = Vec::new();
    let mut loss_ids: Vec<(u64, u64)> = Vec::new();

    for (id, k) in &pkts {
        let at = k.first_ns / 1_000_000;
        if at < lo || at >= hi {
            continue;
        }
        sent += 1;
        let e = pair.entry((k.src_app, k.dst_app)).or_insert((0, 0));
        e.0 += 1;
        if k.delivered {
            delivered += 1;
            e.1 += 1;
        }
        if k.misdelivered && !k.delivered {
            misdelivered += 1;
        }
        if k.looped {
            looping += 1;
            if loop_ids.len() < 4 {
                loop_ids.push((*id, at));
            }
        }
        match (k.drop, k.punted, k.delivered) {
            (Some(r), _, _) => {
                *drops.entry(r.to_string()).or_insert(0) += 1;
                if r == "ttl expired" {
                    ttl_drops += 1;
                }
            }
            // Punted and never seen again: the program was handed the
            // packet and did not forward it.
            (None, Some(r), false) => {
                *drops.entry(format!("{r}, and the program never sent it on"))
                    .or_insert(0) += 1;
            }
            _ => {}
        }
        if !k.delivered {
            // Attribute the loss to the most recent link event at or just
            // after this packet left, so an in-flight packet counts
            // against the failure that killed it.
            match events
                .iter()
                .rposition(|e| e.at_ms <= at + IN_FLIGHT_GRACE_MS)
            {
                Some(i) => {
                    let slot = &mut ev_loss[i];
                    slot.0 += 1;
                    slot.1 = slot.1.min(at);
                    slot.2 = slot.2.max(at);
                    if loss_ids.len() < 4 {
                        loss_ids.push((*id, at));
                    }
                }
                None => {
                    baseline_loss += 1;
                    if loss_ids.len() < 4 {
                        loss_ids.push((*id, at));
                    }
                }
            }
        }
    }

    for (i, e) in events.iter_mut().enumerate() {
        let (count, first, last) = ev_loss[i];
        e.cost = count;
        if count > 0 {
            e.silence_began_ms = Some(first.saturating_sub(e.at_ms));
            e.recovered_ms = Some(last.saturating_sub(e.at_ms));
        }
        e.ok = e.recovered_ms.unwrap_or(0) <= p.recovery_budget_ms;
    }

    let mut pairs: Vec<PairStat> = pair
        .iter()
        .map(|((s, d), (sn, dv))| PairStat {
            src: *s,
            dst: *d,
            sent: *sn,
            delivered: *dv,
        })
        .collect();
    pairs.sort_by_key(|p| (p.src, p.dst));
    let unreachable: Vec<PairStat> = pairs
        .iter()
        .filter(|p| p.delivered == 0)
        .cloned()
        .collect();

    // Every ordered pair the workload declares should have moved traffic.
    let expected_pairs = world.apps.len() * world.apps.len().saturating_sub(1);
    let silent_pairs = expected_pairs.saturating_sub(pairs.len());

    let delivery_rate = if sent == 0 {
        0.0
    } else {
        delivered as f64 / sent as f64
    };

    // ---- criteria ----
    let mut criteria = Vec::new();

    criteria.push(Criterion {
        id: "C1",
        name: "delivery",
        detail: format!(
            "{:.2}% of {} packets ({} lost), floor {:.2}%",
            100.0 * delivery_rate,
            sent,
            sent - delivered,
            100.0 * p.delivery_floor
        ),
        status: if sent == 0 {
            Status::Fail
        } else if delivery_rate >= p.delivery_floor {
            Status::Pass
        } else {
            Status::Fail
        },
    });

    let reached = pairs.len() - unreachable.len();
    criteria.push(Criterion {
        id: "C2",
        name: "reachability",
        detail: format!(
            "{reached}/{expected_pairs} app pairs delivered at least one packet\
             {}",
            if silent_pairs > 0 {
                format!(" ({silent_pairs} pairs sent nothing at all)")
            } else {
                String::new()
            }
        ),
        status: if reached == expected_pairs {
            Status::Pass
        } else {
            Status::Fail
        },
    });

    criteria.push(Criterion {
        id: "C3",
        name: "loops",
        detail: format!(
            "{looping} packet{} revisited a switch, {ttl_drops} died of TTL{}",
            if looping == 1 { "" } else { "s" },
            if exact_loops {
                ""
            } else {
                " (switch id over 63: TTL evidence only)"
            }
        ),
        // Advisory, not Fail: A1 grades whether packets arrive, not what
        // they cost to carry. A2 grades capacity, and every revisited link
        // is capacity spent twice.
        status: if looping == 0 && ttl_drops == 0 {
            Status::Pass
        } else {
            Status::Advisory
        },
    });

    let recovery_status = if events.is_empty() {
        Status::Skipped
    } else if events.iter().all(|e| e.ok) {
        Status::Pass
    } else {
        Status::Fail
    };
    criteria.push(Criterion {
        id: "C4",
        name: "recovery",
        detail: if events.is_empty() {
            "no link ever went down in this run".to_string()
        } else {
            let worst = events.iter().filter_map(|e| e.recovered_ms).max().unwrap_or(0);
            format!(
                "{}/{} events recovered within {} ms (worst {worst} ms)",
                events.iter().filter(|e| e.ok).count(),
                events.len(),
                p.recovery_budget_ms
            )
        },
        status: recovery_status,
    });

    // ---- integrity ----
    let mut integrity = Vec::new();
    if misdelivered > 0 {
        integrity.push(format!(
            "{misdelivered} packets reached an app that does not own the destination address"
        ));
    }
    // Only an open-loop workload has a predictable packet count; a
    // closed-loop one sends what the network lets it.
    let interval_ms = match world.workload {
        crate::world::Workload::AllPairsCbr { interval_ms, .. } => interval_ms,
        crate::world::Workload::Mixed { .. } => 0,
    };
    if interval_ms > 0 && hi > lo {
        let expected = world.apps.len() as f64 * (hi - lo) as f64 / interval_ms as f64;
        if expected > 0.0 {
            let drift = (sent as f64 - expected).abs() / expected;
            if drift > INTEGRITY_TOLERANCE {
                integrity.push(format!(
                    "workload spec implies about {:.0} packets in the scored window, \
                     the log shows {sent} ({:+.1}%)",
                    expected,
                    100.0 * (sent as f64 - expected) / expected
                ));
            }
        }
    }
    if let Some(s) = schedule {
        if s.schedule.world != world.world.name {
            integrity.push(format!(
                "schedule was written for world '{}', not '{}'",
                s.schedule.world, world.world.name
            ));
        }
        let want = s.failures.len() * 2;
        if events.len() != want {
            integrity.push(format!(
                "schedule declares {want} link events, the log shows {}",
                events.len()
            ));
        }
    }

    // ---- worked examples, from a second pass ----
    let example_loop = loop_ids
        .first()
        .map(|(id, _)| trace_packet(log, *id, &pkts))
        .transpose()
        .map_err(read)?
        .flatten();
    let example_loss = loss_ids
        .first()
        .map(|(id, _)| trace_packet(log, *id, &pkts))
        .transpose()
        .map_err(read)?
        .flatten();

    Ok(Report {
        world: world.world.name.clone(),
        family: world.world.family.clone(),
        switches: world.switches.len(),
        links: world.links.len(),
        apps: world.apps.len(),
        diameter: world.world.diameter,
        // The seed is not printed: the hidden schedules are cut from it.
        schedule: schedule.map(|s| {
            format!(
                "{}, {} link event{}",
                s.schedule.world,
                s.failures.len(),
                if s.failures.len() == 1 { "" } else { "s" }
            )
        }),
        program,
        run_ms,
        window_ms: (lo, hi),
        sent,
        delivered,
        misdelivered,
        delivery_rate,
        looping_packets: looping,
        ttl_drops,
        loop_detection_exact: exact_loops,
        pairs,
        unreachable,
        events,
        baseline_loss,
        drops_by_reason: drops,
        criteria,
        integrity,
        example_loop,
        example_loss,
    })
}

/// Second pass: pull one packet's whole journey out of the log so the
/// report card can show it rather than describe it.
fn trace_packet(
    log: &Path,
    id: u64,
    pkts: &HashMap<u64, Pkt>,
) -> Result<Option<PacketTrace>, crate::sim_log::writer::ReadError> {
    let k = match pkts.get(&id) {
        Some(k) => k,
        None => return Ok(None),
    };
    let mut hops = Vec::new();
    let mut outcome = String::new();
    crate::sim_log::writer::for_each_frame(log, |frame| match frame.event {
        LogEvent::PacketIngress { switch, packet, .. } if packet.id == id => {
            if hops.len() < 40 {
                hops.push((frame.at_ns / 1_000_000, switch));
            }
        }
        LogEvent::PacketDropped {
            reason, packet_id, ..
        } if packet_id == id => {
            outcome = format!("dropped: {}", drop_name(&reason));
        }
        LogEvent::PacketPunted {
            switch,
            reason,
            packet_id,
        } if packet_id == id => {
            outcome = format!("{} at switch {switch}", punt_name(reason));
        }
        LogEvent::PacketDelivered { app, packet_id } if packet_id == id => {
            outcome = format!("delivered to app {app}");
        }
        _ => {}
    })?;
    if outcome.is_empty() {
        outcome =
            "never arrived, never dropped, never punted: still in flight when the run ended"
                .to_string();
    }
    Ok(Some(PacketTrace {
        id,
        src_app: k.src_app,
        dst_app: k.dst_app,
        sent_ms: k.first_ns / 1_000_000,
        hops,
        outcome,
    }))
}

// ---------------------------------------------------------------------------
// The report card
// ---------------------------------------------------------------------------

const RULE: &str = "========================================================================";
const THIN: &str = "------------------------------------------------------------------------";

impl Report {
    /// The thing a student reads. Every number on it is a measurement,
    /// and every failure is followed by the evidence for it.
    pub fn render(&self) -> String {
        let mut o = String::new();
        let p = &mut o;

        push(p, RULE);
        push(p, &format!(" REPORT CARD   world '{}'", self.world));
        push(p, RULE);
        push(
            p,
            &format!(
                " topology    {}, {} switches, {} links, {} apps, diameter {}",
                if self.family.is_empty() {
                    "hand-written"
                } else {
                    &self.family
                },
                self.switches,
                self.links,
                self.apps,
                self.diameter
            ),
        );
        push(
            p,
            &format!(
                " schedule    {}",
                self.schedule.as_deref().unwrap_or("none (Part 1)")
            ),
        );
        if let Some(prog) = &self.program {
            push(p, &format!(" program     {prog}"));
        }
        push(
            p,
            &format!(
                " run         0 - {} ms      scored window {} - {} ms",
                self.run_ms, self.window_ms.0, self.window_ms.1
            ),
        );
        push(p, "");

        push(p, THIN);
        for c in &self.criteria {
            push(
                p,
                &format!(" {} {:<14} {:<6}  {}", c.id, c.name, c.status.label(), c.detail),
            );
        }
        push(p, THIN);
        push(
            p,
            &format!(
                " OVERALL      {}",
                match self.status() {
                    Status::Pass => "PASS",
                    _ => "FAIL",
                }
            ),
        );
        push(p, "");

        if !self.events.is_empty() {
            push(p, " RECOVERY TIMELINE");
            push(
                p,
                "   #      t ms   link     event   silence began   traffic back    cost",
            );
            for (i, e) in self.events.iter().enumerate() {
                push(
                    p,
                    &format!(
                        "  {:>2}  {:>8}   {:<7}  {:<6}  {:>12}  {:>13}  {:>6}  {}",
                        i + 1,
                        e.at_ms,
                        format!("{}-{}", e.link.0, e.link.1),
                        if e.down { "DOWN" } else { "UP" },
                        match e.silence_began_ms {
                            Some(v) => format!("+{v} ms"),
                            None => "-".to_string(),
                        },
                        match e.recovered_ms {
                            Some(v) => format!("+{v} ms"),
                            None => "never lost one".to_string(),
                        },
                        e.cost,
                        if e.ok { "ok" } else { "OVER BUDGET" },
                    ),
                );
            }
            if self.baseline_loss > 0 {
                push(
                    p,
                    &format!(
                        "  {} packets were lost while every link was up. That is not a\
                         \n  failure-recovery problem; it is a steady-state one.",
                        self.baseline_loss
                    ),
                );
            }
            push(p, "");
        }

        if !self.drops_by_reason.is_empty() {
            push(p, " WHERE PACKETS STOPPED");
            for (reason, n) in &self.drops_by_reason {
                push(p, &format!("   {n:>8}  {reason}"));
            }
            push(p, "");
        }

        if let Some(c) = self.first_failure() {
            push(p, " WHAT WENT WRONG");
            push(p, &format!("   {} {} — {}", c.id, c.name, c.detail));
            push(p, "");
            match c.id {
                "C2" => {
                    for u in self.unreachable.iter().take(8) {
                        push(
                            p,
                            &format!(
                                "   app {} -> app {}: sent {}, delivered 0",
                                u.src, u.dst, u.sent
                            ),
                        );
                    }
                    if self.unreachable.len() > 8 {
                        push(
                            p,
                            &format!("   ... and {} more pairs", self.unreachable.len() - 8),
                        );
                    }
                    push(p, "");
                    if let Some(t) = &self.example_loss {
                        push_trace(p, "   One of those packets:", t);
                    }
                }
                "C4" => {
                    for e in self.events.iter().filter(|e| !e.ok) {
                        push(
                            p,
                            &format!(
                                "   link {}-{} went {} at t={} ms; traffic did not come back\
                                 \n   until +{} ms, and {} packets were lost getting there.",
                                e.link.0,
                                e.link.1,
                                if e.down { "down" } else { "up" },
                                e.at_ms,
                                e.recovered_ms.unwrap_or(0),
                                e.cost
                            ),
                        );
                    }
                    push(p, "");
                    if let Some(t) = &self.example_loss {
                        push_trace(p, "   One of the lost packets:", t);
                    }
                }
                _ => {
                    if let Some(t) = &self.example_loss {
                        push_trace(p, "   One of the lost packets:", t);
                    }
                }
            }
            push(p, "");
        }

        for c in self.criteria.iter().filter(|c| c.status == Status::Advisory) {
            push(p, " WORTH KNOWING");
            push(p, &format!("   {} {} — {}", c.id, c.name, c.detail));
            push(p, "");
            if c.id == "C3" {
                if let Some(t) = &self.example_loop {
                    push_trace(p, "   A packet that went round:", t);
                    push(p, "");
                }
                push(p, "   Loops do not fail you in A1. Every revisited link is");
                push(p, "   capacity spent twice, and A2 grades capacity.");
            }
            push(p, "");
        }

        if !self.integrity.is_empty() {
            push(p, " INTEGRITY");
            for w in &self.integrity {
                push(p, &format!("   ! {w}"));
            }
            push(p, "");
        }

        push(p, RULE);
        o
    }
}

fn push(o: &mut String, line: &str) {
    o.push_str(line);
    o.push('\n');
}

fn push_trace(o: &mut String, title: &str, t: &PacketTrace) {
    push(o, title);
    push(
        o,
        &format!(
            "     packet {} — app {} -> app {}, sent at t={} ms",
            t.id, t.src_app, t.dst_app, t.sent_ms
        ),
    );
    let path: Vec<String> = t
        .hops
        .iter()
        .map(|(ms, s)| format!("s{s}@{ms}ms"))
        .collect();
    push(o, &format!("     path: {}", path.join(" -> ")));
    push(o, &format!("     outcome: {}", t.outcome));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_log::{EventLogger, LinkConfigW, PacketSnapshotW, SwitchConfigW};
    use std::path::PathBuf;

    /// Three switches in a triangle, three apps. Small enough to write
    /// packet journeys by hand, big enough to have an alternate path.
    const WORLD: &str = r#"
[world]
name = "t"
family = "ring_chords"
diameter = 1

[params]
link_capacity_mbps = 1000
link_latency_us = 100
queue_capacity_bytes = 65536
warmup_ms = 100
duration_ms = 1000

[[switch]]
id = 0
as_id = 0
[[switch]]
id = 1
as_id = 0
[[switch]]
id = 2
as_id = 0

[[link]]
a = 0
b = 1
[[link]]
a = 1
b = 2
[[link]]
a = 0
b = 2

[[app]]
id = 100
switch = 0
prefix = "10.0.1.0/24"
[[app]]
id = 101
switch = 1
prefix = "10.0.2.0/24"
[[app]]
id = 102
switch = 2
prefix = "10.0.3.0/24"

[workload]
kind = "all_pairs_cbr"
interval_ms = 10
size_bytes = 100
"#;

    fn world() -> World {
        World::from_str(WORLD).expect("test world parses")
    }

    /// App id -> the address the world gives it.
    fn addr(app: u32) -> u32 {
        0x0a00_0001 | ((app - 99) << 8)
    }

    fn snap(id: u64, src: u32, dst: u32) -> PacketSnapshotW {
        PacketSnapshotW {
            id,
            kind: 0,
            size_bytes: 100,
            ip_src: addr(src),
            ip_dst: addr(dst),
            ip_proto: 6,
            ip_ttl: 64,
            label_top: 0,
            label_depth: 0,
            flow_id: src as u64,
            seq: None,
        }
    }

    /// A tiny builder for synthetic `.simlog` files. The scorer's whole
    /// job is reading these, so the tests hand it hand-written ones and
    /// assert on what it says.
    struct Log {
        dir: tempfile::TempDir,
        path: PathBuf,
        log: Option<EventLogger>,
        next: u64,
    }

    impl Log {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("run.simlog");
            let log = EventLogger::create(&path).unwrap();
            let mut me = Self {
                dir,
                path,
                log: Some(log),
                next: 1,
            };
            me.topology();
            me
        }

        fn ev(&mut self, at_ms: u64, e: LogEvent) {
            self.log.as_mut().unwrap().log(at_ms * 1_000_000, e);
        }

        fn topology(&mut self) {
            for id in 0..3u32 {
                self.ev(
                    0,
                    LogEvent::SwitchAdded {
                        id,
                        owner: Some(0),
                        num_ports: 8,
                        config: SwitchConfigW {
                            stages: 4,
                            processing_delay_ns: 100,
                            recirculation_enabled: true,
                            max_recirculations: 2,
                            punt_pipe_latency_ns: 10_000,
                            punt_pipe_bandwidth_bps: 1_000_000_000,
                            config_pipe_latency_ns: 10_000,
                            config_pipe_bandwidth_bps: 1_000_000_000,
                        },
                    },
                );
            }
            for (id, a, b) in [(0u32, 0u32, 1u32), (1, 1, 2), (2, 0, 2)] {
                self.ev(
                    0,
                    LogEvent::LinkAdded {
                        id,
                        a: NodeRefW::Switch(a),
                        a_port: 0,
                        b: NodeRefW::Switch(b),
                        b_port: 0,
                        config: LinkConfigW {
                            latency_ns: 100_000,
                            bandwidth_bps: 1_000_000_000,
                            queue_capacity_bytes: 65536,
                        },
                    },
                );
            }
        }

        /// One packet that walks `path` and is then delivered.
        fn delivered(&mut self, at_ms: u64, src: u32, dst: u32, path: &[u32]) -> u64 {
            let id = self.next;
            self.next += 1;
            for (i, s) in path.iter().enumerate() {
                self.ev(
                    at_ms + i as u64,
                    LogEvent::PacketIngress {
                        switch: *s,
                        port: 0,
                        packet: snap(id, src, dst),
                    },
                );
            }
            self.ev(
                at_ms + path.len() as u64,
                LogEvent::PacketDelivered {
                    app: dst,
                    packet_id: id,
                },
            );
            id
        }

        /// One packet that walks `path` and then dies.
        fn dropped(
            &mut self,
            at_ms: u64,
            src: u32,
            dst: u32,
            path: &[u32],
            reason: DropReasonW,
        ) -> u64 {
            let id = self.next;
            self.next += 1;
            for (i, s) in path.iter().enumerate() {
                self.ev(
                    at_ms + i as u64,
                    LogEvent::PacketIngress {
                        switch: *s,
                        port: 0,
                        packet: snap(id, src, dst),
                    },
                );
            }
            self.ev(
                at_ms + path.len() as u64,
                LogEvent::PacketDropped {
                    at: NodeRefW::Switch(*path.last().unwrap()),
                    reason,
                    packet_id: id,
                },
            );
            id
        }

        fn link_down(&mut self, at_ms: u64, link: u32) {
            self.ev(at_ms, LogEvent::LinkFailed { id: link });
        }

        fn link_up(&mut self, at_ms: u64, link: u32) {
            self.ev(at_ms, LogEvent::LinkRestored { id: link });
        }

        /// Close the file and score it.
        fn score(&mut self, p: &ScoreParams) -> Report {
            self.log = None; // flush
            score_file(&self.path, &world(), None, None, p).unwrap()
        }
    }

    /// Loose enough that the small hand-written runs below are not judged
    /// on volume, and with no tail guard so the last packet still counts.
    fn params() -> ScoreParams {
        ScoreParams {
            warmup_ms: 100,
            tail_guard_ms: 0,
            delivery_floor: 0.95,
            recovery_budget_ms: 100,
        }
    }

    /// Every ordered pair moves one packet. Nothing is lost. The report
    /// card should say so and nothing else.
    #[test]
    fn a_clean_run_passes() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        let mut t = 200;
        for s in apps {
            for d in apps {
                if s != d {
                    l.delivered(t, s, d, &[s - 100, d - 100]);
                    t += 10;
                }
            }
        }
        let r = l.score(&params());
        assert_eq!(r.sent, 6, "six ordered pairs, one packet each");
        assert_eq!(r.delivered, 6);
        assert_eq!(r.status(), Status::Pass, "{}", r.render());
        assert_eq!(
            r.criteria.iter().find(|c| c.id == "C4").unwrap().status,
            Status::Skipped,
            "no link ever went down, so recovery is not a question"
        );
        let _ = l.dir.path();
    }

    /// A program is allowed to know nothing for the first `warmup_ms`.
    /// Loss in that window must not appear anywhere on the card.
    #[test]
    fn loss_before_the_warmup_window_is_not_counted() {
        let mut l = Log::new();
        for i in 0..20 {
            l.dropped(i * 4, 100, 101, &[0], DropReasonW::NoRoute);
        }
        let apps = [100u32, 101, 102];
        let mut t = 200;
        for s in apps {
            for d in apps {
                if s != d {
                    l.delivered(t, s, d, &[s - 100, d - 100]);
                    t += 10;
                }
            }
        }
        let r = l.score(&params());
        assert_eq!(r.sent, 6, "the 20 warmup packets are invisible");
        assert_eq!(r.delivery_rate, 1.0);
        assert_eq!(r.status(), Status::Pass, "{}", r.render());
    }

    /// The demotion itself: a run that loops but still delivers above the
    /// floor has to pass. A1 grades arrival, not the cost of the trip.
    /// The handout promises this in as many words, so the scorer owes it.
    #[test]
    fn a_loop_alone_does_not_fail_the_run() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        let mut t = 200;
        // 30 clean deliveries, so one lost packet stays above the 95% floor.
        for _ in 0..5 {
            for s in apps {
                for d in apps {
                    if s != d {
                        l.delivered(t, s, d, &[s - 100, d - 100]);
                        t += 10;
                    }
                }
            }
        }
        // One packet goes round and round until the TTL kills it.
        l.dropped(t, 100, 102, &[0, 1, 2, 0, 1, 2], DropReasonW::Ttl);

        let r = l.score(&params());
        assert_eq!(r.looping_packets, 1);
        let c3 = r.criteria.iter().find(|c| c.id == "C3").unwrap();
        assert_eq!(c3.status, Status::Advisory);
        assert_eq!(r.status(), Status::Pass, "{}", r.render());
        assert!(
            r.render().contains("A2 grades capacity"),
            "the card must still warn about the cost: {}",
            r.render()
        );
    }

    /// A packet that visits a switch twice is a routing loop. A1 does not
    /// grade capacity, so the card has to show the path and let the run
    /// pass, rather than fail it.
    #[test]
    fn a_looping_packet_is_caught_and_its_path_is_shown() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        let mut t = 200;
        for s in apps {
            for d in apps {
                if s != d {
                    l.delivered(t, s, d, &[s - 100, d - 100]);
                    t += 10;
                }
            }
        }
        // 0 -> 1 -> 2 -> 0 -> ... and finally the TTL runs out.
        l.dropped(300, 100, 102, &[0, 1, 2, 0, 1, 2], DropReasonW::Ttl);

        let r = l.score(&params());
        let c3 = r.criteria.iter().find(|c| c.id == "C3").unwrap();
        assert_eq!(c3.status, Status::Advisory, "{}", r.render());
        assert!(
            r.first_failure().map(|c| c.id) != Some("C3"),
            "a loop must never be the reason a run failed: {}",
            r.render()
        );
        assert_eq!(r.looping_packets, 1);
        assert_eq!(r.ttl_drops, 1);
        let t = r.example_loop.expect("the card must show the loop");
        assert_eq!(
            t.hops.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
            vec![0, 1, 2, 0, 1, 2]
        );
        assert!(t.outcome.contains("ttl"), "outcome was {}", t.outcome);
    }

    /// One pair that never gets through is a black hole, and the card
    /// names the pair. A high overall delivery rate must not hide it.
    #[test]
    fn a_black_holed_pair_is_named() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        let mut t = 200;
        for s in apps {
            for d in apps {
                if s != d && !(s == 100 && d == 102) {
                    for k in 0..50 {
                        l.delivered(t + k, s, d, &[s - 100, d - 100]);
                    }
                    t += 60;
                }
            }
        }
        for k in 0..3 {
            l.dropped(200 + k, 100, 102, &[0], DropReasonW::NoRoute);
        }
        let r = l.score(&params());
        assert!(
            r.delivery_rate > 0.98,
            "the rate alone looks fine: {}",
            r.delivery_rate
        );
        let c2 = r.criteria.iter().find(|c| c.id == "C2").unwrap();
        assert_eq!(c2.status, Status::Fail, "{}", r.render());
        assert_eq!(r.unreachable.len(), 1);
        assert_eq!((r.unreachable[0].src, r.unreachable[0].dst), (100, 102));
        assert!(r.render().contains("app 100 -> app 102"));
    }

    /// Recovery is measured from the failure, not from the start of the
    /// run: the same number of lost packets passes or fails depending on
    /// how long the losses kept coming.
    #[test]
    fn recovery_is_measured_per_event_against_the_budget() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        // Steady traffic on every pair, once every 10 ms, out to 3000 ms.
        let send = |l: &mut Log, from: u64, to: u64, blackout: Option<(u64, u64)>| {
            let mut t = from;
            while t < to {
                for s in apps {
                    for d in apps {
                        if s == d {
                            continue;
                        }
                        let dark = matches!(blackout, Some((a, b)) if t >= a && t < b)
                            && s == 100
                            && d == 102;
                        if dark {
                            l.dropped(t, s, d, &[s - 100], DropReasonW::LinkDrop);
                        } else {
                            l.delivered(t, s, d, &[s - 100, d - 100]);
                        }
                    }
                }
                t += 10;
            }
        };
        send(&mut l, 200, 1000, None);
        // Link 2 (switches 0-2) dies at 1000 and the program takes 60 ms
        // to route around it — inside a 100 ms budget.
        l.link_down(1000, 2);
        send(&mut l, 1000, 2000, Some((1000, 1060)));
        l.link_up(2000, 2);
        // Restoring it costs nothing.
        send(&mut l, 2000, 2500, None);
        // Link 0 (switches 0-1) dies at 2500 and the program takes 400 ms
        // — four times the budget.
        l.link_down(2500, 0);
        send(&mut l, 2500, 3200, Some((2500, 2900)));

        let r = l.score(&params());
        assert_eq!(r.events.len(), 3);

        let e0 = &r.events[0];
        assert_eq!((e0.link, e0.down), ((0, 2), true));
        assert_eq!(e0.recovered_ms, Some(50), "last loss was sent at 1050 ms");
        assert!(e0.ok, "50 ms is inside a 100 ms budget");

        let e1 = &r.events[1];
        assert_eq!(e1.cost, 0, "coming back up cost nothing");
        assert!(e1.ok);

        let e2 = &r.events[2];
        assert_eq!((e2.link, e2.down), ((0, 1), true));
        assert_eq!(e2.recovered_ms, Some(390));
        assert!(!e2.ok, "390 ms blows a 100 ms budget");

        let c4 = r.criteria.iter().find(|c| c.id == "C4").unwrap();
        assert_eq!(c4.status, Status::Fail, "{}", r.render());
        assert!(r.render().contains("OVER BUDGET"));
        assert_eq!(
            r.baseline_loss, 0,
            "every loss belongs to a link event, none to steady state"
        );
        // The whole-run rate stays high even though a link event blew the
        // budget. That is exactly why C4 exists.
        assert!(r.delivery_rate > 0.97, "rate was {}", r.delivery_rate);
        assert_eq!(
            r.criteria.iter().find(|c| c.id == "C1").unwrap().status,
            Status::Pass
        );
    }

    /// Whatever a program invents to talk to its neighbours is its own
    /// business. It is not workload and must not move the score.
    #[test]
    fn control_traffic_is_not_scored() {
        let mut l = Log::new();
        let apps = [100u32, 101, 102];
        let mut t = 200;
        for s in apps {
            for d in apps {
                if s != d {
                    l.delivered(t, s, d, &[s - 100, d - 100]);
                    t += 10;
                }
            }
        }
        // A flood of program-invented control packets to a non-app
        // address, none of which is ever delivered to an app.
        for i in 0..500u64 {
            l.ev(
                300 + i,
                LogEvent::PacketIngress {
                    switch: (i % 3) as u32,
                    port: 1,
                    packet: PacketSnapshotW {
                        id: 900_000 + i,
                        kind: 0,
                        size_bytes: 40,
                        ip_src: addr(100),
                        ip_dst: 0xe000_0005,
                        ip_proto: 89,
                        ip_ttl: 1,
                        label_top: 0,
                        label_depth: 0,
                        flow_id: 0,
                        seq: None,
                    },
                },
            );
        }
        let r = l.score(&params());
        assert_eq!(r.sent, 6, "control traffic is invisible to the scorer");
        assert_eq!(r.status(), Status::Pass, "{}", r.render());
    }

    /// Arriving at the wrong app is not arriving.
    #[test]
    fn a_misdelivery_is_not_a_delivery() {
        let mut l = Log::new();
        let id = l.next;
        l.next += 1;
        l.ev(
            200,
            LogEvent::PacketIngress {
                switch: 0,
                port: 0,
                packet: snap(id, 100, 102),
            },
        );
        // Switch 1 hands it to app 101, which does not own 10.0.3.0/24.
        l.ev(
            201,
            LogEvent::PacketDelivered {
                app: 101,
                packet_id: id,
            },
        );
        let r = l.score(&params());
        assert_eq!(r.sent, 1);
        assert_eq!(r.delivered, 0);
        assert_eq!(r.misdelivered, 1);
        assert!(
            r.integrity.iter().any(|w| w.contains("does not own")),
            "the card must flag it: {:?}",
            r.integrity
        );
    }

    /// A run that starts after the world was built still has to name the
    /// world it came from, and a mismatched schedule is a setup error the
    /// card should surface rather than silently score around.
    #[test]
    fn a_report_names_the_world_and_the_window() {
        let mut l = Log::new();
        l.delivered(200, 100, 101, &[0, 1]);
        let r = l.score(&params());
        assert_eq!(r.world, "t");
        assert_eq!(r.family, "ring_chords");
        assert_eq!(r.window_ms.0, 100);
        let text = r.render();
        assert!(text.contains("REPORT CARD"));
        assert!(text.contains("world 't'"));
        assert!(text.contains("ring_chords, 3 switches, 3 links, 3 apps"));
    }
}
