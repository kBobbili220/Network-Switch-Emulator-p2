use crate::packet::Packet;
use crate::types::{AppId, LinkId, PortId, SimTime, SwitchId};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

#[derive(Debug, Clone)]
pub enum EventKind {
    /// App should attempt to generate a new packet.
    AppTick { app: AppId },

    /// App receives a packet.
    AppDeliver { app: AppId, packet: Packet },

    /// Packet enters a switch ingress port.
    SwitchIngress {
        switch: SwitchId,
        port: PortId,
        packet: Packet,
    },

    /// Switch finished pipeline processing for this packet; ready to enqueue at egress.
    SwitchPipelineDone {
        switch: SwitchId,
        port: PortId,
        queue: u16,
        packet: Packet,
    },

    /// The packet has fully left the sending port onto the wire. `from_src`
    /// names the direction: true for the link's src→dst half. The handler
    /// schedules the far-end arrival and pulls the port's next packet.
    LinkSerializationDone {
        link: LinkId,
        from_src: bool,
        packet: Packet,
    },

    /// Punted packet arrives at switch CPU (via punt pipe).
    PuntArrive {
        switch: SwitchId,
        packet: Packet,
        port: PortId,
        reason: crate::packet::PuntReason,
    },

    /// Controller action arrives at data plane (via config pipe).
    ConfigArrive {
        switch: SwitchId,
        action: crate::controller::ControllerAction,
    },

    /// Controller timer fires.
    ControllerTimer { switch: SwitchId },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub time: SimTime,
    pub seq: u64,
    pub kind: EventKind,
}

impl Eq for Event {}
impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.seq == other.seq
    }
}

// BinaryHeap is a max-heap; we want min-heap by (time, seq).
impl Ord for Event {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .time
            .cmp(&self.time)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub struct EventQueue {
    heap: BinaryHeap<Event>,
    next_seq: u64,
}

impl EventQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schedule(&mut self, time: SimTime, kind: EventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Event { time, seq, kind });
        seq
    }

    pub fn pop(&mut self) -> Option<Event> {
        self.heap.pop()
    }

    pub fn peek_time(&self) -> Option<SimTime> {
        self.heap.peek().map(|e| e.time)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn event_queue_orders_by_time() {
        let mut q = EventQueue::new();
        q.schedule(
            Duration::from_millis(10),
            EventKind::ControllerTimer {
                switch: SwitchId::new(0),
            },
        );
        q.schedule(
            Duration::from_millis(5),
            EventKind::ControllerTimer {
                switch: SwitchId::new(1),
            },
        );
        q.schedule(
            Duration::from_millis(7),
            EventKind::ControllerTimer {
                switch: SwitchId::new(2),
            },
        );

        let e1 = q.pop().unwrap();
        let e2 = q.pop().unwrap();
        let e3 = q.pop().unwrap();
        assert_eq!(e1.time, Duration::from_millis(5));
        assert_eq!(e2.time, Duration::from_millis(7));
        assert_eq!(e3.time, Duration::from_millis(10));
    }

    #[test]
    fn event_queue_orders_by_seq_for_simultaneous() {
        let mut q = EventQueue::new();
        let t = Duration::from_millis(10);
        let s_a = q.schedule(
            t,
            EventKind::ControllerTimer {
                switch: SwitchId::new(0),
            },
        );
        let s_b = q.schedule(
            t,
            EventKind::ControllerTimer {
                switch: SwitchId::new(1),
            },
        );
        let s_c = q.schedule(
            t,
            EventKind::ControllerTimer {
                switch: SwitchId::new(2),
            },
        );

        let e1 = q.pop().unwrap();
        let e2 = q.pop().unwrap();
        let e3 = q.pop().unwrap();
        assert_eq!(e1.seq, s_a);
        assert_eq!(e2.seq, s_b);
        assert_eq!(e3.seq, s_c);
    }
}
