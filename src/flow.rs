//! The customer engine: one closed-loop flow.
//!
//! An A2 customer sends at `min(demand, law_rate)`, numbers every data
//! packet, expects an `Ack` for each, retransmits what is not acknowledged
//! in time, and lets an adaptation law move `law_rate` in response to
//! what it sees. Every preset in the handout is a row of configuration
//! for this one engine: the demand shape says what the customer *wants*
//! to send, the law says how it *reacts* to the network.
//!
//! Timing is rate based, not window based: a flow paces one packet every
//! `size × 8 / rate` seconds. Loss is decided on the sender's own tick,
//! from a timeout, so the whole thing stays deterministic and needs no
//! extra event kinds.

use crate::types::SimTime;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// What the customer wants to send, before the network has any say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Demand {
    /// A steady rate.
    Constant { bps: f64 },
    /// Bursts of `on` at `bps`, silent for `off`, first burst at `start`.
    OnOff {
        bps: f64,
        on: Duration,
        off: Duration,
        start: Duration,
    },
    /// As fast as the law allows.
    Unlimited,
}

impl Demand {
    /// Offered rate at `now`, and when it next changes (`None` = never).
    fn at(&self, now: SimTime) -> (f64, Option<SimTime>) {
        match *self {
            Demand::Constant { bps } => (bps, None),
            Demand::Unlimited => (f64::INFINITY, None),
            Demand::OnOff {
                bps,
                on,
                off,
                start,
            } => {
                if now < start {
                    return (0.0, Some(start));
                }
                let period = on + off;
                if period.is_zero() {
                    return (bps, None);
                }
                let since = now - start;
                let periods = since.as_nanos() / period.as_nanos();
                let period_start = start + period * (periods as u32);
                let into = now - period_start;
                if into < on {
                    (bps, Some(period_start + on))
                } else {
                    (0.0, Some(period_start + period))
                }
            }
        }
    }
}

/// How the customer reacts to the network.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Law {
    /// `law_rate` never moves. The open-loop control.
    Fixed { bps: f64 },
    /// Additive increase, multiplicative decrease. On a loss the rate is
    /// halved, once per congestion event: losses of packets sent before
    /// the last halving, or within one timeout after it while the queue
    /// it filled drains, are the same event and do not halve again. Every
    /// `epoch` that passes without a loss adds `increment_bps`. Starts at
    /// `initial_bps`. The cliff and the climb.
    Aimd {
        initial_bps: f64,
        increment_bps: f64,
        epoch: Duration,
    },
    /// Rate matching: the rate is what the network delivered lately —
    /// bytes acknowledged in the last `2 × min_rtt`, divided by that
    /// window — times a probing gain, so a flow that is not losing keeps
    /// asking for a little more and one that is losing settles where the
    /// network stops it. Starts at `initial_bps` until the first RTT is
    /// known, holds still while the customer has nothing to send, and
    /// never falls below one packet per window, so it cannot die.
    ///
    /// The data in flight is limited as well, to `RATE_MATCH_INFLIGHT_BDP`
    /// times the bandwidth-delay product (the delivered rate × `min_rtt`).
    /// One BDP fills the path; the rest waits in the queue of the link
    /// that limits the flow. So the flow holds a standing queue of about
    /// one BDP, and does not need a drop to stop there.
    ///
    /// The `min_rtt` in that product is the smallest round trip seen
    /// since the last probe. Every `RATE_MATCH_PROBE_INTERVAL` without a
    /// new minimum the flow drops to `RATE_MATCH_MIN_INFLIGHT_PACKETS` in
    /// flight for `RATE_MATCH_PROBE_TIME`, so its own queue drains, and
    /// takes the smallest round trip of that time as the new minimum.
    /// Without this, a flow moved to a longer path would keep the old
    /// path's round trip and starve itself.
    RateMatch { initial_bps: f64 },
}

/// How much more than the delivered rate a rate-matching flow asks for.
/// At 1.25 a flow only stops climbing when a fifth of what it sends is
/// lost — which is the point: it barely flinches at a drop.
pub const RATE_MATCH_GAIN: f64 = 1.25;

/// How much data a rate-matching flow keeps in flight, in BDPs. At 2, one
/// BDP is on the path and up to one waits in a queue.
pub const RATE_MATCH_INFLIGHT_BDP: f64 = 2.0;

/// The in-flight limit never goes under this many packets: a slow flow's
/// BDP is a fraction of one packet, and it must still be able to send.
pub const RATE_MATCH_MIN_INFLIGHT_PACKETS: u64 = 4;

