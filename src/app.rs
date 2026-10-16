use crate::egress::EgressPort;
use crate::flow::FlowEngine;
use crate::packet::{IpAddr, Packet, PacketKind};
use crate::types::{AppId, FlowId, PacketId, SimTime};
use std::collections::HashSet;
use std::time::Duration;

fn ip_in_prefix(ip: u32, prefix_addr: u32, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len >= 32 {
        return ip == prefix_addr;
    }
    let mask: u32 = !0u32 << (32 - prefix_len);
    (ip & mask) == (prefix_addr & mask)
}

#[derive(Debug, Clone, Default)]
pub struct AppMetrics {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub avg_delay: Duration,
    pub p50_delay: Duration,
    pub p95_delay: Duration,
    pub p99_delay: Duration,
    pub loss_rate: f64,
    pub throughput_bps: f64,
    /// Packets received, keyed by sender address.
    pub received_by_src: std::collections::BTreeMap<u32, u64>,
    /// Of `packets_received`, copies of a `(flow, seq)` already seen:
    /// retransmissions that arrived after the original did.
    pub duplicates_received: u64,
    /// Acks this app sent back to closed-loop senders.
    pub acks_sent: u64,
    /// Acks this app received for its own flow.
    pub acks_received: u64,

    /// Internal: per-packet observed delays for percentile recomputation.
    delay_samples: Vec<Duration>,
    /// Internal: sender of each entry in `delay_samples`.
    delay_srcs: Vec<u32>,
    /// Internal: total simulated time the app has run, for throughput calc.
    last_recv_time: Duration,
    /// Internal: a receive happened since the summary fields were last
    /// computed. `finalize` clears it.
    stale: bool,
}

impl AppMetrics {
    pub fn record_send(&mut self, packet: &Packet) {
        self.packets_sent += 1;
        self.bytes_sent += packet.size_bytes;
        self.stale = true;
    }

    /// Count a delivery. The summary fields (`avg_delay`, the
    /// percentiles, `throughput_bps`, `loss_rate`) are not updated here
    /// -- sorting every sample on every packet made a busy run quadratic
    /// -- but by `finalize`, which the simulator calls when a run stops.
    pub fn record_recv(&mut self, packet: &Packet, now: SimTime) {
        self.packets_received += 1;
        self.bytes_received += packet.size_bytes;
        let delay = now.saturating_sub(packet.created_at);
        self.delay_samples.push(delay);
        self.delay_srcs.push(packet.ip_src.0);
        *self.received_by_src.entry(packet.ip_src.0).or_insert(0) += 1;
        self.last_recv_time = now;
        self.stale = true;
    }

    /// Bring the summary fields up to date with the samples.
    pub fn finalize(&mut self) {
        if self.stale {
            self.recompute();
            self.stale = false;
        }
    }

    /// Every observed one-way delay, in delivery order.
    pub fn delays(&self) -> &[Duration] {
        &self.delay_samples
    }

    /// The one-way delays of packets that came from `src`, in delivery order.
    pub fn delays_from(&self, src: u32) -> impl Iterator<Item = Duration> + '_ {
        self.delay_srcs
            .iter()
            .zip(&self.delay_samples)
            .filter(move |(s, _)| **s == src)
            .map(|(_, d)| *d)
    }

    fn recompute(&mut self) {
        if self.packets_sent > 0 {
            let lost = self.packets_sent.saturating_sub(self.packets_received);
            self.loss_rate = lost as f64 / self.packets_sent as f64;
        }
        if self.delay_samples.is_empty() {
            return;
        }
        let total: u128 = self.delay_samples.iter().map(|d| d.as_nanos()).sum();
        self.avg_delay = Duration::from_nanos((total / self.delay_samples.len() as u128) as u64);

        let mut sorted = self.delay_samples.clone();
        sorted.sort();
        self.p50_delay = sorted[sorted.len() * 50 / 100];
        self.p95_delay = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
        self.p99_delay = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];

        if self.last_recv_time > Duration::ZERO {
            self.throughput_bps =
                (self.bytes_received * 8) as f64 / self.last_recv_time.as_secs_f64();
        }
    }
}

