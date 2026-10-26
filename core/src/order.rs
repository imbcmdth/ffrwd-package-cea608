//! Access units in decode order, handed out in presentation order.
//!
//! A decoder hands each picture's `cc_data` on with the picture, so the
//! pairs reach the caption decoder in presentation order. With B-frames that
//! is not the order the packets arrive in, and pairs written in arrival
//! order would come out of the decoder shuffled. So each access unit is
//! given its pairs in presentation order: a packet is settled once `depth`
//! later packets have arrived, `depth` being the stream's reorder depth,
//! and the earliest unsettled presentation time is then the next picture.
//! Packets still leave in the order they came, each once it and every
//! packet before it is settled.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

pub struct Order<T> {
    /// Packets in decode order, each with whether it has been settled.
    held: VecDeque<(T, bool)>,
    /// The sequence number of `held`'s front.
    front: u64,
    next: u64,
    waiting: BinaryHeap<Reverse<(i64, u64)>>,
    depth: usize,
    last: Option<i64>,
    out_of_order: u64,
}

impl<T> Order<T> {
    pub fn new(depth: usize) -> Order<T> {
        Order {
            held: VecDeque::new(),
            front: 0,
            next: 0,
            waiting: BinaryHeap::new(),
            depth,
            last: None,
            out_of_order: 0,
        }
    }

    /// How many later packets a packet waits for.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// At least `depth`, from a stream that turned out to reorder deeper
    /// than it said.
    pub fn deepen(&mut self, depth: usize) {
        self.depth = self.depth.max(depth);
    }

    /// Packets settled after a later presentation time had been: each one
    /// deepened the wait by a packet.
    pub fn out_of_order(&self) -> u64 {
        self.out_of_order
    }

    pub fn push(&mut self, item: T, pts: i64) {
        self.held.push_back((item, false));
        self.waiting.push(Reverse((pts, self.next)));
        self.next += 1;
    }

    /// The next packet in presentation order, once it is settled; every one
    /// left when `closing`.
    pub fn settle(&mut self, closing: bool) -> Option<(u64, i64)> {
        if !closing && self.waiting.len() <= self.depth {
            return None;
        }
        let Reverse((pts, seq)) = self.waiting.pop()?;
        if self.last.is_some_and(|last| pts < last) {
            self.out_of_order += 1;
            self.depth += 1;
        }
        self.last = Some(self.last.map_or(pts, |last| last.max(pts)));
        if let Some(entry) = self.held.get_mut((seq - self.front) as usize) {
            entry.1 = true;
        }
        Some((seq, pts))
    }

    pub fn get_mut(&mut self, seq: u64) -> Option<&mut T> {
        let at = seq.checked_sub(self.front)? as usize;
        self.held.get_mut(at).map(|(item, _)| item)
    }

    /// The settled packets at the front, in the order they came.
    pub fn release(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while self.held.front().is_some_and(|(_, settled)| *settled) {
            if let Some((item, _)) = self.held.pop_front() {
                out.push(item);
                self.front += 1;
            }
        }
        out
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I P B B in decode order, shown I B B P: x264's shape with two
    /// B-frames, which needs a depth of 2.
    fn ipbb() -> Vec<i64> {
        vec![0, 3, 1, 2, 6, 4, 5]
    }

    #[test]
    fn packets_are_settled_in_presentation_order_and_leave_in_decode_order() {
        let mut order = Order::new(2);
        let mut settled = Vec::new();
        let mut left = Vec::new();
        for pts in ipbb() {
            order.push(pts, pts);
            while let Some((_, pts)) = order.settle(false) {
                settled.push(pts);
            }
            left.extend(order.release());
        }
        while let Some((_, pts)) = order.settle(true) {
            settled.push(pts);
        }
        left.extend(order.release());
        assert_eq!(settled, vec![0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(left, ipbb());
        assert_eq!(order.out_of_order(), 0);
        assert_eq!(order.held(), 0);
    }

    #[test]
    fn a_stream_without_reordering_holds_nothing() {
        let mut order = Order::new(0);
        for pts in 0..5 {
            order.push(pts, pts);
            assert_eq!(order.settle(false), Some((pts as u64, pts)));
            assert_eq!(order.release(), vec![pts]);
        }
    }

    #[test]
    fn a_stream_deeper_than_it_said_deepens_the_wait() {
        let mut order = Order::new(0);
        let mut settled = Vec::new();
        for pts in ipbb() {
            order.push(pts, pts);
            while let Some((_, pts)) = order.settle(false) {
                settled.push(pts);
            }
        }
        assert!(order.out_of_order() > 0);
        assert!(order.depth() > 0);
    }

    #[test]
    fn get_mut_reaches_a_held_packet_by_its_number() {
        let mut order = Order::new(1);
        order.push(10, 0);
        order.push(20, 1);
        *order.get_mut(1).unwrap() += 1;
        let (seq, _) = order.settle(false).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(order.release(), vec![10]);
        order.settle(true);
        assert_eq!(order.release(), vec![21]);
        assert!(order.get_mut(0).is_none());
    }
}
