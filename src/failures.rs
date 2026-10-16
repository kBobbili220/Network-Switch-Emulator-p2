//! Failure schedules: when links die, and when they come back.
//!
//! A schedule is a separate file from the world on purpose. One topology
//! hosts many schedules, so a world and a schedule seed are independent
//! coordinates.
//!
//! This component is a **measurement instrument**, not content. Its job is
//! not to break the network; it is to produce failures whose recovery can
//! be measured without ambiguity. Everything below follows from that:
//!
//! - **Quiet around every event.** Scoring asks "was delivery back above
//!   the bar within the recovery budget, after every failure?" That is only
//!   answerable if each event sits in clean space. So consecutive events are
//!   spaced by at least the budget, with slack.
//! - **Restoration is an event too.** Re-adding a link can build a
//!   transient loop, and the handout asks students to use the link again
//!   when it returns. So the up edge gets its own measurement window.
//! - **Jittered windows.** If every failure held for exactly 2000 ms, a
//!   student could write "after a dip, wait 2000 ms and flip back" and never
//!   implement detection at all. That would transfer to the grading run,
//!   because the *shape* repeats even though the seed does not. Windows are
//!   drawn from a range, floored at the recovery budget.
//! - **Down and up are paired.** An event list would let a link go down and
//!   never return. Pairing makes that unrepresentable rather than checked.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::world::{World, WorldError};

/// Bumped whenever the schedule generator's output changes for a fixed seed.
pub const GENERATOR_VERSION: u32 = 2;

/// How long a program gets to restore delivery after a link changes state.
/// Schedules carry their own budget; this is the fallback.
pub const DEFAULT_RECOVERY_BUDGET_MS: u64 = 1_000;


