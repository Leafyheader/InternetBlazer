//! Segment bookkeeping and the dynamic splitting strategy.

use serde::{Deserialize, Serialize};

/// A contiguous byte range `[start, end)` of the target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    /// Bytes written to disk, counted from `start`.
    pub downloaded: u64,
}

impl Segment {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end, downloaded: 0 }
    }

    pub fn position(&self) -> u64 {
        self.start + self.downloaded
    }

    pub fn is_complete(&self) -> bool {
        self.position() >= self.end
    }
}

/// End offset used while the size of the file is unknown.
const UNBOUNDED: u64 = u64::MAX;

#[derive(Debug)]
struct Slot {
    seg: Segment,
    /// Bytes a worker has received and committed to writing. Runs ahead of `seg.downloaded`
    /// while a write is in flight, and splits start from here so they never cut into that data.
    reserved: u64,
    busy: bool,
}

impl Slot {
    fn reserved_position(&self) -> u64 {
        self.seg.start + self.reserved
    }
}

/// Work handed to a connection: fetch `[from, end)` of segment `idx`. The end may shrink while
/// the transfer runs, when another connection splits the segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Claim {
    pub idx: usize,
    pub from: u64,
    pub end: u64,
}

#[derive(Debug)]
pub(crate) struct Plan {
    slots: Vec<Slot>,
    min_split: u64,
    /// False when the server does not support ranges: the single segment is then refetched from
    /// the beginning on every attempt.
    ranged: bool,
}

impl Plan {
    /// Splits a ranged download into up to `parts` equal segments up front, which needs fewer
    /// requests than reaching the same number by repeated halving.
    pub fn new(total: Option<u64>, ranged: bool, min_split: u64, parts: usize) -> Self {
        let min_split = min_split.max(1);
        let segments = match total {
            Some(total) if ranged => {
                let parts = (parts as u64).clamp(1, (total / min_split).max(1));
                let size = total / parts;
                (0..parts)
                    .map(|i| {
                        Segment::new(i * size, if i + 1 == parts { total } else { (i + 1) * size })
                    })
                    .collect()
            }
            _ => vec![Segment::new(0, total.unwrap_or(UNBOUNDED))],
        };
        Self::from_segments(segments, ranged && total.is_some(), min_split)
    }

    pub fn from_segments(segments: Vec<Segment>, ranged: bool, min_split: u64) -> Self {
        let slots = segments
            .into_iter()
            .map(|seg| Slot { reserved: seg.downloaded, seg, busy: false })
            .collect();
        Self { slots, min_split: min_split.max(1), ranged }
    }