/// How long a rate-matching flow trusts its smallest round trip before
/// it measures again.
pub const RATE_MATCH_PROBE_INTERVAL: Duration = Duration::from_millis(2_000);

/// How long it holds its data in flight at the minimum to measure.
pub const RATE_MATCH_PROBE_TIME: Duration = Duration::from_millis(50);

/// The shortest window a rate-matching flow measures over. Two RTTs on a
/// fast path is a few milliseconds, which holds at most one packet of a
/// slow flow: the estimate would be quantised at one packet per window
/// and the flow could never climb out of it.
pub const RATE_MATCH_MIN_WINDOW: Duration = Duration::from_millis(100);

impl Law {
    fn initial_rate(&self) -> f64 {
        match *self {
            Law::Fixed { bps } => bps,
            Law::Aimd { initial_bps, .. } => initial_bps,
            Law::RateMatch { initial_bps } => initial_bps,
        }
    }
}

/// What one tick of the engine wants the simulator to do.
pub struct FlowTick {
    /// A packet to send now, if any.
    pub send: Option<FlowSend>,
    /// When to tick again. `None` means the flow is finished.
    pub next: Option<Duration>,
}

/// One data packet the engine wants on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowSend {
    pub seq: u64,
    pub size_bytes: u64,
    /// True when this `seq` has been sent before.
    pub retransmit: bool,
}

/// Sender-side state of one flow.
#[derive(Debug, Clone)]
pub struct FlowEngine {
    pub demand: Demand,
    pub law: Law,
    pub packet_size: u64,
    /// The law's current ceiling on the send rate.
    pub law_rate_bps: f64,

    /// Loss is declared when an ack is `rto_multiplier × srtt` late, and
    /// never sooner than `rto_min`; before any RTT sample, `initial_rto`.
    pub rto_multiplier: f64,
    pub rto_min: Duration,
    pub initial_rto: Duration,

    next_seq: u64,
    /// Unacknowledged sends: seq → when the latest copy left.
    outstanding: BTreeMap<u64, SimTime>,
    /// Seqs declared lost and waiting for their turn on the wire.
    retransmit_queue: VecDeque<u64>,
    /// Seqs sent more than once and not yet acknowledged. An ack for one
    /// does not say which copy it answers, so it is no measure of the
    /// path's round trip.
    resent: std::collections::BTreeSet<u64>,

    pub min_rtt: Option<Duration>,
    pub srtt: Option<Duration>,

    pub first_sends: u64,
    pub retransmits: u64,
    pub acked: u64,
    pub losses: u64,

    // AIMD bookkeeping.
    /// When the current loss-free epoch began.
    epoch_start: SimTime,
    /// A loss happened in the current epoch, so it earns no increase.
    epoch_had_loss: bool,
    /// The last time the rate was halved; halving waits one RTT.
    last_halve_at: Option<SimTime>,
    /// Halvings so far.
    pub halvings: u64,

    // Rate-matching bookkeeping: (acked at, bytes) for the recent window,
    // and when the first ack arrived -- the window is only trusted once
    // a full one has had time to fill, or the start-up would read as a
    // collapse.
    acked_bytes: VecDeque<(SimTime, u64)>,
    first_ack_at: Option<SimTime>,
    /// The delivered rate of the last full window, before the probing
    /// gain. `None` until one window has been measured; the in-flight
    /// limit waits for it.
    matched_bps: Option<f64>,
    /// The smallest round trip since the last probe, and when it was
    /// last seen. This, not the run-long `min_rtt`, sizes the in-flight
    /// limit.
    path_rtt: Option<Duration>,
    path_rtt_at: SimTime,
    /// A probe is on until this time; it began at `probe_start`, and
    /// `probe_rtt` is the smallest round trip of a packet sent in it.
    probe_until: Option<SimTime>,
    probe_start: SimTime,
    probe_rtt: Option<Duration>,
    /// The demand was positive at the last tick. A burst that begins after
    /// silence restarts the law at its initial rate: the customer has not
    /// heard from the network in a while and comes back at full strength.
    /// That is the moment of impact the handout's settling time covers.
    was_offering: bool,
}

impl FlowEngine {
    pub fn new(demand: Demand, law: Law, packet_size: u64) -> Self {
        Self {
            demand,
            law,
            packet_size: packet_size.max(1),
            law_rate_bps: law.initial_rate(),
            rto_multiplier: 2.0,
            rto_min: Duration::from_millis(1),
            initial_rto: Duration::from_millis(100),
            next_seq: 0,
            outstanding: BTreeMap::new(),
            retransmit_queue: VecDeque::new(),
            resent: std::collections::BTreeSet::new(),
            min_rtt: None,
            srtt: None,
            first_sends: 0,
            retransmits: 0,
            acked: 0,
            losses: 0,
            epoch_start: Duration::ZERO,
            epoch_had_loss: false,
            last_halve_at: None,
            halvings: 0,
            acked_bytes: VecDeque::new(),
            first_ack_at: None,
            matched_bps: None,
            path_rtt: None,
            path_rtt_at: Duration::ZERO,
            probe_until: None,
            probe_start: Duration::ZERO,
            probe_rtt: None,
            was_offering: false,
        }
    }

