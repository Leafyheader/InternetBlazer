//! The speed shown for a running download.

use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

/// How far back the speed looks. Long enough to even out bursts, as data arrives in bursts even
/// on a steady line, and short enough to follow a real change within a few seconds.
const WINDOW: Duration = Duration::from_secs(3);

/// Average speed over the last [`WINDOW`], from a running count of bytes received.
///
/// The count must be of bytes as they arrive from the network, not as they are written to disk:
/// writes come in buffer-sized steps, which would make the speed jump between ticks.
pub(crate) struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    /// Starts measuring from `bytes` received so far.
    pub fn new(now: Instant, bytes: u64) -> Self {
        Self { samples: VecDeque::from([(now, bytes)]) }
    }

    /// Records that `bytes` have been received by `now`, and returns bytes per second.
    pub fn sample(&mut self, now: Instant, bytes: u64) -> u64 {
        self.samples.push_back((now, bytes));
        // Keep one sample at least a window old, so the average spans the whole window.
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
        let (since, before) = self.samples[0];
        let secs = now.duration_since(since).as_secs_f64();
        match secs > 0.0 {
            true => (bytes.saturating_sub(before) as f64 / secs) as u64,
            false => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evens_out_bursts() {
        let start = Instant::now();
        let mut meter = SpeedMeter::new(start, 0);
        let tick = Duration::from_millis(250);
        // 1 MB/s arriving as one 512 KB burst every half second.
        let mut speed = 0;
        for i in 1..=40u32 {
            let bytes = u64::from(i / 2) * 512 * 1024;
            speed = meter.sample(start + tick * i, bytes);
        }
        let mbps = speed as f64 / (1024.0 * 1024.0);
        assert!((0.9..1.1).contains(&mbps), "{mbps}");
    }

    #[test]
    fn falls_to_zero_when_data_stops() {
        let start = Instant::now();
        let mut meter = SpeedMeter::new(start, 0);
        meter.sample(start + Duration::from_secs(1), 1_000_000);
        let speed = meter.sample(start + Duration::from_secs(5), 1_000_000);
        assert!(speed < 300_000, "{speed}");
        let speed = meter.sample(start + Duration::from_secs(9), 1_000_000);
        assert_eq!(speed, 0);
    }
}