    /// Hands out an idle unfinished segment, or else splits the busy segment with the most data
    /// left. Returns `None` when no work is left that is worth a new connection.
    pub fn claim(&mut self) -> Option<Claim> {
        let ranged = self.ranged;
        if let Some(idx) = self.slots.iter().position(|s| !s.busy && !s.seg.is_complete()) {
            let slot = &mut self.slots[idx];
            if !ranged {
                slot.seg.downloaded = 0;
            }
            slot.busy = true;
            slot.reserved = slot.seg.downloaded;
            return Some(Claim { idx, from: slot.reserved_position(), end: slot.seg.end });
        }
        if !ranged {
            return None;
        }

        let (idx, remaining) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.busy)
            .map(|(i, s)| (i, s.seg.end - s.reserved_position()))
            .max_by_key(|&(_, remaining)| remaining)?;
        if remaining / 2 < self.min_split {
            return None;
        }
        let victim = &mut self.slots[idx];
        let mid = victim.reserved_position() + remaining / 2;
        let tail = Segment::new(mid, victim.seg.end);
        victim.seg.end = mid;
        self.slots.push(Slot { seg: tail, reserved: 0, busy: true });
        Some(Claim { idx: self.slots.len() - 1, from: mid, end: tail.end })
    }

    /// Reserves up to `len` received bytes of segment `idx`, clamped to its current end.
    /// Returns how many bytes to keep, and whether the segment is now fully reserved.
    pub fn reserve(&mut self, idx: usize, len: usize) -> (usize, bool) {
        let slot = &mut self.slots[idx];
        let available = slot.seg.end - slot.reserved_position();
        let take = (len as u64).min(available);
        slot.reserved += take;
        (take as usize, slot.reserved_position() >= slot.seg.end)
    }

    /// Records `len` reserved bytes as written to disk.
    pub fn commit(&mut self, idx: usize, len: u64) {
        self.slots[idx].seg.downloaded += len;
    }

    /// Returns a segment to the pool, dropping reservations that were never written.
    pub fn release(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        slot.busy = false;
        slot.reserved = slot.seg.downloaded;
    }

    pub fn is_unbounded(&self, idx: usize) -> bool {
        self.slots[idx].seg.end == UNBOUNDED
    }

    /// Ends a stream of unknown size at its current position.
    pub fn finish_unbounded(&mut self, idx: usize) {
        let seg = &mut self.slots[idx].seg;
        seg.end = seg.position();
    }

    pub fn downloaded(&self) -> u64 {
        self.slots.iter().map(|s| s.seg.downloaded).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(|s| s.seg.is_complete())
    }

    pub fn segments(&self) -> Vec<Segment> {
        let mut segments: Vec<_> = self.slots.iter().map(|s| s.seg).collect();
        segments.sort_by_key(|s| s.start);
        segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(plan: &mut Plan, idx: usize, len: usize) -> bool {
        let (take, done) = plan.reserve(idx, len);
        plan.commit(idx, take as u64);
        done
    }

    #[test]
    fn idle_connections_split_the_largest_remaining_segment() {
        let mut plan = Plan::new(Some(1000), true, 10, 1);
        let a = plan.claim().unwrap();
        assert_eq!(a, Claim { idx: 0, from: 0, end: 1000 });

        let b = plan.claim().unwrap();
        assert_eq!((b.from, b.end), (500, 1000));

        write(&mut plan, a.idx, 100);
        // Segment 0 has 400 left, segment 1 has 500: the new connection takes half of segment 1.
        let c = plan.claim().unwrap();
        assert_eq!((c.from, c.end), (750, 1000));
        assert_eq!(
            plan.segments().iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            [(0, 500), (500, 750), (750, 1000)]
        );
    }

    #[test]
    fn starts_with_equal_parts_no_smaller_than_min_split() {
        let starts =
            |plan: Plan| plan.segments().iter().map(|s| (s.start, s.end)).collect::<Vec<_>>();
        assert_eq!(starts(Plan::new(Some(100), true, 10, 3)), [(0, 33), (33, 66), (66, 100)]);
        assert_eq!(starts(Plan::new(Some(100), true, 40, 8)), [(0, 50), (50, 100)]);
        assert_eq!(starts(Plan::new(Some(100), false, 10, 8)), [(0, 100)]);
    }

    #[test]
    fn split_shrinks_the_running_transfer() {
        let mut plan = Plan::new(Some(100), true, 10, 1);
        let a = plan.claim().unwrap();
        let b = plan.claim().unwrap();
        assert_eq!(b.from, 50);

        // The first connection asked for 0..100 but must stop at the new boundary.
        assert_eq!(plan.reserve(a.idx, 80), (50, true));
    }

    #[test]
    fn splits_never_cut_into_reserved_bytes() {
        let mut plan = Plan::new(Some(100), true, 10, 1);
        let a = plan.claim().unwrap();
        plan.reserve(a.idx, 60); // received but not yet written
        let b = plan.claim().unwrap();
        assert_eq!((b.from, b.end), (80, 100));
    }

    #[test]
    fn small_remainders_are_not_split() {
        let mut plan = Plan::new(Some(100), true, 60, 1);
        plan.claim().unwrap();
        assert_eq!(plan.claim(), None);
    }

    #[test]
    fn released_segments_are_reclaimed_from_the_last_written_byte() {
        let mut plan = Plan::new(Some(100), true, 10, 1);
        let a = plan.claim().unwrap();
        write(&mut plan, a.idx, 30);
        plan.reserve(a.idx, 5); // lost with the connection
        plan.release(a.idx);
        assert_eq!(plan.claim(), Some(Claim { idx: 0, from: 30, end: 100 }));
    }

    #[test]
    fn unranged_downloads_restart_from_zero() {
        let mut plan = Plan::new(Some(100), false, 10, 1);
        let a = plan.claim().unwrap();
        write(&mut plan, a.idx, 30);
        assert_eq!(plan.claim(), None);
        plan.release(a.idx);
        assert_eq!(plan.claim(), Some(Claim { idx: 0, from: 0, end: 100 }));
        assert_eq!(plan.downloaded(), 0);
    }

    #[test]
    fn unknown_size_completes_when_the_stream_ends() {
        let mut plan = Plan::new(None, true, 10, 1);
        let a = plan.claim().unwrap();
        assert!(plan.is_unbounded(a.idx));
        write(&mut plan, a.idx, 42);
        assert!(!plan.is_complete());
        plan.finish_unbounded(a.idx);
        assert!(plan.is_complete());
        assert_eq!(plan.downloaded(), 42);
    }
}