    fn match_window(&self, min_rtt: Duration) -> Duration {
        (min_rtt * 2).max(RATE_MATCH_MIN_WINDOW)
    }

    /// The slowest a law will go: one packet per RTT (per rate-matching
    /// window), so a flow always probes and can recover.
    fn floor_bps(&self, window: Duration) -> f64 {
        (self.packet_size * 8) as f64 / window.max(Duration::from_micros(1)).as_secs_f64()
    }

    /// The law's reaction to a loss noticed at `now` of a packet that left
    /// at `sent_at`.
    fn law_on_loss(&mut self, now: SimTime, sent_at: SimTime) {
        if let Law::Aimd { .. } = self.law {
            self.epoch_had_loss = true;
            let rtt = self.srtt.unwrap_or_else(|| self.rto());
            // One halving per congestion event. A packet that was already
            // on its way when we last halved was sent at the old rate, and
            // a rate-based sender has no self-clocking: after a halving the
            // queue it filled still has to drain, so packets sent within
            // one timeout of the halving meet the same congestion. Neither
            // says anything about the new rate.
            let recovery = self.rto();
            let due = match self.last_halve_at {
                None => true,
                Some(t) => sent_at > t + recovery,
            };
            if due {
                self.law_rate_bps = (self.law_rate_bps / 2.0).max(self.floor_bps(rtt));
                self.last_halve_at = Some(now);
                self.halvings += 1;
            }
        }
    }

    /// The law's reaction to an ack of `bytes` at `now`.
    fn law_on_ack(&mut self, now: SimTime, bytes: u64) {
        if let Law::RateMatch { .. } = self.law {
            self.acked_bytes.push_back((now, bytes));
            self.first_ack_at.get_or_insert(now);
        }
    }

    /// The law's periodic update, run at the start of every tick.
    fn law_on_tick(&mut self, now: SimTime) {
        match self.law {
            Law::Fixed { .. } => {}
            Law::Aimd {
                increment_bps,
                epoch,
                ..
            } => {
                if !epoch.is_zero() && now >= self.epoch_start + epoch {
                    if !self.epoch_had_loss {
                        self.law_rate_bps += increment_bps;
                    }
                    self.epoch_start = now;
                    self.epoch_had_loss = false;
                }
            }
            Law::RateMatch { .. } => {
                let (Some(min_rtt), Some(first)) = (self.min_rtt, self.first_ack_at) else {
                    return;
                };
                let window = self.match_window(min_rtt);
                if now < first + window {
                    return;
                }
                while let Some((t, _)) = self.acked_bytes.front() {
                    if *t + window <= now {
                        self.acked_bytes.pop_front();
                    } else {
                        break;
                    }
                }
                // Nothing acked and nothing in flight: the customer was
                // silent (an Amazon gap), not starved. Hold the rate.
                if self.acked_bytes.is_empty() && self.outstanding.is_empty() {
                    return;
                }
                let bytes: u64 = self.acked_bytes.iter().map(|(_, b)| *b).sum();
                let matched = (bytes * 8) as f64 / window.as_secs_f64();
                self.matched_bps = Some(matched);
                self.law_rate_bps = (matched * RATE_MATCH_GAIN).max(self.floor_bps(window));
            }
        }
    }

    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    /// The most bytes a rate-matching flow keeps in flight right now.
    /// `None`: no limit (another law, or nothing measured yet).
    pub fn inflight_limit_bytes(&self) -> Option<u64> {
        let Law::RateMatch { .. } = self.law else { return None };
        let floor = RATE_MATCH_MIN_INFLIGHT_PACKETS * self.packet_size;
        if self.probe_until.is_some() {
            return Some(floor);
        }
        let (matched, rtt) = (self.matched_bps?, self.path_rtt?);
        let bdp = matched / 8.0 * rtt.as_secs_f64();
        Some(((RATE_MATCH_INFLIGHT_BDP * bdp) as u64).max(floor))
    }