/// Traffic generation pattern.
///
/// `next_send` returns the next time relative to `now` that the app should
/// send a packet, plus the size of that packet, or None if the pattern is
/// finished. A simple deterministic RNG state is carried by each variant.
#[derive(Debug, Clone)]
pub enum TrafficPattern {
    /// Constant-bitrate: one packet every `interval`, fixed size.
    ConstantBitrate {
        interval: Duration,
        size_bytes: u64,
    },
    /// Poisson arrivals with mean rate `lambda_pps`. Uses a deterministic
    /// xorshift seeded by `seed`.
    Poisson {
        lambda_pps: f64,
        size_bytes: u64,
        seed: u64,
    },
    /// Bursty ON/OFF: during ON sends a packet every `on_interval`, then
    /// stays silent for `off_duration`.
    BurstyOnOff {
        on_duration: Duration,
        off_duration: Duration,
        on_interval: Duration,
        size_bytes: u64,
        /// Internal: current ON window started at this absolute time;
        /// `None` means we haven't started yet.
        on_started: Option<SimTime>,
        /// Internal: number of packets sent in current ON window.
        in_window: u32,
    },
    /// Request/response: sends a request packet, then waits `gap` after
    /// receiving the corresponding reply before sending the next request.
    /// Internal state: whether we are currently waiting for a reply.
    RequestResponse {
        gap: Duration,
        size_bytes: u64,
        waiting: bool,
    },
    /// Bulk transfer: send `total_bytes` worth of packets back-to-back as
    /// fast as `pacing` allows.
    BulkTransfer {
        total_bytes: u64,
        sent_bytes: u64,
        pacing: Duration,
        size_bytes: u64,
    },
}

impl TrafficPattern {
    /// Compute when the next packet should be generated, relative to `now`.
    /// Returns `None` if the pattern is finished.
    fn next_delay(&mut self) -> Option<(Duration, u64)> {
        match self {
            Self::ConstantBitrate {
                interval,
                size_bytes,
            } => Some((*interval, *size_bytes)),
            Self::Poisson {
                lambda_pps,
                size_bytes,
                seed,
            } => {
                if *lambda_pps <= 0.0 {
                    return None;
                }
                // xorshift64 -> [0,1)
                let mut s = *seed;
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                *seed = s;
                let u = (s as f64 / u64::MAX as f64).clamp(1e-12, 1.0 - 1e-12);
                let dt = -u.ln() / *lambda_pps;
                Some((Duration::from_secs_f64(dt), *size_bytes))
            }
            Self::BurstyOnOff {
                on_duration,
                off_duration: _,
                on_interval,
                size_bytes,
                on_started: _,
                in_window: _,
            } => {
                // Pure interval-based generation; gating is handled in `tick`.
                let _ = on_duration;
                Some((*on_interval, *size_bytes))
            }
            Self::RequestResponse {
                gap: _,
                size_bytes,
                waiting,
            } => {
                if *waiting {
                    None
                } else {
                    *waiting = true;
                    Some((Duration::ZERO, *size_bytes))
                }
            }
            Self::BulkTransfer {
                total_bytes,
                sent_bytes,
                pacing,
                size_bytes,
            } => {
                if *sent_bytes >= *total_bytes {
                    None
                } else {
                    *sent_bytes += *size_bytes;
                    Some((*pacing, *size_bytes))
                }
            }
        }
    }
}

/// An endpoint connected to the network.
pub struct App {
    pub id: AppId,
    /// The single source IP this app stamps on packets it generates.
    pub ip: IpAddr,
    /// Length of the prefix this app *owns* (incoming packets whose
    /// `ip_dst` falls within `ip & mask` are accepted by `deliver`). Set
    /// to 32 for single-IP apps.
    pub prefix_len: u8,
    pub flow_id: FlowId,
    pub pattern: TrafficPattern,
    /// Destination IP this app aims at. Used as `ip_dst` on every packet.
    pub destination: IpAddr,
    /// Prefix length of the destination address. Currently informational
    /// only (the simulator routes by exact `ip_dst`); the viewer surfaces
    /// it for context.
    pub destination_prefix_len: u8,
    /// Extra destinations this app cycles through, round-robin, after
    /// `destination`. Empty means the original single-destination
    /// behaviour. Used for all-pairs workloads, where one app must reach
    /// every other app.
    pub extra_destinations: Vec<IpAddr>,
    pub metrics: AppMetrics,
    /// The app's single outgoing port (port 0). Sized from the link's
    /// `queue_capacity_bytes` when the app is connected.
    pub egress: EgressPort,
    pub buffer_used: u64,
    pub buffer_budget: u64,
    /// When set, this app is an A2 customer: `flow` decides what to send
    /// and `pattern` is ignored. Its data packets carry `seq`, the
    /// receiver acks each one, and unacked sends are retransmitted.
    pub flow: Option<FlowEngine>,
    /// `(flow, seq)` pairs already delivered here, for duplicate counting.
    seen: HashSet<(u64, u64)>,
    /// Send one empty, self-addressed packet on the first tick so the
    /// switch this app hangs off sees its address before any route
    /// exists -- the way a host announces itself to its gateway. Without
    /// it a customer that stays quiet until routes have converged is
    /// never punted, so a punt-learning program never finds it.
    announce_pending: bool,
    next_packet_id: u64,
    /// Internal: cursor over `[destination] ++ extra_destinations`.
    next_dest_idx: usize,
}

