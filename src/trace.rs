//! Traceroute / debug probe machinery.
//!
//! A trace probe is **invisible** to ordinary switch programs: it traverses
//! the network as a normal data packet (`kind = Data`), with the simulator
//! recording the perfect path in a hidden trail. When the probe is
//! delivered or dropped, the simulator publishes a `TraceResult` keyed by
//! `TraceId`. The requester polls for it via `Simulator::trace_result`.
//!
//! Each AS has a token-bucket budget on outstanding traces.

use crate::types::{AsId, LinkId, NodeId, SimTime, SwitchId, TraceId};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Ttl,
    NoRoute,
    NoEgressLink,
    /// The link out of the chosen port is down.
    LinkDrop,
    SwitchFailed,
    Recirculation,
    /// The egress queue (or the node's buffer pool) had no room.
    QueueFull,
    /// The data-plane program chose to drop the packet.
    Policy,
}

#[derive(Debug, Clone)]
pub struct TraceResult {
    pub trace_id: TraceId,
    pub requester_as: AsId,
    pub switch_path: Vec<SwitchId>,
    pub as_path: Vec<AsId>,
    pub link_path: Vec<LinkId>,
    pub per_hop_delay: Vec<Duration>,
    pub delivered: bool,
    pub dropped_at: Option<NodeId>,
    pub drop_reason: Option<DropReason>,
}

#[derive(Debug, Clone, Copy)]
pub struct TraceBudget {
    pub max_traces_per_second: u64,
    pub burst_size: u64,
}

impl Default for TraceBudget {
    fn default() -> Self {
        Self {
            max_traces_per_second: 100,
            burst_size: 10,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BucketState {
    pub tokens: f64,
    pub last_refill: SimTime,
}

impl BucketState {
    pub fn new(burst: u64) -> Self {
        Self { tokens: burst as f64, last_refill: Duration::ZERO }
    }
}

#[derive(Debug, Default)]
pub struct TraceManager {
    pub budget_per_as: HashMap<AsId, TraceBudget>,
    pub(crate) buckets: HashMap<AsId, BucketState>,
    pub completed: HashMap<TraceId, TraceResult>,
    next_trace_id: u64,
}

impl TraceManager {
    pub fn new() -> Self { Self::default() }

    pub fn set_budget(&mut self, as_id: AsId, budget: TraceBudget) {
        self.buckets.insert(as_id, BucketState::new(budget.burst_size));
        self.budget_per_as.insert(as_id, budget);
    }

    pub fn allocate_id(&mut self) -> TraceId {
        let id = self.next_trace_id;
        self.next_trace_id += 1;
        TraceId::new(id)
    }

    /// Try to spend a token from `as_id`'s bucket. Returns `true` on success.
    pub fn consume_token(&mut self, as_id: AsId, now: SimTime) -> bool {
        let budget = match self.budget_per_as.get(&as_id) {
            Some(b) => *b,
            None => return false,
        };
        let bucket = self.buckets.entry(as_id).or_insert_with(|| BucketState::new(budget.burst_size));
        let dt = now.saturating_sub(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + dt * budget.max_traces_per_second as f64)
            .min(budget.burst_size as f64);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn record(&mut self, result: TraceResult) {
        self.completed.insert(result.trace_id, result);
    }

    pub fn take(&mut self, id: TraceId) -> Option<TraceResult> {
        self.completed.remove(&id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceError {
    BudgetExceeded,
    UnknownAs,
    NoBorderSwitch,
    NoLinkAtBorder,
}