    /// Start a probe of the round trip when the last minimum is old, and
    /// end one that has run its time.
    fn probe_on_tick(&mut self, now: SimTime) {
        let Law::RateMatch { .. } = self.law else { return };
        match self.probe_until {
            Some(until) if now >= until => {
                if let Some(rtt) = self.probe_rtt {
                    self.path_rtt = Some(rtt);
                }
                self.path_rtt_at = now;
                self.probe_until = None;
            }
            Some(_) => {}
            None => {
                if self.path_rtt.is_some() && now >= self.path_rtt_at + RATE_MATCH_PROBE_INTERVAL {
                    self.probe_start = now;
                    self.probe_until = Some(now + RATE_MATCH_PROBE_TIME);
                    self.probe_rtt = None;
                }
            }
        }
    }

    /// Every seq the sender has not yet seen acknowledged: in flight, or
    /// declared lost and waiting to go again.
    pub fn pending_seqs(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.outstanding.keys().copied().collect();
        v.extend(self.retransmit_queue.iter().copied());
        v.sort_unstable();
        v
    }

    /// Seqs handed out so far (the next new one will be this number).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The retransmission timeout in force right now.
    pub fn rto(&self) -> Duration {
        match self.srtt {
            Some(s) => s.mul_f64(self.rto_multiplier).max(self.rto_min),
            None => self.initial_rto,
        }
    }

    /// The rate the flow would send at right now, and when the demand
    /// next changes.
    pub fn rate_at(&self, now: SimTime) -> (f64, Option<SimTime>) {
        let (offered, change) = self.demand.at(now);
        (offered.min(self.law_rate_bps), change)
    }

    /// Declare every send whose ack is overdue lost, oldest first.
    fn detect_losses(&mut self, now: SimTime) {
        let rto = self.rto();
        let lost: Vec<(u64, SimTime)> = self
            .outstanding
            .iter()
            .filter(|(_, sent_at)| now >= **sent_at + rto)
            .map(|(seq, sent_at)| (*seq, *sent_at))
            .collect();
        for (seq, sent_at) in lost {
            self.outstanding.remove(&seq);
            self.retransmit_queue.push_back(seq);
            self.losses += 1;
            self.law_on_loss(now, sent_at);
        }
    }

    /// One pacing tick. Decides losses, lets the law move, then hands
    /// out at most one packet: a retransmission if any is due, else the
    /// next new seq.
    pub fn tick(&mut self, now: SimTime) -> FlowTick {
        self.detect_losses(now);
        // A burst after silence starts fresh: full initial rate, no memory
        // of the last burst's fight.
        let offering = self.demand.at(now).0 > 0.0;
        if offering && !self.was_offering && self.first_sends > 0 {
            self.law_rate_bps = self.law.initial_rate();
            self.acked_bytes.clear();
            self.first_ack_at = None;
            self.matched_bps = None;
            self.path_rtt = None;
            self.probe_until = None;
            self.epoch_start = now;
            self.epoch_had_loss = false;
        }
        self.was_offering = offering;
        if offering {
            self.probe_on_tick(now);
        }
        self.law_on_tick(now);
        let (rate, change) = self.rate_at(now);
        if !(rate > 0.0 && rate.is_finite()) {
            // Nothing to send now. Wake when the demand comes back, or
            // when the oldest outstanding send would time out, whichever
            // is first, so losses are still noticed while silent.
            let wake = [change, self.next_timeout()]
                .into_iter()
                .flatten()
                .min();
            return FlowTick {
                send: None,
                next: wake.map(|t| t.saturating_sub(now).max(Duration::from_nanos(1))),
            };
        }
        let interval = Duration::from_secs_f64((self.packet_size * 8) as f64 / rate).max(Duration::from_nanos(1));
        // A rate-matching flow with its limit of data in flight waits for
        // an ack (or a timeout) to make room, and looks again one pacing
        // interval later.
        if let Some(limit) = self.inflight_limit_bytes() {
            if (self.outstanding.len() as u64 + 1) * self.packet_size > limit {
                return FlowTick { send: None, next: Some(interval) };
            }
        }
        let (seq, retransmit) = match self.retransmit_queue.pop_front() {
            Some(seq) => {
                self.retransmits += 1;
                self.resent.insert(seq);
                (seq, true)
            }
            None => {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.first_sends += 1;
                (seq, false)
            }
        };
        self.outstanding.insert(seq, now);
        FlowTick {
            send: Some(FlowSend {
                seq,
                size_bytes: self.packet_size,
                retransmit,
            }),
            next: Some(interval),
        }
    }

    fn next_timeout(&self) -> Option<SimTime> {
        let rto = self.rto();
        self.outstanding.values().min().map(|t| *t + rto)
    }

