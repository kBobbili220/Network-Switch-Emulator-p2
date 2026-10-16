//! Egress ports: the queues a node holds for one outgoing link, and the
//! scheduler that decides which queued packet goes onto the wire next.
//!
//! The wire *pulls*. A packet is admitted into a queue and nothing about
//! its departure is decided; when the port's wire goes idle the scheduler
//! picks a queue, its head is popped and only then is its serialization
//! started. That is the moment where priority, weighted service and
//! post-admission drops can exist. With one FIFO queue the departure times
//! are exactly those of a drop-tail link, so A1 worlds are unaffected.
//!
//! Buffer is shared: a node has one byte pool (`buffer_budget`) that every
//! port on it draws from, and each queue additionally has its own cap
//! (`max_bytes`). Both must have room for a packet to be admitted. Bytes
//! are charged on admission and released when the packet has finished
//! serializing, i.e. when it has fully left the buffer.

use crate::packet::Packet;
use std::collections::VecDeque;

/// One queue on an egress port.
#[derive(Debug, Default)]
pub struct Queue {
    packets: VecDeque<Packet>,
    bytes: u64,
    /// Per-queue cap in bytes. Admission fails when `bytes + size` exceeds it.
    pub max_bytes: u64,
    /// Share of the wire under a weighted scheduler. Ignored by FIFO and
    /// strict priority.
    pub weight: u32,
    /// Early-drop line in bytes, applied by the drop policy (Phase 1b).
    /// `None` means drop-tail only.
    pub drop_threshold_bytes: Option<u64>,
    /// Packets refused on their way into this queue, for any reason.
    pub dropped: u64,
}

impl Queue {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            bytes: 0,
            max_bytes,
            weight: 1,
            drop_threshold_bytes: None,
            dropped: 0,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Size of the packet at the head, if any.
    pub fn head_size(&self) -> Option<u64> {
        self.packets.front().map(|p| p.size_bytes)
    }
}

/// How a port's queues are served. Mirrors `controller::Discipline`; the
/// port builds the matching `Scheduler` from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discipline {
    /// One logical queue: serve in arrival order. With several queues
    /// this behaves like `StrictPriority`.
    Fifo,
    /// Queue 0 always first, then 1, and so on.
    StrictPriority,
    /// Deficit round robin: each queue gets `weight` packets' worth of
    /// credit per round and spends it on its head packets.
    Drr,
}

/// Picks which queue the wire serves next. Called only when at least one
/// queue is non-empty; must return the index of a non-empty queue.
pub trait Scheduler {
    fn pick(&mut self, queues: &[Queue]) -> Option<usize>;
}

/// Lowest-numbered non-empty queue. With one queue this is plain FIFO.
#[derive(Debug, Default)]
pub struct Fifo;

impl Scheduler for Fifo {
    fn pick(&mut self, queues: &[Queue]) -> Option<usize> {
        queues.iter().position(|q| !q.is_empty())
    }
}

/// Queue 0 is served whenever it has anything, then queue 1, and so on.
/// Lower-numbered queues can starve higher ones; that is the point.
#[derive(Debug, Default)]
pub struct StrictPriority;

impl Scheduler for StrictPriority {
    fn pick(&mut self, queues: &[Queue]) -> Option<usize> {
        queues.iter().position(|q| !q.is_empty())
    }
}

/// Deficit round robin (Shreedhar & Varghese). Each queue carries a
/// deficit counter. The scheduler visits queues in order; a visited
/// non-empty queue is topped up by its quantum (`weight × unit_bytes`) once
/// per round and sends its head if the deficit covers it. A queue that
/// goes empty forfeits its deficit, so idle queues never bank credit.
#[derive(Debug)]
pub struct Drr {
    /// Bytes of credit one weight unit buys per round.
    unit_bytes: u64,
    deficit: Vec<u64>,
    /// Queue the next round starts from.
    cursor: usize,
    /// Whether `cursor` has already been topped up this round.
    topped_up: bool,
}

