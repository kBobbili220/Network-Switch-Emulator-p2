//! SimpleBGP: a public-promise inter-AS protocol.
//!
//! See `bgp_plan.md` and `docs/student_simple_bgp.md` for the design rationale.
//! This module provides:
//!
//! * AS registry (`AsRegistry`) — public mapping `AsId → AsInfo`.
//! * Native message types mirroring the wire types from `switch_program_types::bgp`.
//! * Validation of received envelopes (`validate_envelope`).
//! * `PromiseLedger` recording every accepted promise and its lifecycle.
//! * `ConformanceMonitor` that grades observed AS-level paths against
//!   active promises.
//!
//! The simulator is wired to:
//!
//! * stamp every packet with a hidden trail (in BGP-configured topologies)
//! * intercept SimpleBGP packets at the receiver's BGP speaker, validate,
//!   and update the ledger
//! * walk the trail when a data packet completes, asking the conformance
//!   monitor to grade it.
//!
//! The host never installs forwarding entries based on BGP. Every routing
//! decision still happens in switch programs.

use crate::packet::{IpAddr, Packet, PacketKind, PuntReason};
use crate::types::{AsId, BgpMsgId, LinkId, PromiseId, SimTime, SwitchId};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use switch_program_types::bgp as wire;

// ---------- Native types (mirror wire) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Prefix {
    pub addr: u32,
    pub len: u8,
}

impl Prefix {
    pub fn new(addr: u32, len: u8) -> Self { Self { addr, len } }
    pub fn contains(&self, ip: u32) -> bool {
        crate::types::Prefix { addr: self.addr, len: self.len }.contains(ip)
    }
}