    /// An ack for `seq` arrived. Ignored if the seq is not outstanding
    /// (a duplicate ack, or one for a copy already declared lost).
    pub fn on_ack(&mut self, seq: u64, now: SimTime) {
        // An ack that arrives after we gave up on the seq still means the
        // data got there: cancel the pending retransmission.
        if let Some(pos) = self.retransmit_queue.iter().position(|s| *s == seq) {
            self.retransmit_queue.remove(pos);
            self.resent.remove(&seq);
            self.acked += 1;
            return;
        }
        let Some(sent_at) = self.outstanding.remove(&seq) else {
            return;
        };
        self.acked += 1;
        self.law_on_ack(now, self.packet_size);
        let sample = now.saturating_sub(sent_at);
        self.min_rtt = Some(self.min_rtt.map_or(sample, |m| m.min(sample)));
        // The path's round trip is read only from packets sent once.
        if !self.resent.remove(&seq) {
            if self.path_rtt.map_or(true, |m| sample <= m) {
                self.path_rtt = Some(sample);
                self.path_rtt_at = now;
            }
            if self.probe_until.is_some() && sent_at >= self.probe_start {
                self.probe_rtt = Some(self.probe_rtt.map_or(sample, |m| m.min(sample)));
            }
        }
        self.srtt = Some(match self.srtt {
            None => sample,
            // Jacobson's 1/8 smoothing, in nanoseconds.
            Some(s) => {
                let s = s.as_nanos() as i128;
                let x = sample.as_nanos() as i128;
                Duration::from_nanos((s + (x - s) / 8).max(0) as u64)
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn engine(bps: f64) -> FlowEngine {
        FlowEngine::new(Demand::Constant { bps }, Law::Fixed { bps }, 100)
    }

    #[test]
    fn paces_at_the_offered_rate_and_numbers_packets() {
        // 100-byte packets at 80 kbit/s: one every 10 ms.
        let mut e = engine(80_000.0);
        let t0 = e.tick(Duration::ZERO);
        assert_eq!(t0.send.unwrap().seq, 0);
        assert_eq!(t0.next, Some(ms(10)));
        let t1 = e.tick(ms(10));
        assert_eq!(t1.send.unwrap().seq, 1);
        assert_eq!(e.outstanding(), 2);
        assert_eq!(e.first_sends, 2);
    }

    #[test]
    fn small_packets_pace_at_the_offered_rate() {
        // The slowest A2 trader: 200-byte packets at 50 kbit/s, one every
        // 32 ms. That is 31 a second, over the latency floor's minimum of
        // 20 deliveries a window; at 1000 bytes it would be 6.
        let bps = 50_000.0;
        let mut e = FlowEngine::new(Demand::Constant { bps }, Law::Fixed { bps }, 200);
        let t0 = e.tick(Duration::ZERO);
        assert_eq!(t0.send.unwrap().size_bytes, 200);
        assert_eq!(t0.next, Some(ms(32)));
        assert!(1000 / 32 >= 20);
    }

    #[test]
    fn ack_clears_outstanding_and_measures_rtt() {
        let mut e = engine(80_000.0);
        e.tick(Duration::ZERO);
        e.on_ack(0, ms(3));
        assert_eq!(e.outstanding(), 0);
        assert_eq!(e.acked, 1);
        assert_eq!(e.min_rtt, Some(ms(3)));
        assert_eq!(e.srtt, Some(ms(3)));
        // A second sample smooths in at 1/8.
        e.tick(ms(10));
        e.on_ack(1, ms(10) + ms(11));
        assert_eq!(e.min_rtt, Some(ms(3)));
        assert_eq!(e.srtt, Some(ms(4)));
        // Acking something unknown is harmless.
        e.on_ack(77, ms(30));
        assert_eq!(e.acked, 2);
    }

    #[test]
    fn unacked_send_is_retransmitted_with_the_same_seq_after_the_rto() {
        let mut e = engine(80_000.0);
        e.initial_rto = ms(25);
        e.rto_min = ms(25); // keep the RTO at 25 ms once srtt is known
        e.tick(Duration::ZERO); // seq 0
        e.tick(ms(10)); // seq 1
        e.on_ack(1, ms(12));
        // At 20 ms seq 0 is 20 ms old, under the 25 ms RTO: a new seq.
        assert_eq!(e.tick(ms(20)).send.unwrap().seq, 2);
        // At 30 ms it is overdue: the retransmission goes first.
        let t = e.tick(ms(30)).send.unwrap();
        assert_eq!(t.seq, 0);
        assert!(t.retransmit);
        assert_eq!(e.losses, 1);
        assert_eq!(e.retransmits, 1);
        assert_eq!(e.first_sends, 3);
        // Then normal service resumes.
        let t = e.tick(ms(40)).send.unwrap();
        assert_eq!(t.seq, 3);
        assert!(!t.retransmit);
    }

    #[test]
    fn late_ack_cancels_a_pending_retransmission() {
        let mut e = engine(8_000_000.0); // fast: many ticks before RTO
        e.initial_rto = ms(5);
        e.tick(Duration::ZERO); // seq 0
        // At 5 ms seq 0 is declared lost; but it is queued behind nothing
        // and the tick sends it immediately. Use two flows' worth: send
        // seq 1 too so the loss is queued while a new send goes out.
        e.tick(ms(1)); // seq 1
        e.detect_losses(ms(6));
        assert_eq!(e.losses, 2);
        assert_eq!(e.retransmit_queue.len(), 2);
        e.on_ack(0, ms(6));
        assert_eq!(e.retransmit_queue, VecDeque::from(vec![1]));
        assert_eq!(e.acked, 1);
        assert_eq!(e.tick(ms(7)).send.unwrap().seq, 1);
    }

    #[test]
    fn on_off_demand_is_silent_in_the_off_period_and_wakes_on_time() {
        let d = Demand::OnOff {
            bps: 80_000.0,
            on: ms(100),
            off: ms(400),
            start: ms(50),
        };
        let mut e = FlowEngine::new(d, Law::Fixed { bps: 1e9 }, 100);
        let t = e.tick(Duration::ZERO);
        assert!(t.send.is_none());
        assert_eq!(t.next, Some(ms(50)));
        let t = e.tick(ms(50));
        assert_eq!(t.send.unwrap().seq, 0);
        assert_eq!(t.next, Some(ms(10)));
        e.on_ack(0, ms(55)); // otherwise seq 0 would time out and go first
        let t = e.tick(ms(150));
        assert!(t.send.is_none());
        assert_eq!(t.next, Some(ms(400)));
        assert_eq!(e.tick(ms(550)).send.unwrap().seq, 1);
    }

    /// Drive a flow for `steps` ticks of `dt`. Every send is acked `rtt`
    /// later unless `lose` says to drop it. Returns the law rate after
    /// each tick.
    fn drive(
        e: &mut FlowEngine,
        steps: u32,
        dt: Duration,
        rtt: Duration,
        lose: &dyn Fn(&FlowSend) -> bool,
    ) -> Vec<f64> {
        let mut acks: Vec<(SimTime, u64)> = Vec::new();
        let mut series = Vec::new();
        for i in 0..steps {
            let now = dt * i;
            // Deliver acks that are due.
            acks.retain(|(at, seq)| {
                if *at <= now {
                    e.on_ack(*seq, now);
                    false
                } else {
                    true
                }
            });
            let t = e.tick(now);
            if let Some(s) = t.send {
                if !lose(&s) {
                    acks.push((now + rtt, s.seq));
                }
            }
            series.push(e.law_rate_bps);
        }
        series
    }

    /// Lose the first copy of seqs 3 and 4; their retransmissions get through.
    fn lose_3_and_4_once(s: &FlowSend) -> bool {
        !s.retransmit && (s.seq == 3 || s.seq == 4)
    }

    #[test]
    fn aimd_halves_on_loss_once_per_rtt_and_climbs_when_clean() {
        let law = Law::Aimd {
            initial_bps: 80_000.0,
            increment_bps: 8_000.0,
            epoch: ms(100),
        };
        // 10 ms ticks, 50 ms RTT (RTO 100 ms), 600 ms. Seqs 3 and 4 leave
        // at 30/40 ms, are declared lost at 130/140 ms -- both were in
        // flight at the first halving, so one halving. Epoch [0,100) is
        // clean: +8k to
        // 88k. The halving takes it to 44k. Epoch [100,200) had the loss
        // and earns nothing; 300, 400, 500 each add 8k: 68k at the end.
        let mut e = FlowEngine::new(Demand::Unlimited, law, 100);
        let series = drive(&mut e, 60, ms(10), ms(50), &lose_3_and_4_once);
        assert_eq!(e.halvings, 1, "two losses from one congestion event halve once");
        let min = series.iter().cloned().fold(f64::INFINITY, f64::min);
        assert_eq!(min, 44_000.0, "the cliff: {series:?}");
        assert_eq!(*series.last().unwrap(), 68_000.0, "the climb: {series:?}");
        for w in series.windows(2) {
            assert!(w[1] - w[0] <= 8_000.0 + 1e-6, "no step above the increment: {series:?}");
        }
        // Without loss AIMD climbs linearly: four epochs in 400 ms.
        let mut clean = FlowEngine::new(Demand::Unlimited, law, 100);
        let s = drive(&mut clean, 41, ms(10), ms(50), &|_| false);
        assert_eq!(*s.last().unwrap(), 80_000.0 + 4.0 * 8_000.0);
    }

    /// Run a rate-matching engine for `total`, with every ack arriving
    /// `rtt(now)` after its packet left, and none at all once `silent(now)`.
    /// Calls `check` after every step.
    fn run_rate_match(
        e: &mut FlowEngine,
        total: Duration,
        rtt: &dyn Fn(SimTime) -> Duration,
        silent: &dyn Fn(SimTime) -> bool,
        check: &mut dyn FnMut(&FlowEngine, SimTime),
    ) {
        let step = Duration::from_micros(100);
        let mut acks: Vec<(SimTime, u64)> = Vec::new();
        let mut next_tick = Duration::ZERO;
        let mut now = Duration::ZERO;
        while now <= total {
            let (due, later): (Vec<_>, Vec<_>) = acks.into_iter().partition(|(t, _)| *t <= now);
            acks = later;
            for (_, seq) in due {
                e.on_ack(seq, now);
            }
            if now >= next_tick {
                let t = e.tick(now);
                if let Some(s) = t.send {
                    if !silent(now) {
                        acks.push((now + rtt(now), s.seq));
                    }
                }
                next_tick = now + t.next.unwrap();
            }
            check(e, now);
            now += step;
        }
    }

    #[test]
    fn rate_match_keeps_at_most_two_bdp_in_flight() {
        let law = Law::RateMatch { initial_bps: 4_000_000.0 };
        let mut e = FlowEngine::new(Demand::Unlimited, law, 1000);
        // 10 ms round trips for 400 ms, then the network goes silent: no
        // ack comes back, so nothing makes room.
        // The limit when the silence begins, and the delivered rate it
        // was made from; the most in flight after that.
        let mut at_silence: Option<(u64, f64)> = None;
        let mut most_in_flight = 0u64;
        run_rate_match(&mut e, ms(450), &|_| ms(10), &|now| now >= ms(400), &mut |e, now| {
            if now < ms(400) {
                at_silence = e.inflight_limit_bytes().zip(e.matched_bps);
            } else {
                most_in_flight = most_in_flight.max(e.outstanding() as u64 * 1000);
            }
        });
        let (limit, matched) = at_silence.expect("a limit once a window is measured");
        // 2 x (delivered rate x 10 ms), and never under four packets.
        let bdp = matched / 8.0 * 0.010;
        assert_eq!(limit, ((RATE_MATCH_INFLIGHT_BDP * bdp) as u64).max(4_000));
        assert!(limit > 4_000, "the limit comes from the BDP here: {limit}");
        // With no ack to make room the flow stops there (the limit only
        // falls as the delivered rate falls).
        assert!(most_in_flight <= limit, "{most_in_flight} bytes in flight, limit {limit}");
        assert!(most_in_flight >= limit / 2, "and it does use the limit: {most_in_flight} of {limit}");
        // AIMD has no such limit.
        let aimd = Law::Aimd { initial_bps: 1e6, increment_bps: 1e5, epoch: ms(100) };
        assert_eq!(FlowEngine::new(Demand::Unlimited, aimd, 1000).inflight_limit_bytes(), None);
    }

    #[test]
    fn rate_match_measures_the_round_trip_again_after_a_path_change() {
        let law = Law::RateMatch { initial_bps: 4_000_000.0 };
        let mut e = FlowEngine::new(Demand::Unlimited, law, 1000);
        // The path gets 1.8 times longer at 300 ms. The old minimum
        // stays until the next probe, one interval after it was last seen.
        let rtt = |now: SimTime| if now < ms(300) { ms(10) } else { ms(18) };
        let mut at_1s = None;
        let mut probed = false;
        run_rate_match(&mut e, ms(2_600), &rtt, &|_| false, &mut |e, now| {
            if now == ms(1_000) {
                at_1s = e.path_rtt;
            }
            probed |= e.probe_until.is_some();
        });
        assert_eq!(at_1s, Some(ms(10)), "the old minimum is trusted for one interval");
        assert!(probed, "a probe ran");
        assert_eq!(e.path_rtt, Some(ms(18)), "the probe found the new path's round trip");
        assert_eq!(e.min_rtt, Some(ms(10)), "the run-long minimum is untouched");
    }

    #[test]
    fn rate_match_barely_flinches_at_one_loss_and_declines_under_many() {
        let law = Law::RateMatch { initial_bps: 80_000.0 };
        // Same scenario as the AIMD test, with a customer that wants
        // 80 kbit/s. The window is 2 × 50 ms; at 80 kbit/s that is ten
        // 100-byte packets. With the probing gain the law sits above the
        // demand whenever nothing is lost; losing two packets costs at
        // most two packets' worth for one window and the retransmissions
        // bring it straight back: no cliff, no lasting change.
        let mut e = FlowEngine::new(Demand::Constant { bps: 80_000.0 }, law, 100);
        let series = drive(&mut e, 60, ms(10), ms(50), &lose_3_and_4_once);
        let per_packet = 800.0 / 0.1; // one packet per 100 ms window
        let min = series.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(min >= 80_000.0 - 2.5 * per_packet, "barely flinches: {series:?}");
        assert!(*series.last().unwrap() >= 80_000.0, "asks for at least the demand: {series:?}");
        assert!(
            *series.last().unwrap() <= 80_000.0 * RATE_MATCH_GAIN + per_packet,
            "and no more than the gain over it: {series:?}"
        );
        assert_eq!(e.halvings, 0);

        // Unlimited demand with nothing lost: the gain compounds, so the
        // law climbs every window -- that is how it finds the capacity.
        let mut probe = FlowEngine::new(Demand::Unlimited, law, 100);
        let sp = drive(&mut probe, 100, ms(10), ms(50), &|_| false);
        // (The harness sends at most one packet per 10 ms tick, so the
        // climb stops at 1.25 x that ceiling rather than running away.)
        assert!(*sp.last().unwrap() >= 80_000.0 * RATE_MATCH_GAIN - 1.0, "probes upward: {sp:?}");

        // Under persistent loss of every odd seq (every copy), half of what
        // is sent comes back, so 1.25 × half is a decline: geometric,
        // toward the floor of one packet per window, never climbing.
        let mut e2 = FlowEngine::new(Demand::Unlimited, law, 100);
        let s2 = drive(&mut e2, 300, ms(10), ms(50), &|s| s.seq % 2 == 1);
        let floor = e2.floor_bps(e2.min_rtt.unwrap() * 2);
        let last = *s2.last().unwrap();
        assert!((last - floor).abs() < 1.0, "ends at the floor {floor}: {last}");
        let start = s2.iter().position(|r| *r < 80_000.0 - 1.0).unwrap();
        for w in s2[start..].windows(2) {
            assert!(w[1] <= w[0] + 1e-6, "never climbs under loss: {s2:?}");
        }
        assert!(s2[start..].len() > 5, "the decline takes several windows: {s2:?}");

        // A silent spell is not starvation: an on/off customer comes back
        // at its initial rate after the gap instead of at the floor.
        let onoff = Demand::OnOff {
            bps: 80_000.0,
            on: ms(300),
            off: ms(700),
            start: Duration::ZERO,
        };
        let mut e3 = FlowEngine::new(onoff, law, 100);
        let s3 = drive(&mut e3, 150, ms(10), ms(50), &|_| false);
        // At 1.2 s (tick 120) the second burst is under way at full rate.
        assert!(s3[120] >= 80_000.0, "back at full rate after the gap: {}", s3[120]);
    }

    #[test]
    fn a_burst_after_silence_restarts_the_law_at_its_initial_rate() {
        let law = Law::Aimd {
            initial_bps: 80_000.0,
            increment_bps: 8_000.0,
            epoch: ms(100),
        };
        let onoff = Demand::OnOff {
            bps: 80_000.0,
            on: ms(500),
            off: ms(500),
            start: Duration::ZERO,
        };
        let mut e = FlowEngine::new(onoff, law, 100);
        // Lose the first copies of seqs 3 and 4 during the first burst:
        // one halving, so the burst ends well under 80 kbit/s...
        let s = drive(&mut e, 101, ms(10), ms(50), &lose_3_and_4_once);
        assert!(s[49] < 80_000.0, "halved during the first burst: {}", s[49]);
        assert_eq!(e.halvings, 1);
        // ...and the second burst, at tick 100 (t = 1 s), begins at the
        // initial rate.
        assert_eq!(s[100], 80_000.0, "restarted at initial: {s:?}");
    }

    #[test]
    fn law_rate_caps_unlimited_and_constant_demand() {
        let mut e = FlowEngine::new(Demand::Unlimited, Law::Fixed { bps: 800.0 }, 100);
        assert_eq!(e.tick(Duration::ZERO).next, Some(Duration::from_secs(1)));
        let mut e = FlowEngine::new(
            Demand::Constant { bps: 8_000.0 },
            Law::Fixed { bps: 800.0 },
            100,
        );
        assert_eq!(e.tick(Duration::ZERO).next, Some(Duration::from_secs(1)));
    }
}