impl Drr {
    pub fn new(num_queues: usize, unit_bytes: u64) -> Self {
        Self {
            unit_bytes: unit_bytes.max(1),
            deficit: vec![0; num_queues],
            cursor: 0,
            topped_up: false,
        }
    }
}

impl Scheduler for Drr {
    fn pick(&mut self, queues: &[Queue]) -> Option<usize> {
        let n = queues.len();
        if n == 0 || queues.iter().all(|q| q.is_empty()) {
            return None;
        }
        if self.deficit.len() != n {
            self.deficit = vec![0; n];
            self.cursor = 0;
            self.topped_up = false;
        }
        // Every full pass adds at least one quantum to some non-empty
        // queue, so a bounded number of passes always finds a sendable
        // head. The bound protects against a pathological quantum.
        for _ in 0..n * 64 {
            let i = self.cursor % n;
            let q = &queues[i];
            if q.is_empty() {
                self.deficit[i] = 0;
            } else {
                if !self.topped_up {
                    let quantum = (q.weight.max(1) as u64).saturating_mul(self.unit_bytes);
                    self.deficit[i] = self.deficit[i].saturating_add(quantum);
                    self.topped_up = true;
                }
                let head = q.head_size().unwrap_or(0);
                if self.deficit[i] >= head {
                    self.deficit[i] -= head;
                    // Stay on this queue: it may send more this round.
                    return Some(i);
                }
            }
            self.cursor = (i + 1) % n;
            self.topped_up = false;
        }
        // Should be unreachable; fall back to strict order so the wire
        // never stalls with packets waiting.
        queues.iter().position(|q| !q.is_empty())
    }
}

/// Requested shape of one queue, in bytes. Built by the switch from the
/// controller's packet-denominated request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueShape {
    pub max_bytes: u64,
    pub weight: u32,
    pub drop_threshold_bytes: Option<u64>,
}

pub fn make_scheduler(discipline: Discipline, num_queues: usize, unit_bytes: u64) -> Box<dyn Scheduler> {
    match discipline {
        Discipline::Fifo => Box::new(Fifo),
        Discipline::StrictPriority => Box::new(StrictPriority),
        Discipline::Drr => Box::new(Drr::new(num_queues, unit_bytes)),
    }
}

/// Why `EgressPort::admit` refused a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The node's shared buffer pool had no room.
    PoolFull,
    /// The queue's own cap had no room.
    QueueFull,
    /// There was room, but the queue's drop threshold said drop anyway.
    /// This is the program's policy, not a lack of buffer.
    Threshold,
}

/// The queues and scheduler behind one outgoing port.
pub struct EgressPort {
    pub queues: Vec<Queue>,
    pub scheduler: Box<dyn Scheduler>,
    /// True while a packet from this port is being serialized onto the wire.
    pub transmitting: bool,
    /// Packets refused for any reason.
    pub packets_dropped: u64,
    pub bytes_dropped: u64,
    /// Of those, packets refused by a queue's drop threshold while there
    /// was still room for them.
    pub policy_dropped: u64,
}

impl EgressPort {
    /// One FIFO queue capped at `queue_bytes`.
    pub fn single_fifo(queue_bytes: u64) -> Self {
        Self {
            queues: vec![Queue::new(queue_bytes)],
            scheduler: Box::new(Fifo),
            transmitting: false,
            packets_dropped: 0,
            bytes_dropped: 0,
            policy_dropped: 0,
        }
    }

