use crate::types::{LinkId, NodeId, PortId, SimTime};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct LinkConfig {
    pub latency: Duration,
    pub bandwidth_bps: u64,
    /// Size of the drop-tail egress queue at *each end* of the link. The
    /// queue itself lives on the node's egress port (see `egress.rs`);
    /// this is the value that port is sized with when the link is
    /// connected to an app. Switches size their ports at construction.
    pub queue_capacity_bytes: u64,
}

impl LinkConfig {
    pub fn serialization_delay(&self, size_bytes: u64) -> Duration {
        if self.bandwidth_bps == 0 {
            return Duration::ZERO;
        }
        let nanos = (size_bytes as u128 * 8 * 1_000_000_000) / self.bandwidth_bps as u128;
        Duration::from_nanos(nanos as u64)
    }

    /// For a packet that starts onto the wire at `now`: when it has fully
    /// left the sender, and when it arrives at the far end.
    pub fn transmit_times(&self, now: SimTime, size_bytes: u64) -> (SimTime, SimTime) {
        let serialization_done = now + self.serialization_delay(size_bytes);
        (serialization_done, serialization_done + self.latency)
    }
}

/// Per-direction bookkeeping. The queue is not here: it is on the sending
/// node's egress port, and the wire pulls from it.
#[derive(Debug, Default)]
pub struct Direction {
    /// Packets refused because the link was down.
    pub bytes_dropped: u64,
    pub packets_dropped: u64,
}

/// A full-duplex cable between a source node/port and a destination
/// node/port: two independent wires, each with serialization at the
/// link's bandwidth followed by `latency` of propagation. `fwd` is the
/// src→dst wire, `rev` the dst→src wire.
#[derive(Debug)]
pub struct Link {
    pub id: LinkId,
    pub config: LinkConfig,
    pub src: NodeId,
    pub src_port: PortId,
    pub dst: NodeId,
    pub dst_port: PortId,

    pub fwd: Direction,
    pub rev: Direction,

    /// When true, no new packet may be admitted for this link in either
    /// direction. Packets already queued at the ports still drain. Toggle
    /// through `Simulator::fail_link` / `restore_link`.
    pub failed: bool,
}

impl Link {
    pub fn new(
        id: LinkId,
        config: LinkConfig,
        src: NodeId,
        src_port: PortId,
        dst: NodeId,
        dst_port: PortId,
    ) -> Self {
        Self {
            id,
            config,
            src,
            src_port,
            dst,
            dst_port,
            fwd: Direction::default(),
            rev: Direction::default(),
            failed: false,
        }
    }

    /// True when `node` is this link's `src` end, i.e. its packets travel
    /// on the `fwd` wire.
    pub fn is_src(&self, node: NodeId) -> bool {
        self.src == node
    }

    /// The (node, port) at the far end from `from_src`'s sender.
    pub fn far_end(&self, from_src: bool) -> (NodeId, PortId) {
        if from_src {
            (self.dst, self.dst_port)
        } else {
            (self.src, self.src_port)
        }
    }

    /// The (node, port) that sends on the `from_src` wire.
    pub fn near_end(&self, from_src: bool) -> (NodeId, PortId) {
        if from_src {
            (self.src, self.src_port)
        } else {
            (self.dst, self.dst_port)
        }
    }

    pub fn record_link_down_drop(&mut self, from_src: bool, size_bytes: u64) {
        let d = if from_src { &mut self.fwd } else { &mut self.rev };
        d.bytes_dropped += size_bytes;
        d.packets_dropped += 1;
    }

    /// Packets refused in either direction because the link was down.
    pub fn packets_dropped(&self) -> u64 {
        self.fwd.packets_dropped + self.rev.packets_dropped
    }

    pub fn bytes_dropped(&self) -> u64 {
        self.fwd.bytes_dropped + self.rev.bytes_dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_delay_is_correct() {
        let c = LinkConfig {
            latency: Duration::ZERO,
            bandwidth_bps: 8_000_000_000, // 1 GB/s
            queue_capacity_bytes: 1_000_000,
        };
        // 1000 bytes at 1 GB/s = 1 us
        assert_eq!(c.serialization_delay(1000), Duration::from_micros(1));
    }

    #[test]
    fn transmit_times_add_serialization_then_latency() {
        let c = LinkConfig {
            latency: Duration::from_millis(10),
            bandwidth_bps: 8_000, // 1 KB/s -> 1ms per byte
            queue_capacity_bytes: 1000,
        };
        let (ser_done, arrive) = c.transmit_times(Duration::from_millis(5), 1);
        assert_eq!(ser_done, Duration::from_millis(6));
        assert_eq!(arrive, Duration::from_millis(16));
    }

    #[test]
    fn ends_are_resolved_per_direction() {
        let link = Link::new(
            LinkId::new(0),
            LinkConfig {
                latency: Duration::ZERO,
                bandwidth_bps: 1,
                queue_capacity_bytes: 1,
            },
            NodeId::new(10),
            PortId::new(1),
            NodeId::new(20),
            PortId::new(2),
        );
        assert!(link.is_src(NodeId::new(10)));
        assert!(!link.is_src(NodeId::new(20)));
        assert_eq!(link.far_end(true), (NodeId::new(20), PortId::new(2)));
        assert_eq!(link.far_end(false), (NodeId::new(10), PortId::new(1)));
        assert_eq!(link.near_end(true), (NodeId::new(10), PortId::new(1)));
        assert_eq!(link.near_end(false), (NodeId::new(20), PortId::new(2)));
    }
}