/// Size of an acknowledgement packet, in bytes.
pub const ACK_SIZE_BYTES: u64 = 40;

/// What one app tick asks the simulator to do.
pub struct AppTick {
    pub packet: Option<Packet>,
    /// When to tick again; `None` means never.
    pub next: Option<Duration>,
}

impl App {
    /// Construct an app with explicit source/destination prefixes.
    /// `prefix_len = 32` (and `destination_prefix_len = 32`) preserves
    /// the original single-IP behavior.
    pub fn new(
        id: AppId,
        ip: IpAddr,
        prefix_len: u8,
        destination: IpAddr,
        destination_prefix_len: u8,
        pattern: TrafficPattern,
    ) -> Self {
        Self {
            id,
            ip,
            prefix_len,
            flow_id: FlowId::new(id.raw() as u64),
            pattern,
            destination,
            destination_prefix_len,
            extra_destinations: Vec::new(),
            metrics: AppMetrics::default(),
            egress: EgressPort::single_fifo(u64::MAX),
            buffer_used: 0,
            buffer_budget: u64::MAX,
            flow: None,
            seen: HashSet::new(),
            announce_pending: false,
            next_packet_id: 0,
            next_dest_idx: 0,
        }
    }

    /// Make this app a closed-loop customer driven by `flow`.
    pub fn with_flow(mut self, flow: FlowEngine) -> Self {
        self.flow = Some(flow);
        self
    }

    /// Announce this app's address to its switch on the first tick (see
    /// `announce_pending`). The packet is empty and addressed to the app
    /// itself, so no scorer counts it.
    pub fn with_announce(mut self) -> Self {
        self.announce_pending = true;
        self
    }

    /// Add another destination to the round-robin set. Each generated
    /// packet targets the next destination in `[destination] ++
    /// extra_destinations`, so an app with N-1 extras spreads its traffic
    /// evenly over all N-1 peers.
    pub fn add_destination(&mut self, ip: IpAddr) {
        self.extra_destinations.push(ip);
    }

    /// Every destination this app targets, in round-robin order.
    pub fn destinations(&self) -> Vec<IpAddr> {
        let mut v = Vec::with_capacity(1 + self.extra_destinations.len());
        v.push(self.destination);
        v.extend_from_slice(&self.extra_destinations);
        v
    }

    /// Called by the simulator at the scheduled tick time.
    pub fn tick(&mut self, now: SimTime) -> AppTick {
        if self.announce_pending {
            self.announce_pending = false;
            let pid = PacketId::new(((self.id.raw() as u64) << 32) | self.next_packet_id);
            self.next_packet_id += 1;
            let mut p = Packet::new(pid, now, 0);
            p.app_id = self.id;
            p.flow_id = self.flow_id;
            p.ip_src = self.ip;
            p.ip_dst = self.ip;
            // Tick again at once for the real traffic.
            return AppTick {
                packet: Some(p),
                next: Some(Duration::ZERO),
            };
        }
        if self.flow.is_some() {
            return self.flow_tick(now);
        }
        match self.pattern_tick(now) {
            Some((packet, next)) => AppTick {
                packet: Some(packet),
                next: Some(next),
            },
            None => AppTick {
                packet: None,
                next: None,
            },
        }
    }

    fn flow_tick(&mut self, now: SimTime) -> AppTick {
        let out = self.flow.as_mut().expect("flow_tick without a flow").tick(now);
        let packet = out.send.map(|s| {
            let mut p = self
                .build_packet(now, s.size_bytes)
                .expect("build_packet always succeeds");
            p.seq = Some(s.seq);
            p
        });
        AppTick {
            packet,
            next: out.next,
        }
    }