    /// Try to place `packet` in queue `q`, given the node's pool state.
    /// `q` beyond the last queue is clamped to the last one, so a program
    /// that sets a queue id the port does not have still forwards.
    /// Returns the packet back with the reason on refusal so the caller
    /// can log the drop under the right cause.
    pub fn admit(
        &mut self,
        q: usize,
        packet: Packet,
        pool_used: u64,
        pool_budget: u64,
    ) -> Result<(), (Packet, Refusal)> {
        let size = packet.size_bytes;
        let idx = q.min(self.queues.len().saturating_sub(1));
        let queue = &mut self.queues[idx];
        let why = if pool_used + size > pool_budget {
            Some(Refusal::PoolFull)
        } else if queue.bytes + size > queue.max_bytes {
            Some(Refusal::QueueFull)
        } else if queue
            .drop_threshold_bytes
            .is_some_and(|t| queue.bytes + size > t)
        {
            Some(Refusal::Threshold)
        } else {
            None
        };
        if let Some(why) = why {
            queue.dropped += 1;
            self.packets_dropped += 1;
            self.bytes_dropped += size;
            if why == Refusal::Threshold {
                self.policy_dropped += 1;
            }
            return Err((packet, why));
        }
        queue.bytes += size;
        queue.packets.push_back(packet);
        Ok(())
    }

    /// Ask the scheduler for the next packet and pop it. The queue's byte
    /// count is released here; the caller releases the node's pool once
    /// serialization completes.
    pub fn pick_next(&mut self) -> Option<Packet> {
        let idx = self.scheduler.pick(&self.queues)?;
        let queue = self.queues.get_mut(idx)?;
        let pkt = queue.packets.pop_front()?;
        queue.bytes -= pkt.size_bytes;
        Some(pkt)
    }

    /// Replace this port's queues and scheduler. Packets already waiting
    /// are carried over in order into the queue with the same index (or
    /// the last new queue if theirs no longer exists), regardless of the
    /// new caps: the node's buffer already accounts for them, and evicting
    /// would invent drops the program never asked for. An empty `shapes`
    /// keeps the existing queues and only swaps the scheduler.
    pub fn reconfigure(&mut self, shapes: &[QueueShape], discipline: Discipline, unit_bytes: u64) {
        if !shapes.is_empty() {
            let mut fresh: Vec<Queue> = shapes
                .iter()
                .map(|s| Queue {
                    packets: VecDeque::new(),
                    bytes: 0,
                    max_bytes: s.max_bytes,
                    weight: s.weight,
                    drop_threshold_bytes: s.drop_threshold_bytes,
                    dropped: 0,
                })
                .collect();
            let last = fresh.len() - 1;
            for (i, old) in self.queues.drain(..).enumerate() {
                let target = &mut fresh[i.min(last)];
                for p in old.packets {
                    target.bytes += p.size_bytes;
                    target.packets.push_back(p);
                }
            }
            self.queues = fresh;
        }
        self.scheduler = make_scheduler(discipline, self.queues.len(), unit_bytes);
    }

    /// Bytes waiting across all queues (not counting the packet on the wire).
    pub fn bytes_queued(&self) -> u64 {
        self.queues.iter().map(|q| q.bytes).sum()
    }