impl From<wire::PrefixW> for Prefix {
    fn from(p: wire::PrefixW) -> Self { Self { addr: p.addr, len: p.len } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrafficClass {
    BestEffort,
    LowLatency,
    Throughput,
    UptimeSensitive,
    LowLatencyThroughput,
    Vod,
    Gaming,
    Enterprise,
    Backup,
    Hft,
}

impl From<wire::TrafficClassW> for TrafficClass {
    fn from(t: wire::TrafficClassW) -> Self {
        use wire::TrafficClassW as W;
        match t {
            W::BestEffort => TrafficClass::BestEffort,
            W::LowLatency => TrafficClass::LowLatency,
            W::Throughput => TrafficClass::Throughput,
            W::UptimeSensitive => TrafficClass::UptimeSensitive,
            W::LowLatencyThroughput => TrafficClass::LowLatencyThroughput,
            W::Vod => TrafficClass::Vod,
            W::Gaming => TrafficClass::Gaming,
            W::Enterprise => TrafficClass::Enterprise,
            W::Backup => TrafficClass::Backup,
            W::Hft => TrafficClass::Hft,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsPath {
    pub ases: Vec<AsId>,
}

impl From<&wire::AsPathW> for AsPath {
    fn from(p: &wire::AsPathW) -> Self {
        Self { ases: p.ases.iter().map(|a| AsId::new(a.0)).collect() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgpTag {
    NoExport,
    BackupOnly,
    LowLatencyHint,
    BulkOkay,
    Blackhole,
    Experimental(u32),
}

impl From<&wire::BgpTagW> for BgpTag {
    fn from(t: &wire::BgpTagW) -> Self {
        use wire::BgpTagW as W;
        match *t {
            W::NoExport => BgpTag::NoExport,
            W::BackupOnly => BgpTag::BackupOnly,
            W::LowLatencyHint => BgpTag::LowLatencyHint,
            W::BulkOkay => BgpTag::BulkOkay,
            W::Blackhole => BgpTag::Blackhole,
            W::Experimental(v) => BgpTag::Experimental(v),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutePromise {
    pub promise_id: PromiseId,
    pub prefix: Prefix,
    pub traffic_class: Option<TrafficClass>,
    pub promised_paths: Vec<AsPath>,
    pub tags: Vec<BgpTag>,
    pub valid_until: Option<SimTime>,
}

#[derive(Debug, Clone)]
pub struct RouteWithdraw {
    pub withdrawn_promise_id: Option<PromiseId>,
    pub prefix: Option<Prefix>,
    pub traffic_class: Option<TrafficClass>,
}

#[derive(Debug, Clone)]
pub enum SimpleBgpMessage {
    KeepAlive { nonce: u64 },
    Ack { acked_msg_id: BgpMsgId },
    Promise(RoutePromise),
    Withdraw(RouteWithdraw),
}

#[derive(Debug, Clone)]
pub struct BgpEnvelope {
    pub msg_id: BgpMsgId,
    pub sender_as: AsId,
    pub receiver_as: AsId,
    pub sender_bgp_ip: IpAddr,
    pub receiver_bgp_ip: IpAddr,
    pub message: SimpleBgpMessage,
}

impl BgpEnvelope {
    pub fn from_wire(env: wire::BgpEnvelopeW) -> Self {
        let message = match env.message {
            wire::SimpleBgpMessageW::KeepAlive(k) => SimpleBgpMessage::KeepAlive { nonce: k.nonce },
            wire::SimpleBgpMessageW::Ack(a) => SimpleBgpMessage::Ack {
                acked_msg_id: BgpMsgId::new(a.acked_msg_id),
            },
            wire::SimpleBgpMessageW::Promise(p) => SimpleBgpMessage::Promise(RoutePromise {
                promise_id: PromiseId::new(p.promise_id),
                prefix: p.prefix.into(),
                traffic_class: p.traffic_class.map(Into::into),
                promised_paths: p.promised_paths.iter().map(Into::into).collect(),
                tags: p.tags.iter().map(Into::into).collect(),
                valid_until: p.valid_until_ns.map(Duration::from_nanos),
            }),
            wire::SimpleBgpMessageW::Withdraw(w) => SimpleBgpMessage::Withdraw(RouteWithdraw {
                withdrawn_promise_id: w.withdrawn_promise_id.map(PromiseId::new),
                prefix: w.prefix.map(Into::into),
                traffic_class: w.traffic_class.map(Into::into),
            }),
        };
        Self {
            msg_id: BgpMsgId::new(env.msg_id),
            sender_as: AsId::new(env.sender_as.0),
            receiver_as: AsId::new(env.receiver_as.0),
            sender_bgp_ip: IpAddr(env.sender_bgp_ip),
            receiver_bgp_ip: IpAddr(env.receiver_bgp_ip),
            message,
        }
    }
}

// ---------- AS registry ----------

#[derive(Debug, Clone)]
pub struct AsInfo {
    pub as_id: AsId,
    pub border_switch: SwitchId,
    pub bgp_speaker_ip: IpAddr,
    pub owned_prefixes: Vec<Prefix>,
}

#[derive(Debug, Clone, Default)]
pub struct AsRegistry {
    pub ases: HashMap<AsId, AsInfo>,
    /// Reverse lookup: which switches belong to which AS. A switch can be
    /// in at most one AS. The registry can be sparse — switches not listed
    /// here have no AS membership.
    pub switch_to_as: HashMap<SwitchId, AsId>,
}

impl AsRegistry {
    pub fn new() -> Self { Self::default() }

    /// Register an AS along with its border switch and any other interior
    /// switches that belong to it. The border switch is always added to
    /// `switch_to_as`; interior switches must be added explicitly so the
    /// conformance monitor knows which hops are intra-AS.
    pub fn add_as(&mut self, info: AsInfo, interior_switches: &[SwitchId]) {
        let as_id = info.as_id;
        let border = info.border_switch;
        self.ases.insert(as_id, info);
        self.switch_to_as.insert(border, as_id);
        for s in interior_switches {
            self.switch_to_as.insert(*s, as_id);
        }
    }

    pub fn as_for_switch(&self, switch: SwitchId) -> Option<AsId> {
        self.switch_to_as.get(&switch).copied()
    }

    pub fn as_for_ip(&self, ip: IpAddr) -> Option<AsId> {
        self.ases
            .iter()
            .find(|(_, info)| info.bgp_speaker_ip == ip)
            .map(|(id, _)| *id)
    }

    pub fn owns_prefix(&self, as_id: AsId, prefix: Prefix) -> bool {
        match self.ases.get(&as_id) {
            Some(info) => info.owned_prefixes.iter().any(|p| *p == prefix),
            None => false,
        }
    }
}

// ---------- Validation ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgpValidationError {
    InvalidEnvelope,
    UnknownAs,
    UnknownBgpSpeaker,
    InvalidPath,
    UnauthorizedPrefixOrigin,
    WrongReceiver,
}

/// Validate a received envelope against the AS registry.
///
/// `arrived_at_switch` is the border switch the packet actually reached.
/// The receiver's BGP speaker IP and AS are cross-checked against the
/// registry, and (for promises) the sender's prefix-origination authority
/// is checked.
pub fn validate_envelope(
    env: &BgpEnvelope,
    registry: &AsRegistry,
    arrived_at_switch: SwitchId,
) -> Result<(), BgpValidationError> {
    let sender_info = registry.ases.get(&env.sender_as)
        .ok_or(BgpValidationError::UnknownAs)?;
    let receiver_info = registry.ases.get(&env.receiver_as)
        .ok_or(BgpValidationError::UnknownAs)?;

    if sender_info.bgp_speaker_ip != env.sender_bgp_ip {
        return Err(BgpValidationError::UnknownBgpSpeaker);
    }
    if receiver_info.bgp_speaker_ip != env.receiver_bgp_ip {
        return Err(BgpValidationError::UnknownBgpSpeaker);
    }
    if receiver_info.border_switch != arrived_at_switch {
        return Err(BgpValidationError::WrongReceiver);
    }

    if let SimpleBgpMessage::Promise(p) = &env.message {
        if p.promised_paths.is_empty() {
            return Err(BgpValidationError::InvalidPath);
        }
        for path in &p.promised_paths {
            if path.ases.is_empty() {
                return Err(BgpValidationError::InvalidPath);
            }
            if path.ases[0] != env.sender_as {
                return Err(BgpValidationError::InvalidPath);
            }
            // The prefix must be owned by the *terminal* AS in the path
            // (the AS the path claims to deliver to). For a length-1
            // path this is the sender claiming origination of its own
            // prefix; for transit it's the downstream origin AS.
            let terminal = *path.ases.last().unwrap();
            if !registry.owns_prefix(terminal, p.prefix) {
                return Err(BgpValidationError::UnauthorizedPrefixOrigin);
            }
        }
    }
    Ok(())
}

// ---------- Promise ledger ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromiseStatus {
    Active,
    Withdrawn { withdrawn_at: SimTime },
    Expired { expired_at: SimTime },
}

#[derive(Debug, Clone)]
pub struct PromiseLedgerEntry {
    pub promise_id: PromiseId,
    pub sender_as: AsId,
    pub receiver_as: AsId,
    pub prefix: Prefix,
    pub traffic_class: Option<TrafficClass>,
    pub promised_paths: Vec<AsPath>,
    pub tags: Vec<BgpTag>,
    pub received_at: SimTime,
    pub valid_until: Option<SimTime>,
    pub status: PromiseStatus,
}

#[derive(Debug, Clone, Default)]
pub struct PromiseLedger {
    /// Stable insertion-ordered list. Entries are appended; updates mutate
    /// the existing entry in place.
    pub entries: Vec<PromiseLedgerEntry>,
    /// `(sender, receiver, promise_id) -> entries[idx]`.
    index: HashMap<(AsId, AsId, PromiseId), usize>,
    /// Last time we received any BGP packet for `(from, to)` AS pair.
    pub last_seen: HashMap<(AsId, AsId), SimTime>,
}

impl PromiseLedger {
    pub fn new() -> Self { Self::default() }

    fn record_seen(&mut self, from: AsId, to: AsId, now: SimTime) {
        self.last_seen.insert((from, to), now);
    }

    /// Insert or update a Promise.
    pub fn record_promise(
        &mut self,
        sender: AsId,
        receiver: AsId,
        p: &RoutePromise,
        now: SimTime,
    ) {
        self.record_seen(sender, receiver, now);
        let key = (sender, receiver, p.promise_id);
        if let Some(&idx) = self.index.get(&key) {
            // Duplicate id from same sender to same receiver: refresh in place.
            let e = &mut self.entries[idx];
            e.prefix = p.prefix;
            e.traffic_class = p.traffic_class;
            e.promised_paths = p.promised_paths.clone();
            e.tags = p.tags.clone();
            e.received_at = now;
            e.valid_until = p.valid_until;
            e.status = PromiseStatus::Active;
            return;
        }
        let entry = PromiseLedgerEntry {
            promise_id: p.promise_id,
            sender_as: sender,
            receiver_as: receiver,
            prefix: p.prefix,
            traffic_class: p.traffic_class,
            promised_paths: p.promised_paths.clone(),
            tags: p.tags.clone(),
            received_at: now,
            valid_until: p.valid_until,
            status: PromiseStatus::Active,
        };
        let idx = self.entries.len();
        self.entries.push(entry);
        self.index.insert(key, idx);
    }

    /// Apply a withdrawal received by `receiver` from `sender`. Withdraws
    /// active entries that match.
    pub fn record_withdraw(
        &mut self,
        sender: AsId,
        receiver: AsId,
        w: &RouteWithdraw,
        now: SimTime,
    ) {
        self.record_seen(sender, receiver, now);
        for e in self.entries.iter_mut() {
            if !matches!(e.status, PromiseStatus::Active) {
                continue;
            }
            if e.sender_as != sender || e.receiver_as != receiver {
                continue;
            }
            // Match on whichever fields are set.
            if let Some(pid) = w.withdrawn_promise_id {
                if e.promise_id != pid {
                    continue;
                }
            }
            if let Some(prefix) = w.prefix {
                if e.prefix != prefix {
                    continue;
                }
            }
            if let Some(tc) = w.traffic_class {
                if e.traffic_class != Some(tc) {
                    continue;
                }
            }
            e.status = PromiseStatus::Withdrawn { withdrawn_at: now };
        }
    }

    /// Sweep `valid_until` < now and mark expired.
    pub fn expire(&mut self, now: SimTime) {
        for e in self.entries.iter_mut() {
            if matches!(e.status, PromiseStatus::Active) {
                if let Some(deadline) = e.valid_until {
                    if now >= deadline {
                        e.status = PromiseStatus::Expired { expired_at: now };
                    }
                }
            }
        }
    }

    pub fn active_promises(&self) -> Vec<&PromiseLedgerEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, PromiseStatus::Active))
            .collect()
    }

    /// Active promises that the receiver `r` accepted from `s` for prefix
    /// `p`. If `class` is specified, prefer same-class promises; otherwise
    /// fall back to all-class promises.
    pub fn active_for(
        &self,
        sender: AsId,
        receiver: AsId,
        prefix_ip: u32,
        class: Option<TrafficClass>,
    ) -> Vec<&PromiseLedgerEntry> {
        let candidates: Vec<&PromiseLedgerEntry> = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, PromiseStatus::Active))
            .filter(|e| e.sender_as == sender && e.receiver_as == receiver)
            .filter(|e| e.prefix.contains(prefix_ip))
            .collect();
        // Class-matching preference.
        if let Some(c) = class {
            let matched: Vec<_> = candidates.iter().copied()
                .filter(|e| e.traffic_class == Some(c)).collect();
            if !matched.is_empty() {
                return matched;
            }
        }
        candidates.into_iter()
            .filter(|e| e.traffic_class.is_none() || class.is_none())
            .collect()
    }
}

// ---------- Conformance monitor ----------

#[derive(Debug, Clone, Copy, Default)]
pub struct PromiseConformanceStats {
    pub packets_honored: u64,
    pub bytes_honored: u64,
    pub packets_violated: u64,
    pub bytes_violated: u64,
    pub packets_no_promise: u64,
    pub bytes_no_promise: u64,
}

impl PromiseConformanceStats {
    /// Fraction of observed packets that diverged from this promise's
    /// stated paths. `None` when no packet has been graded yet.
    pub fn violation_fraction(&self) -> Option<f64> {
        let total = self.packets_honored + self.packets_violated;
        if total == 0 { None } else { Some(self.packets_violated as f64 / total as f64) }
    }

    pub fn compliance_fraction(&self) -> Option<f64> {
        self.violation_fraction().map(|v| 1.0 - v)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AsConformanceSummary {
    pub promises_sent: u64,
    pub promises_withdrawn: u64,
    pub packets_honored: u64,
    pub packets_violated: u64,
    pub bytes_honored: u64,
    pub bytes_violated: u64,
}

impl AsConformanceSummary {
    /// Fraction of observed transit packets where this AS broke a
    /// promise it had made. `None` when no packet has been graded.
    pub fn violation_fraction(&self) -> Option<f64> {
        let total = self.packets_honored + self.packets_violated;
        if total == 0 { None } else { Some(self.packets_violated as f64 / total as f64) }
    }

    pub fn compliance_fraction(&self) -> Option<f64> {
        self.violation_fraction().map(|v| 1.0 - v)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConformanceMonitor {
    pub per_promise: HashMap<PromiseId, PromiseConformanceStats>,
    pub per_as: HashMap<AsId, AsConformanceSummary>,
}

impl ConformanceMonitor {
    pub fn new() -> Self { Self::default() }

    pub fn record_promise_sent(&mut self, sender: AsId) {
        self.per_as.entry(sender).or_default().promises_sent += 1;
    }
    pub fn record_promise_withdrawn(&mut self, sender: AsId) {
        self.per_as.entry(sender).or_default().promises_withdrawn += 1;
    }

    /// Walk the AS-path of a packet that has just terminated (delivered or
    /// dropped) and grade each inter-AS hop against the active ledger.
    pub fn observe_packet(
        &mut self,
        packet_size: u64,
        ip_dst: u32,
        as_path: &[AsId],
        ledger: &PromiseLedger,
    ) {
        // For each consecutive pair (prev_as, this_as) we check: this_as
        // received a packet from prev_as for ip_dst; do its active
        // promises to prev_as cover this packet, and does the actual
        // remaining AS-path match one of the promised continuations?
        //
        // remaining AS-path from this_as onward = as_path[i..].
        if as_path.len() < 2 {
            return;
        }
        for i in 1..as_path.len() {
            let prev_as = as_path[i - 1];
            let this_as = as_path[i];
            if prev_as == this_as {
                continue;
            }
            // Active promises this_as made to prev_as for ip_dst.
            let promises = ledger.active_for(this_as, prev_as, ip_dst, None);
            if promises.is_empty() {
                let s = self.per_as.entry(this_as).or_default();
                // Aggregate "no-promise" bytes go untracked at AS level.
                let _ = s;
                // Per-promise nothing to record. Per-pair just count once.
                self.per_pair_no_promise(packet_size);
                continue;
            }
            // Observed continuation: the AS-path from this_as onward.
            let observed: Vec<AsId> = as_path[i..].to_vec();
            let mut honored = false;
            let mut matched_promise: Option<PromiseId> = None;
            for promise in &promises {
                for path in &promise.promised_paths {
                    if observed == path.ases {
                        honored = true;
                        matched_promise = Some(promise.promise_id);
                        break;
                    }
                }
                if honored { break; }
            }
            if honored {
                if let Some(pid) = matched_promise {
                    let s = self.per_promise.entry(pid).or_default();
                    s.packets_honored += 1;
                    s.bytes_honored += packet_size;
                }
                let a = self.per_as.entry(this_as).or_default();
                a.packets_honored += 1;
                a.bytes_honored += packet_size;
            } else {
                // All matching promises violated; charge each one.
                for promise in &promises {
                    let s = self.per_promise.entry(promise.promise_id).or_default();
                    s.packets_violated += 1;
                    s.bytes_violated += packet_size;
                }
                let a = self.per_as.entry(this_as).or_default();
                a.packets_violated += 1;
                a.bytes_violated += packet_size;
            }
        }
    }

    fn per_pair_no_promise(&mut self, _size: u64) {
        // Reserved for future per-pair tracking; aggregate stats already
        // capture honored/violated.
    }

    pub fn for_promise(&self, id: PromiseId) -> PromiseConformanceStats {
        self.per_promise.get(&id).copied().unwrap_or_default()
    }
    pub fn for_as(&self, id: AsId) -> AsConformanceSummary {
        self.per_as.get(&id).copied().unwrap_or_default()
    }
}

// ---------- Top-level BGP state stored on Simulator ----------

#[derive(Debug, Default)]
pub struct BgpState {
    pub registry: AsRegistry,
    pub ledger: PromiseLedger,
    pub monitor: ConformanceMonitor,
    /// SimpleBGP packets that have been delivered to a BGP speaker but
    /// failed validation, kept for debugging.
    pub rejected: Vec<(BgpEnvelope, BgpValidationError)>,
}

impl BgpState {
    pub fn new(registry: AsRegistry) -> Self {
        Self {
            registry,
            ..Default::default()
        }
    }

    /// Called by the simulator each time a SimpleBGP packet has been
    /// delivered to a switch's CPU. Validates and updates the ledger.
    pub fn handle_simple_bgp(&mut self, switch: SwitchId, packet: &Packet, now: SimTime) {
        let env = match wire::decode_envelope(&packet.payload) {
            Ok(e) => BgpEnvelope::from_wire(e),
            Err(_) => return,
        };
        if let Err(err) = validate_envelope(&env, &self.registry, switch) {
            self.rejected.push((env, err));
            return;
        }
        match &env.message {
            SimpleBgpMessage::Promise(p) => {
                self.ledger.record_promise(env.sender_as, env.receiver_as, p, now);
                self.monitor.record_promise_sent(env.sender_as);
            }
            SimpleBgpMessage::Withdraw(w) => {
                self.ledger.record_withdraw(env.sender_as, env.receiver_as, w, now);
                self.monitor.record_promise_withdrawn(env.sender_as);
            }
            SimpleBgpMessage::KeepAlive { .. } | SimpleBgpMessage::Ack { .. } => {
                self.ledger.record_seen(env.sender_as, env.receiver_as, now);
            }
        }
    }

    /// Walk a packet's hidden trail and ask the conformance monitor to
    /// grade it. Called when a packet is delivered or dropped.
    pub fn grade_packet(&mut self, packet: &Packet, now: SimTime) {
        self.ledger.expire(now);
        let mut as_path: Vec<AsId> = Vec::with_capacity(packet.trail.hops.len());
        for hop in &packet.trail.hops {
            if let Some(asn) = hop.as_id {
                if as_path.last().copied() != Some(asn) {
                    as_path.push(asn);
                }
            }
        }
        if as_path.len() < 2 {
            return;
        }
        self.monitor.observe_packet(packet.size_bytes, packet.ip_dst.0, &as_path, &self.ledger);
    }

    pub fn last_bgp_packet_time(&self, from: AsId, to: AsId) -> Option<SimTime> {
        self.ledger.last_seen.get(&(from, to)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SwitchId as Sw;

    fn registry() -> AsRegistry {
        let mut r = AsRegistry::new();
        r.add_as(
            AsInfo {
                as_id: AsId::new(1),
                border_switch: Sw::new(0),
                bgp_speaker_ip: IpAddr(0xc0_a8_01_01),
                owned_prefixes: vec![Prefix::new(0x0a010000, 16)],
            },
            &[Sw::new(10)],
        );
        r.add_as(
            AsInfo {
                as_id: AsId::new(2),
                border_switch: Sw::new(1),
                bgp_speaker_ip: IpAddr(0xc0_a8_01_02),
                owned_prefixes: vec![Prefix::new(0x0a020000, 16)],
            },
            &[],
        );
        r
    }

    fn promise_envelope(
        msg_id: u64,
        sender: AsId,
        receiver: AsId,
        sender_ip: u32,
        receiver_ip: u32,
        prefix: Prefix,
        paths: Vec<Vec<AsId>>,
    ) -> BgpEnvelope {
        BgpEnvelope {
            msg_id: BgpMsgId::new(msg_id),
            sender_as: sender,
            receiver_as: receiver,
            sender_bgp_ip: IpAddr(sender_ip),
            receiver_bgp_ip: IpAddr(receiver_ip),
            message: SimpleBgpMessage::Promise(RoutePromise {
                promise_id: PromiseId::new(msg_id),
                prefix,
                traffic_class: None,
                promised_paths: paths.into_iter()
                    .map(|p| AsPath { ases: p }).collect(),
                tags: vec![],
                valid_until: None,
            }),
        }
    }

    #[test]
    fn validate_accepts_well_formed() {
        let r = registry();
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a020000, 16),
            vec![vec![AsId::new(2)]],
        );
        assert!(validate_envelope(&env, &r, Sw::new(0)).is_ok());
    }

    #[test]
    fn validate_accepts_transit_when_terminal_owns_prefix() {
        let r = registry();
        // AS2 -> AS1, transit path [2, 1] for AS1's own prefix. AS1 is
        // the terminal AS and owns 10.1.0.0/16, so accept.
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a010000, 16),
            vec![vec![AsId::new(2), AsId::new(1)]],
        );
        assert!(validate_envelope(&env, &r, Sw::new(0)).is_ok());
    }

    #[test]
    fn validate_rejects_transit_when_terminal_doesnt_own_prefix() {
        let r = registry();
        // AS2 -> AS1, transit path [2, 1] but for AS2's prefix. The
        // terminal (AS1) doesn't own 10.2.0.0/16.
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a020000, 16),
            vec![vec![AsId::new(2), AsId::new(1)]],
        );
        assert_eq!(
            validate_envelope(&env, &r, Sw::new(0)),
            Err(BgpValidationError::UnauthorizedPrefixOrigin),
        );
    }

    #[test]
    fn validate_rejects_unauthorized_origin() {
        let r = registry();
        // AS 2 trying to advertise AS 1's prefix.
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a010000, 16),
            vec![vec![AsId::new(2)]],
        );
        assert_eq!(
            validate_envelope(&env, &r, Sw::new(0)),
            Err(BgpValidationError::UnauthorizedPrefixOrigin),
        );
    }

    #[test]
    fn validate_rejects_path_not_starting_with_sender() {
        let r = registry();
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a020000, 16),
            vec![vec![AsId::new(99)]],
        );
        assert_eq!(
            validate_envelope(&env, &r, Sw::new(0)),
            Err(BgpValidationError::InvalidPath),
        );
    }