    /// The open-loop generators. Returns the packet and the delay to the
    /// next tick, or `None` when the pattern is exhausted.
    fn pattern_tick(&mut self, now: SimTime) -> Option<(Packet, Duration)> {
        // Bursty pattern needs gating handled at the tick boundary.
        let bursty_result: Option<(u64, Duration)> = if let TrafficPattern::BurstyOnOff {
            on_duration,
            off_duration,
            on_interval,
            size_bytes,
            on_started,
            in_window,
        } = &mut self.pattern
        {
            // Decide: are we in an ON window?
            let started = on_started.get_or_insert(now);
            let in_on = now.saturating_sub(*started) < *on_duration;
            if !in_on {
                let off_end = *started + *on_duration + *off_duration;
                let next_start = if now >= off_end { now } else { off_end };
                *on_started = Some(next_start);
                *in_window = 0;
                let delay = next_start.saturating_sub(now);
                let next_delay = if delay.is_zero() { *on_interval } else { delay };
                Some((*size_bytes, next_delay))
            } else {
                *in_window += 1;
                Some((*size_bytes, *on_interval))
            }
        } else {
            None
        };

        if let Some((size, next_delay)) = bursty_result {
            let pkt = self.build_packet(now, size)?;
            return Some((pkt, next_delay));
        }

        let (delay, size) = self.pattern.next_delay()?;
        let pkt = self.build_packet(now, size)?;
        Some((pkt, delay))
    }

    fn build_packet(&mut self, now: SimTime, size_bytes: u64) -> Option<Packet> {
        let pid = PacketId::new(((self.id.raw() as u64) << 32) | self.next_packet_id);
        self.next_packet_id += 1;
        let mut p = Packet::new(pid, now, size_bytes);
        p.app_id = self.id;
        p.flow_id = self.flow_id;
        p.ip_src = self.ip;
        p.ip_dst = if self.extra_destinations.is_empty() {
            self.destination
        } else {
            let n = 1 + self.extra_destinations.len();
            let idx = self.next_dest_idx % n;
            self.next_dest_idx = (self.next_dest_idx + 1) % n;
            if idx == 0 {
                self.destination
            } else {
                self.extra_destinations[idx - 1]
            }
        };
        self.metrics.record_send(&p);
        Some(p)
    }