// ---------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub schedule: ScheduleMeta,
    #[serde(rename = "failure", default)]
    pub failures: Vec<Failure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleMeta {
    /// Name of the world this schedule was cut against. Checked before a
    /// run, so a schedule cannot be pointed at the wrong topology.
    pub world: String,
    pub seed: u64,
    /// How long a program has to get delivery back up after an event.
    pub recovery_budget_ms: u64,
    /// Run length this schedule needs. The world must be at least this long.
    pub duration_ms: u64,
    #[serde(default)]
    pub generator: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    /// Endpoints, not an index into the link list. This is what a report
    /// card prints — "link 3-7 died at t=2400 ms" reads; "link 11" does not.
    pub link: [u32; 2],
    pub down_ms: u64,
    pub up_ms: u64,
    /// How many app pairs have to change their shortest path because of
    /// this failure. Failures differ enormously here — some force one pair
    /// to reroute, some force half the network — and the number is not
    /// visible from the topology at a glance. The scorer uses it as a
    /// difficulty label; the quiz uses it as a question.
    #[serde(default)]
    pub blast_radius: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Down,
    Up,
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------



// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

fn bad(m: String) -> WorldError {
    WorldError::Invalid(m)
}

fn norm(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

/// FNV-1a, hand-rolled for the same reason `crate::rng` is: the hasher in
/// `std` is explicitly not stable across Rust releases, and a published
/// schedule that silently retimes itself when the toolchain moves is worse
/// than no schedule at all.

/// What makes this world *this* world.
///
/// Without it the schedule RNG is a function of the seed alone: the only
/// world-derived quantity touching the stream is `order.len()`, so any two
/// worlds with the same link count draw byte-identical timings, across
/// families and across the practice/reserve split.
///
/// Deliberately excludes `params`, so the Part 1 / Part 2 delivery-floor
/// difference does not retime a schedule, and `world.generator`, because a
/// generator change that actually moves a world moves its edge set, which
/// is already here.

/// A measurement window: never shorter than the recovery budget, never a
/// constant. The floor keeps the schedule scoreable; the jitter stops a
/// timer tuned to the pattern from working.


// ---------------------------------------------------------------------------
// Distances and blast radius
// ---------------------------------------------------------------------------

/// Shortest-path hop counts between every pair of switches that hosts an
/// app, optionally with one link removed.

/// App pairs whose shortest-path length changes when this link dies.

// ---------------------------------------------------------------------------
// Load, validate, drive
// ---------------------------------------------------------------------------

impl Schedule {
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, WorldError> {
        let text = std::fs::read_to_string(path)?;
        let s: Schedule = toml::from_str(&text).map_err(WorldError::Parse)?;
        Ok(s)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("schedule serializes")
    }

    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<(), WorldError> {
        std::fs::write(path, self.to_toml())?;
        Ok(())
    }

    /// Every event, in time order.
    pub fn events(&self) -> Vec<(u64, (u32, u32), Action)> {
        let mut v: Vec<(u64, (u32, u32), Action)> = Vec::new();
        for f in &self.failures {
            let link = norm(f.link[0], f.link[1]);
            v.push((f.down_ms, link, Action::Down));
            v.push((f.up_ms, link, Action::Up));
        }
        v.sort_by_key(|(t, _, _)| *t);
        v
    }

    /// Everything that must hold before a schedule is worth running. The
    /// generator runs this on its own output rather than trusting itself.
    pub fn validate(&self, world: &World) -> Result<(), WorldError> {
        if self.schedule.world != world.world.name {
            return Err(bad(format!(
                "schedule was cut against world '{}', not '{}'",
                self.schedule.world, world.world.name
            )));
        }
        if self.failures.is_empty() {
            return Err(bad("schedule has no failures".into()));
        }

        let known: BTreeSet<(u32, u32)> =
            world.links.iter().map(|l| norm(l.a, l.b)).collect();
        let budget = self.schedule.recovery_budget_ms;

        let mut prev_end = 0u64;
        for f in &self.failures {
            let link = norm(f.link[0], f.link[1]);
            if !known.contains(&link) {
                return Err(bad(format!(
                    "schedule fails link {}-{}, which the world does not have",
                    f.link[0], f.link[1]
                )));
            }
            if f.up_ms <= f.down_ms {
                return Err(bad(format!(
                    "link {}-{} comes back at {} ms, before it died at {} ms",
                    f.link[0], f.link[1], f.up_ms, f.down_ms
                )));
            }
            // One at a time, for now. A1's worlds are 2-edge-connected,
            // which covers exactly one concurrent failure — no more.
            if f.down_ms < prev_end {
                return Err(bad(format!(
                    "link {}-{} dies at {} ms, while another failure is still open",
                    f.link[0], f.link[1], f.down_ms
                )));
            }
            // Every window must be wide enough to recover in, or the
            // schedule is asking a question it does not leave room to answer.
            if f.up_ms - f.down_ms < budget {
                return Err(bad(format!(
                    "link {}-{} is down for {} ms, under the {} ms recovery budget",
                    f.link[0],
                    f.link[1],
                    f.up_ms - f.down_ms,
                    budget
                )));
            }
            if prev_end > 0 && f.down_ms - prev_end < budget {
                return Err(bad(format!(
                    "only {} ms between events, under the {} ms recovery budget",
                    f.down_ms - prev_end,
                    budget
                )));
            }
            // Never partition. Automatic while failures are one at a time
            // and worlds are 2-edge-connected — checked anyway, because the
            // check is what makes it true rather than likely.
            let mut down = BTreeSet::new();
            down.insert(link);
            if !world.connected_without(&down) {
                return Err(bad(format!(
                    "failing link {}-{} would partition the world",
                    f.link[0], f.link[1]
                )));
            }
            prev_end = f.up_ms;
        }

        if self.schedule.duration_ms < prev_end + budget {
            return Err(bad(format!(
                "run ends at {} ms, leaving under {} ms to measure the last restoration",
                self.schedule.duration_ms, budget
            )));
        }
        Ok(())
    }
}

