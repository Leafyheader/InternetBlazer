//! Segment bookkeeping and the dynamic splitting strategy.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

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
/// Bytes before a connection's start that it asks for again, to check they match what is on
/// disk. A file that changed on a server that gives no `ETag` or date shows up here.
pub(crate) const OVERLAP: u64 = 64;
/// Once nothing is left to split, a connection silent this long gets a second connection racing
/// it for the rest of its segment.
const RACE_IDLE: Duration = Duration::from_secs(2);
/// While splitting is still possible, a connection is only raced when it has been silent this
/// long and longer than the download's gaps between data usually are.
const RACE_BUSY: Duration = Duration::from_secs(1);

/// Running mean, variance and maximum of a series of durations, in seconds.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    n: u64,
    mean: f64,
    m2: f64,
    max: f64,
}

impl Stats {
    fn add(&mut self, x: f64) {
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
        self.max = self.max.max(x);
    }

    /// The longest silence that still looks like the usual rhythm: three standard deviations
    /// above the mean, but never beyond the longest gap seen.
    fn usual_max(&self) -> f64 {
        let sigma = if self.n > 1 { (self.m2 / (self.n - 1) as f64).sqrt() } else { 0.0 };
        (self.mean + 3.0 * sigma).min(self.max)
    }
}

#[derive(Debug)]
struct Slot {
    seg: Segment,
    /// Bytes a worker has received and committed to writing. Runs ahead of `seg.downloaded`
    /// while a write is in flight, and splits start from here so they never cut into that data.
    reserved: u64,
    busy: bool,
    /// Stops the connection working on this segment, once a racing connection takes it over.
    cut: CancellationToken,
    /// The racing connection, if one was started. It is stopped as soon as this segment's own
    /// connection receives data again. One race per claim.
    racer: Option<CancellationToken>,
    raced: bool,
    race_started: Instant,
    claimed_at: Instant,
    /// When data last arrived, or when the segment was claimed if none has yet.
    last_data: Instant,
    /// Bytes received since the claim.
    received: u64,
}

impl Slot {
    fn new(seg: Segment, busy: bool, now: Instant) -> Self {
        Self {
            reserved: seg.downloaded,
            seg,
            busy,
            cut: CancellationToken::new(),
            racer: None,
            raced: false,
            race_started: now,
            claimed_at: now,
            last_data: now,
            received: 0,
        }
    }

    fn reserved_position(&self) -> u64 {
        self.seg.start + self.reserved
    }

    fn remaining(&self) -> u64 {
        self.seg.end - self.reserved_position()
    }

    /// Bytes per second since the claim.
    fn speed(&self, now: Instant) -> f64 {
        let secs = now.duration_since(self.claimed_at).as_secs_f64();
        if secs > 0.0 { self.received as f64 / secs } else { 0.0 }
    }

    fn claim(&mut self, now: Instant) {
        self.busy = true;
        self.reserved = self.seg.downloaded;
        self.cut = CancellationToken::new();
        self.racer = None;
        self.raced = false;
        self.claimed_at = now;
        self.last_data = now;
        self.received = 0;
    }
}

/// Work handed to a connection: fetch `[from, end)` of segment `idx`. The end may shrink while
/// the transfer runs, when another connection splits the segment.
///
/// A racing claim fetches the rest of a silent connection's segment `idx` alongside it, and
/// takes the segment over with [`Plan::take_over`] if its data comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Claim {
    pub idx: usize,
    pub from: u64,
    pub end: u64,
    pub racing: bool,
}