    /// A packet arrived at this app. Returns an ack to send back when the
    /// packet was numbered data from a closed-loop sender.
    pub fn deliver(&mut self, packet: Packet, now: SimTime) -> Option<Packet> {
        // An IP endpoint silently drops packets not addressed to it.
        // Match against the app's full prefix, not just its primary IP.
        if !ip_in_prefix(packet.ip_dst.0, self.ip.0, self.prefix_len) {
            return None;
        }
        if packet.kind == PacketKind::Ack {
            self.metrics.acks_received += 1;
            if let (Some(flow), Some(seq)) = (self.flow.as_mut(), packet.seq) {
                flow.on_ack(seq, now);
            }
            return None;
        }
        self.metrics.record_recv(&packet, now);
        if let TrafficPattern::RequestResponse { waiting, .. } = &mut self.pattern {
            *waiting = false;
        }
        let seq = packet.seq?;
        if !self.seen.insert((packet.flow_id.raw(), seq)) {
            self.metrics.duplicates_received += 1;
        }
        // Ack every numbered copy, duplicates included: the sender may
        // have given up on the first one.
        let pid = PacketId::new(((self.id.raw() as u64) << 32) | self.next_packet_id);
        self.next_packet_id += 1;
        let mut ack = Packet::new(pid, now, ACK_SIZE_BYTES);
        ack.kind = PacketKind::Ack;
        ack.ip_src = self.ip;
        ack.ip_dst = packet.ip_src;
        ack.app_id = self.id;
        ack.flow_id = packet.flow_id;
        ack.seq = Some(seq);
        self.metrics.acks_sent += 1;
        Some(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(v: u32) -> IpAddr {
        IpAddr(v)
    }

    #[test]
    fn cbr_generates_steady() {
        let mut app = App::new(
            AppId::new(1),
            ip(1),
            32,
            ip(2),
            32,
            TrafficPattern::ConstantBitrate {
                interval: Duration::from_millis(10),
                size_bytes: 100,
            },
        );
        let t = app.tick(Duration::ZERO);
        assert_eq!(t.packet.unwrap().size_bytes, 100);
        assert_eq!(t.next, Some(Duration::from_millis(10)));
        assert_eq!(app.metrics.packets_sent, 1);
    }

    #[test]
    fn closed_loop_sender_numbers_packets_and_receiver_acks_them() {
        use crate::flow::{Demand, FlowEngine, Law};
        let bps = 80_000.0;
        let mut a = App::new(AppId::new(1), ip(1), 32, ip(2), 32, TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(1),
            size_bytes: 0,
        })
        .with_flow(FlowEngine::new(Demand::Constant { bps }, Law::Fixed { bps }, 100));
        let mut b = App::new(AppId::new(2), ip(2), 32, ip(1), 32, TrafficPattern::ConstantBitrate {
            interval: Duration::from_secs(1),
            size_bytes: 0,
        });

        let t = a.tick(Duration::ZERO);
        let p = t.packet.unwrap();
        assert_eq!(p.seq, Some(0));
        assert_eq!(p.kind, PacketKind::Data);
        assert_eq!(t.next, Some(Duration::from_millis(10)));
        assert_eq!(a.flow.as_ref().unwrap().outstanding(), 1);

        let ack = b.deliver(p.clone(), Duration::from_millis(2)).expect("numbered data is acked");
        assert_eq!(ack.kind, PacketKind::Ack);
        assert_eq!(ack.seq, Some(0));
        assert_eq!(ack.ip_dst, ip(1));
        assert_eq!(ack.size_bytes, ACK_SIZE_BYTES);
        assert_eq!(b.metrics.packets_received, 1);
        assert_eq!(b.metrics.acks_sent, 1);
        assert_eq!(b.metrics.duplicates_received, 0);

        // A second copy of seq 0 is a duplicate, and is acked again.
        assert!(b.deliver(p, Duration::from_millis(3)).is_some());
        assert_eq!(b.metrics.duplicates_received, 1);

        // The ack closes the loop; it is not counted as received data.
        assert!(a.deliver(ack, Duration::from_millis(4)).is_none());
        assert_eq!(a.flow.as_ref().unwrap().outstanding(), 0);
        assert_eq!(a.flow.as_ref().unwrap().min_rtt, Some(Duration::from_millis(4)));
        assert_eq!(a.metrics.packets_received, 0);
        assert_eq!(a.metrics.acks_received, 1);

        // Open-loop data is never acked.
        let mut plain = Packet::new(PacketId::new(9), Duration::ZERO, 100);
        plain.ip_dst = ip(2);
        assert!(b.deliver(plain, Duration::from_millis(5)).is_none());
    }

    #[test]
    fn delay_metrics() {
        let mut app = App::new(
            AppId::new(1),
            ip(1),
            32,
            ip(2),
            32,
            TrafficPattern::ConstantBitrate {
                interval: Duration::from_millis(10),
                size_bytes: 100,
            },
        );
        let mut p = app.tick(Duration::ZERO).packet.unwrap();
        p.created_at = Duration::ZERO;
        p.ip_dst = ip(1);
        app.deliver(p, Duration::from_millis(5));
        app.metrics.finalize();
        assert_eq!(app.metrics.avg_delay, Duration::from_millis(5));
    }

    #[test]
    fn loss_rate_computed() {
        let mut app = App::new(
            AppId::new(1),
            ip(1),
            32,
            ip(2),
            32,
            TrafficPattern::ConstantBitrate {
                interval: Duration::from_millis(10),
                size_bytes: 100,
            },
        );
        for _ in 0..10 {
            app.tick(Duration::ZERO);
        }
        // Deliver only 7
        for _ in 0..7 {
            let mut p = Packet::new(PacketId::new(0), Duration::ZERO, 100);
            p.created_at = Duration::ZERO;
            p.ip_dst = ip(1);
            app.deliver(p, Duration::from_millis(1));
        }
        // 3 lost out of 10
        app.metrics.finalize();
        assert!((app.metrics.loss_rate - 0.3).abs() < 1e-9);
    }

    #[test]
    fn throughput_computed() {
        let mut app = App::new(
            AppId::new(1),
            ip(1),
            32,
            ip(2),
            32,
            TrafficPattern::ConstantBitrate {
                interval: Duration::from_millis(10),
                size_bytes: 100,
            },
        );
        let mut p = Packet::new(PacketId::new(1), Duration::ZERO, 1000);
        p.ip_dst = ip(1);
        app.deliver(p, Duration::from_secs(1));
        // 1000 bytes / 1s = 8000 bps
        app.metrics.finalize();
        assert!((app.metrics.throughput_bps - 8000.0).abs() < 1.0);
    }
}