    pub fn packets_queued(&self) -> usize {
        self.queues.iter().map(|q| q.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PacketId;
    use std::time::Duration;

    fn pkt(id: u64, size: u64) -> Packet {
        Packet::new(PacketId::new(id), Duration::ZERO, size)
    }

    #[test]
    fn per_queue_cap_refuses_when_full() {
        let mut p = EgressPort::single_fifo(100);
        assert!(p.admit(0, pkt(1, 60), 0, u64::MAX).is_ok());
        assert!(p.admit(0, pkt(2, 40), 60, u64::MAX).is_ok());
        let (refused, why) = p.admit(0, pkt(3, 1), 100, u64::MAX).unwrap_err();
        assert_eq!(refused.id, PacketId::new(3));
        assert_eq!(why, Refusal::QueueFull);
        assert_eq!(p.packets_dropped, 1);
        assert_eq!(p.policy_dropped, 0);
        assert_eq!(p.bytes_queued(), 100);
    }

    #[test]
    fn pool_budget_refuses_even_when_queue_has_room() {
        let mut p = EgressPort::single_fifo(1000);
        assert!(p.admit(0, pkt(1, 60), 0, 100).is_ok());
        // Another port on the node already holds 40 bytes: pool is full.
        let (_, why) = p.admit(0, pkt(2, 1), 100, 100).unwrap_err();
        assert_eq!(why, Refusal::PoolFull);
        assert_eq!(p.packets_queued(), 1);
    }

    #[test]
    fn drop_threshold_refuses_with_room_to_spare_and_says_so() {
        let mut p = EgressPort::single_fifo(1000);
        p.reconfigure(
            &[QueueShape {
                max_bytes: 1000,
                weight: 1,
                drop_threshold_bytes: Some(300),
            }],
            Discipline::Fifo,
            100,
        );
        for i in 0..3 {
            p.admit(0, pkt(i, 100), 0, u64::MAX).unwrap();
        }
        // 300 bytes queued: the next packet fits the cap but not the line.
        let (_, why) = p.admit(0, pkt(3, 100), 0, u64::MAX).unwrap_err();
        assert_eq!(why, Refusal::Threshold);
        assert_eq!(p.policy_dropped, 1);
        assert_eq!(p.packets_dropped, 1);
        assert_eq!(p.bytes_queued(), 300);
        // Draining one packet reopens the queue.
        p.pick_next().unwrap();
        assert!(p.admit(0, pkt(4, 100), 0, u64::MAX).is_ok());
        // A genuinely full queue is reported as full, not as policy, even
        // when a line is set.
        let mut full = EgressPort::single_fifo(100);
        full.queues[0].drop_threshold_bytes = Some(100);
        full.admit(0, pkt(5, 100), 0, u64::MAX).unwrap();
        let (_, why) = full.admit(0, pkt(6, 100), 0, u64::MAX).unwrap_err();
        assert_eq!(why, Refusal::QueueFull);
        assert_eq!(full.policy_dropped, 0);
    }

    #[test]
    fn fifo_serves_in_arrival_order_and_releases_queue_bytes() {
        let mut p = EgressPort::single_fifo(1000);
        for i in 1..=3 {
            p.admit(0, pkt(i, 10), 0, u64::MAX).unwrap();
        }
        assert_eq!(p.bytes_queued(), 30);
        assert_eq!(p.pick_next().unwrap().id, PacketId::new(1));
        assert_eq!(p.bytes_queued(), 20);
        assert_eq!(p.pick_next().unwrap().id, PacketId::new(2));
        assert_eq!(p.pick_next().unwrap().id, PacketId::new(3));
        assert!(p.pick_next().is_none());
        assert_eq!(p.bytes_queued(), 0);
    }

    #[test]
    fn unknown_queue_id_clamps_to_last_queue() {
        let mut p = EgressPort::single_fifo(1000);
        assert!(p.admit(7, pkt(1, 10), 0, u64::MAX).is_ok());
        assert_eq!(p.queues[0].len(), 1);
    }

    fn shapes(weights: &[u32]) -> Vec<QueueShape> {
        weights
            .iter()
            .map(|&w| QueueShape {
                max_bytes: 10_000,
                weight: w,
                drop_threshold_bytes: None,
            })
            .collect()
    }

    /// Drain the port, returning the queue index each pick came from.
    fn drain_order(p: &mut EgressPort) -> Vec<usize> {
        let mut order = Vec::new();
        while let Some(pkt) = p.pick_next() {
            // Packet ids encode their queue as id / 100.
            order.push((pkt.id.raw() / 100) as usize);
        }
        order
    }

    #[test]
    fn strict_priority_drains_queue_zero_before_anything_else() {
        let mut p = EgressPort::single_fifo(1000);
        p.reconfigure(&shapes(&[1, 1]), Discipline::StrictPriority, 100);
        // Low-priority packets arrive first.
        for i in 0..3 {
            p.admit(1, pkt(100 + i, 100), 0, u64::MAX).unwrap();
        }
        for i in 0..2 {
            p.admit(0, pkt(i, 100), 0, u64::MAX).unwrap();
        }
        assert_eq!(drain_order(&mut p), vec![0, 0, 1, 1, 1]);
    }

    #[test]
    fn drr_shares_the_wire_by_weight() {
        let mut p = EgressPort::single_fifo(1000);
        // Queue 0 weight 1, queue 1 weight 2; unit = one 100-byte packet.
        p.reconfigure(&shapes(&[1, 2]), Discipline::Drr, 100);
        for i in 0..6 {
            p.admit(0, pkt(i, 100), 0, u64::MAX).unwrap();
            p.admit(1, pkt(100 + i, 100), 0, u64::MAX).unwrap();
        }
        let order = drain_order(&mut p);
        // First round: one from q0, two from q1; repeat. Twelve packets.
        assert_eq!(order.len(), 12);
        assert_eq!(&order[..6], &[0, 1, 1, 0, 1, 1]);
        // Once q1 is exhausted the remainder of q0 flows without waiting.
        assert_eq!(order.iter().filter(|&&q| q == 0).count(), 6);
    }

    #[test]
    fn drr_lets_a_big_packet_accumulate_credit_across_rounds() {
        let mut p = EgressPort::single_fifo(1000);
        p.reconfigure(&shapes(&[1, 1]), Discipline::Drr, 100);
        // One 300-byte packet in q0 needs three rounds of credit while q1
        // sends three 100-byte packets.
        p.admit(0, pkt(0, 300), 0, u64::MAX).unwrap();
        for i in 0..5 {
            p.admit(1, pkt(100 + i, 100), 0, u64::MAX).unwrap();
        }
        assert_eq!(drain_order(&mut p), vec![1, 1, 0, 1, 1, 1]);
    }

    #[test]
    fn drr_idle_queue_does_not_bank_credit() {
        let mut p = EgressPort::single_fifo(1000);
        p.reconfigure(&shapes(&[1, 1]), Discipline::Drr, 100);
        for i in 0..4 {
            p.admit(1, pkt(100 + i, 100), 0, u64::MAX).unwrap();
        }
        assert_eq!(p.pick_next().unwrap().id.raw(), 100);
        assert_eq!(p.pick_next().unwrap().id.raw(), 101);
        // q0 was idle during those rounds. Now it gets two packets; it
        // must not send both back to back on saved-up credit.
        p.admit(0, pkt(0, 100), 0, u64::MAX).unwrap();
        p.admit(0, pkt(1, 100), 0, u64::MAX).unwrap();
        assert_eq!(drain_order(&mut p), vec![0, 1, 0, 1]);
    }

    #[test]
    fn reconfigure_carries_waiting_packets_into_the_new_queues() {
        let mut p = EgressPort::single_fifo(1000);
        p.admit(0, pkt(1, 100), 0, u64::MAX).unwrap();
        p.admit(0, pkt(2, 100), 0, u64::MAX).unwrap();
        // Shrink below what is already queued: nothing is evicted.
        p.reconfigure(
            &[QueueShape {
                max_bytes: 50,
                weight: 1,
                drop_threshold_bytes: None,
            }],
            Discipline::Fifo,
            100,
        );
        assert_eq!(p.packets_queued(), 2);
        assert_eq!(p.bytes_queued(), 200);
        assert_eq!(p.queues[0].max_bytes, 50);
        // But new admissions see the cap.
        assert_eq!(p.admit(0, pkt(3, 100), 0, u64::MAX).unwrap_err().1, Refusal::QueueFull);
        // Growing to three queues keeps the old queue-0 packets in queue 0.
        p.reconfigure(&shapes(&[1, 1, 1]), Discipline::StrictPriority, 100);
        assert_eq!(p.queues.len(), 3);
        assert_eq!(p.queues[0].len(), 2);
        // Shrinking back to one queue folds higher queues into the last.
        p.admit(2, pkt(4, 100), 0, u64::MAX).unwrap();
        p.reconfigure(&shapes(&[1]), Discipline::Fifo, 100);
        assert_eq!(p.queues[0].len(), 3);
        assert_eq!(p.pick_next().unwrap().id.raw(), 1);
    }
}