#[derive(Debug)]
pub(crate) struct Plan {
    slots: Vec<Slot>,
    min_split: u64,
    /// False when the server does not support ranges: the single segment is then refetched from
    /// the beginning on every attempt.
    ranged: bool,
    /// Gaps between data arriving on a connection.
    gaps: Stats,
    /// Time from a claim to its first data: how long a new connection takes to get going.
    startup: Stats,
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
        let now = Instant::now();
        let slots = segments.into_iter().map(|seg| Slot::new(seg, false, now)).collect();
        Self {
            slots,
            min_split: min_split.max(1),
            ranged,
            gaps: Stats::default(),
            startup: Stats::default(),
        }
    }

    /// Hands out an idle unfinished segment; else races a connection that has gone silent; else
    /// splits the busy segment with the most data left. Returns `None` when no work is left that
    /// is worth a new connection.
    #[cfg(test)]
    pub fn claim(&mut self, now: Instant) -> Option<Claim> {
        self.claim_with(now, true)
    }

    /// [`Plan::claim`], racing only if `may_race`.
    pub fn claim_with(&mut self, now: Instant, may_race: bool) -> Option<Claim> {
        let ranged = self.ranged;
        if let Some(idx) = self.slots.iter().position(|s| !s.busy && !s.seg.is_complete()) {
            let slot = &mut self.slots[idx];
            if !ranged {
                slot.seg.downloaded = 0;
            }
            slot.claim(now);
            let from = slot.reserved_position();
            return Some(Claim { idx, from, end: slot.seg.end, racing: false });
        }
        if !ranged {
            return None;
        }

        let split = self.split_point(now);
        if may_race && let Some(race) = self.race(now, split.is_some()) {
            return Some(race);
        }
        let (idx, mid) = split?;
        let victim = &mut self.slots[idx];
        let tail = Segment::new(mid, victim.seg.end);
        victim.seg.end = mid;
        self.slots.push(Slot::new(tail, true, now));
        Some(Claim { idx: self.slots.len() - 1, from: mid, end: tail.end, racing: false })
    }

    /// Only races a silent connection, for a connection that is done with its own work.
    pub fn claim_race(&mut self, now: Instant) -> Option<Claim> {
        if !self.ranged {
            return None;
        }
        let can_split = self.split_point(now).is_some();
        self.race(now, can_split)
    }

    fn race(&mut self, now: Instant, can_split: bool) -> Option<Claim> {
        let idx = self.race_candidate(now, can_split)?;
        let slot = &mut self.slots[idx];
        slot.raced = true;
        slot.racer = Some(CancellationToken::new());
        slot.race_started = now;
        let from = slot.reserved_position();
        Some(Claim { idx, from, end: slot.seg.end, racing: true })
    }

    /// Whether any connection is working on a segment.
    pub fn is_busy(&self) -> bool {
        self.slots.iter().any(|s| s.busy)
    }

    /// Where to split the busy segment with the most data left, if it is worth it. The segment
    /// keeps what its connection will receive while the new one gets going, and the new
    /// connection takes half of the rest.
    fn split_point(&self, now: Instant) -> Option<(usize, u64)> {
        let (idx, ahead, rest) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.busy)
            .map(|(i, s)| {
                let remaining = s.remaining();
                let ahead = ((s.speed(now) * self.startup.mean) as u64).min(remaining);
                (i, ahead, remaining - ahead)
            })
            .max_by_key(|&(_, _, rest)| rest)?;
        if rest / 2 < self.min_split {
            return None;
        }
        Some((idx, self.slots[idx].reserved_position() + ahead + rest / 2))
    }

    /// The busy connection silent the longest, if it has been silent for longer than usual.
    /// Connections that have not received anything yet are measured against how long others took
    /// to get going, the rest against the gaps between data.
    fn race_candidate(&self, now: Instant, can_split: bool) -> Option<usize> {
        let secs = |s: f64| Duration::try_from_secs_f64(s).unwrap_or_default();
        let (idx, _) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.busy && !s.raced && s.remaining() > 0)
            .filter(|(_, s)| {
                let threshold = match (s.received, can_split) {
                    (0, true) => RACE_BUSY.max(secs(self.startup.usual_max())),
                    (0, false) => RACE_IDLE.max(secs(self.startup.usual_max())),
                    (_, true) => RACE_BUSY.max(secs(self.gaps.usual_max())),
                    (_, false) => RACE_IDLE,
                };
                now.duration_since(s.last_data) >= threshold
            })
            .map(|(i, s)| (i, now.duration_since(s.last_data)))
            .max_by_key(|&(_, silence)| silence)?;
        Some(idx)
    }

    /// Hands the segment of a racing claim over to the racer, which has received data before
    /// the segment's own connection did. The segment's connection is stopped, and the racer
    /// continues with the returned claim. `None` means the racer lost.
    pub fn take_over(&mut self, race: Claim) -> Option<Claim> {
        let slot = &mut self.slots[race.idx];
        let racer_alive = slot.racer.as_ref().is_some_and(|t| !t.is_cancelled());
        if !slot.busy || !racer_alive || slot.reserved_position() != race.from {
            return None;
        }
        let end = slot.seg.end;
        slot.seg.end = race.from;
        slot.racer = None;
        slot.cut.cancel();
        // The racer's request started when the race did.
        let started = slot.race_started;
        self.slots.push(Slot::new(Segment::new(race.from, end), true, started));
        Some(Claim { idx: self.slots.len() - 1, from: race.from, end, racing: false })
    }

    /// Fires when the connection holding `claim` should stop: for a segment's own connection,
    /// when a racer took the segment over; for a racer, when the segment's connection received
    /// data first.
    pub fn stopper(&self, claim: &Claim) -> CancellationToken {
        let slot = &self.slots[claim.idx];
        match (claim.racing, &slot.racer) {
            (false, _) => slot.cut.clone(),
            (true, Some(racer)) => racer.clone(),
            (true, None) => {
                let lost = CancellationToken::new();
                lost.cancel();
                lost
            }
        }
    }

    /// How many bytes right before the claim's start are on disk, up to [`OVERLAP`], so that a
    /// request starting that much earlier can check them.
    pub fn overlap(&self, claim: &Claim) -> u64 {
        let slot = &self.slots[claim.idx];
        if !self.ranged || slot.reserved != slot.seg.downloaded {
            return 0;
        }
        if slot.seg.downloaded > 0 {
            return slot.seg.downloaded.min(OVERLAP);
        }
        self.slots
            .iter()
            .find(|s| {
                s.seg.end == slot.seg.start && s.seg.is_complete() && s.reserved == s.seg.downloaded
            })
            .map_or(0, |s| (s.seg.end - s.seg.start).min(OVERLAP))
    }

    /// Bytes written from the start of the file on.
    pub fn written_from_start(&self) -> u64 {
        self.slots.iter().find(|s| s.seg.start == 0).map_or(0, |s| s.seg.downloaded)
    }

    /// Reserves up to `len` received bytes of segment `idx`, clamped to its current end.
    /// Returns how many bytes to keep, and whether the segment is now fully reserved.
    pub fn reserve(&mut self, idx: usize, len: usize, now: Instant) -> (usize, bool) {
        let slot = &mut self.slots[idx];
        let available = slot.seg.end - slot.reserved_position();
        let take = (len as u64).min(available);
        slot.reserved += take;
        if take > 0 {
            let (stats, since) = match slot.received {
                0 => (&mut self.startup, slot.claimed_at),
                _ => (&mut self.gaps, slot.last_data),
            };
            stats.add(now.duration_since(since).as_secs_f64());
            slot.received += take;
            slot.last_data = now;
            // The connection is back, so a racer has lost.
            if let Some(racer) = slot.racer.take() {
                racer.cancel();
            }
        }
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
        if let Some(racer) = slot.racer.take() {
            racer.cancel();
        }
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
    use std::sync::LazyLock;

    use super::*;

    static T0: LazyLock<Instant> = LazyLock::new(Instant::now);

    fn t0() -> Instant {
        *T0
    }

    fn at(secs: f64) -> Instant {
        t0() + Duration::from_secs_f64(secs)
    }

    fn write(plan: &mut Plan, idx: usize, len: usize) -> bool {
        let (take, done) = plan.reserve(idx, len, t0());
        plan.commit(idx, take as u64);
        done
    }

    #[test]
    fn idle_connections_split_the_largest_remaining_segment() {
        let mut plan = Plan::new(Some(1000), true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        assert_eq!(a, Claim { idx: 0, from: 0, end: 1000, racing: false });

        let b = plan.claim(t0()).unwrap();
        assert_eq!((b.from, b.end), (500, 1000));

        write(&mut plan, a.idx, 100);
        // Segment 0 has 400 left, segment 1 has 500: the new connection takes half of segment 1.
        let c = plan.claim(t0()).unwrap();
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
        let a = plan.claim(t0()).unwrap();
        let b = plan.claim(t0()).unwrap();
        assert_eq!(b.from, 50);

        // The first connection asked for 0..100 but must stop at the new boundary.
        assert_eq!(plan.reserve(a.idx, 80, t0()), (50, true));
    }

    #[test]
    fn splits_never_cut_into_reserved_bytes() {
        let mut plan = Plan::new(Some(100), true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        plan.reserve(a.idx, 60, t0()); // received but not yet written
        let b = plan.claim(t0()).unwrap();
        assert_eq!((b.from, b.end), (80, 100));
    }

    #[test]
    fn small_remainders_are_not_split() {
        let mut plan = Plan::new(Some(100), true, 60, 1);
        plan.claim(t0()).unwrap();
        assert_eq!(plan.claim(t0()), None);
    }

    #[test]
    fn released_segments_are_reclaimed_from_the_last_written_byte() {
        let mut plan = Plan::new(Some(100), true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        write(&mut plan, a.idx, 30);
        plan.reserve(a.idx, 5, t0()); // lost with the connection
        plan.release(a.idx);
        assert_eq!(plan.claim(t0()), Some(Claim { idx: 0, from: 30, end: 100, racing: false }));
    }

    #[test]
    fn unranged_downloads_restart_from_zero() {
        let mut plan = Plan::new(Some(100), false, 10, 1);
        let a = plan.claim(t0()).unwrap();
        write(&mut plan, a.idx, 30);
        assert_eq!(plan.claim(t0()), None);
        plan.release(a.idx);
        assert_eq!(plan.claim(t0()), Some(Claim { idx: 0, from: 0, end: 100, racing: false }));
        assert_eq!(plan.downloaded(), 0);
    }

    #[test]
    fn unknown_size_completes_when_the_stream_ends() {
        let mut plan = Plan::new(None, true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        assert!(plan.is_unbounded(a.idx));
        write(&mut plan, a.idx, 42);
        assert!(!plan.is_complete());
        plan.finish_unbounded(a.idx);
        assert!(plan.is_complete());
        assert_eq!(plan.downloaded(), 42);
    }

    #[test]
    fn races_a_silent_connection_once_nothing_is_left_to_split() {
        let mut plan = Plan::new(Some(100), true, 60, 1);
        let a = plan.claim(t0()).unwrap();
        plan.reserve(a.idx, 10, at(0.1));
        assert_eq!(plan.claim(at(1.5)), None, "silent, but not for long");

        let race = plan.claim(at(2.2)).unwrap();
        assert_eq!(race, Claim { idx: 0, from: 10, end: 100, racing: true });
        assert_eq!(plan.claim(at(5.0)), None, "one race per segment");

        let a_stop = plan.stopper(&a);
        let won = plan.take_over(race).unwrap();
        assert_eq!(won, Claim { idx: 1, from: 10, end: 100, racing: false });
        assert!(a_stop.is_cancelled(), "the silent connection is stopped");
        assert_eq!(plan.reserve(a.idx, 50, at(5.0)), (0, true), "and gets nothing more");
        assert_eq!(
            plan.segments().iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            [(0, 10), (10, 100)]
        );
    }

    #[test]
    fn a_racer_loses_when_the_connection_delivers_first() {
        let mut plan = Plan::new(Some(100), true, 60, 1);
        let a = plan.claim(t0()).unwrap();
        let race = plan.claim(at(3.0)).unwrap();
        assert!(race.racing, "no data at all for 3 s");
        let race_stop = plan.stopper(&race);

        plan.reserve(a.idx, 10, at(3.1));
        assert!(race_stop.is_cancelled());
        assert_eq!(plan.take_over(race), None);
        assert_eq!(plan.segments().len(), 1);
    }

    #[test]
    fn a_short_pause_is_raced_only_when_unusual() {
        let mut plan = Plan::new(Some(1_000_000), true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        // Data every 2 s: a 1.5 s pause is normal for this server.
        for i in 0..5 {
            plan.reserve(a.idx, 10, at(f64::from(i) * 2.0));
        }
        let split = plan.claim(at(9.5)).unwrap();
        assert!(!split.racing, "split instead of racing");
        // Far longer than any gap so far.
        let race = plan.claim(at(30.0)).unwrap();
        assert!(race.racing);
    }

    #[test]
    fn a_split_leaves_what_the_connection_gets_while_the_new_one_starts() {
        let mut plan = Plan::new(Some(10_000), true, 10, 1);
        let a = plan.claim(t0()).unwrap();
        // 1 s to the first data, then 100 bytes in that second.
        plan.reserve(a.idx, 100, at(1.0));
        let b = plan.claim(at(1.0)).unwrap();
        // 9,900 left; 100 more arrive while the new connection starts; it takes half the rest.
        assert_eq!((b.from, b.end), (100 + 100 + 4900, 10_000));
    }

    #[test]
    fn overlaps_bytes_already_on_disk() {
        let mut plan = Plan::from_segments(
            vec![
                Segment { start: 0, end: 100, downloaded: 100 },
                Segment { start: 100, end: 200, downloaded: 0 },
                Segment { start: 200, end: 300, downloaded: 30 },
                Segment { start: 300, end: 400, downloaded: 0 },
            ],
            true,
            10,
        );
        let b = plan.claim(t0()).unwrap();
        assert_eq!((b.from, plan.overlap(&b)), (100, OVERLAP), "the end of a finished segment");
        let c = plan.claim(t0()).unwrap();
        assert_eq!((c.from, plan.overlap(&c)), (230, 30), "its own bytes");
        let d = plan.claim(t0()).unwrap();
        assert_eq!((d.from, plan.overlap(&d)), (300, 0), "the segment before is not done");
        assert_eq!(plan.written_from_start(), 100);
    }
}