    #[test]
    fn validate_rejects_wrong_receiver_switch() {
        let r = registry();
        let env = promise_envelope(
            1, AsId::new(2), AsId::new(1),
            0xc0_a8_01_02, 0xc0_a8_01_01,
            Prefix::new(0x0a020000, 16),
            vec![vec![AsId::new(2)]],
        );
        assert_eq!(
            validate_envelope(&env, &r, Sw::new(99)),
            Err(BgpValidationError::WrongReceiver),
        );
    }

    #[test]
    fn ledger_records_and_withdraws() {
        let mut l = PromiseLedger::new();
        let p = RoutePromise {
            promise_id: PromiseId::new(7),
            prefix: Prefix::new(0x0a020000, 16),
            traffic_class: None,
            promised_paths: vec![AsPath { ases: vec![AsId::new(2), AsId::new(3)] }],
            tags: vec![],
            valid_until: None,
        };
        l.record_promise(AsId::new(2), AsId::new(1), &p, Duration::from_millis(100));
        assert_eq!(l.entries.len(), 1);
        assert!(matches!(l.entries[0].status, PromiseStatus::Active));

        let w = RouteWithdraw {
            withdrawn_promise_id: Some(PromiseId::new(7)),
            prefix: None,
            traffic_class: None,
        };
        l.record_withdraw(AsId::new(2), AsId::new(1), &w, Duration::from_millis(200));
        assert!(matches!(l.entries[0].status, PromiseStatus::Withdrawn { .. }));
    }

    #[test]
    fn ledger_expires() {
        let mut l = PromiseLedger::new();
        let p = RoutePromise {
            promise_id: PromiseId::new(7),
            prefix: Prefix::new(0x0a020000, 16),
            traffic_class: None,
            promised_paths: vec![AsPath { ases: vec![AsId::new(2)] }],
            tags: vec![],
            valid_until: Some(Duration::from_millis(150)),
        };
        l.record_promise(AsId::new(2), AsId::new(1), &p, Duration::from_millis(100));
        l.expire(Duration::from_millis(160));
        assert!(matches!(l.entries[0].status, PromiseStatus::Expired { .. }));
    }

    #[test]
    fn conformance_grades_observed_path() {
        let mut l = PromiseLedger::new();
        // AS2 promised AS1 a path [2,3] for prefix 10.3/16.
        let p = RoutePromise {
            promise_id: PromiseId::new(42),
            prefix: Prefix::new(0x0a030000, 16),
            traffic_class: None,
            promised_paths: vec![
                AsPath { ases: vec![AsId::new(2), AsId::new(3)] },
            ],
            tags: vec![],
            valid_until: None,
        };
        l.record_promise(AsId::new(2), AsId::new(1), &p, Duration::ZERO);
        let mut m = ConformanceMonitor::new();
        // AS path 1 -> 2 -> 3, packet to 10.3.0.5
        let path = vec![AsId::new(1), AsId::new(2), AsId::new(3)];
        m.observe_packet(100, 0x0a030005, &path, &l);
        let s = m.for_promise(PromiseId::new(42));
        assert_eq!(s.packets_honored, 1);
        assert_eq!(s.packets_violated, 0);

        // Bad path: 1 -> 2 -> 4 -> 3 (deviates from promise).
        let bad = vec![AsId::new(1), AsId::new(2), AsId::new(4), AsId::new(3)];
        m.observe_packet(100, 0x0a030005, &bad, &l);
        let s = m.for_promise(PromiseId::new(42));
        assert_eq!(s.packets_honored, 1);
        assert_eq!(s.packets_violated, 1);
    }
}

// Avoid unused import warnings in non-test builds.
#[allow(dead_code)]
fn _link_id_kept(_l: LinkId) {}
#[allow(dead_code)]
fn _hashset_kept<T: std::hash::Hash + Eq>(_: HashSet<T>) {}
#[allow(dead_code)]
fn _packet_kind_kept(_: PacketKind) {}
#[allow(dead_code)]
fn _punt_reason_kept(_: PuntReason) {}
