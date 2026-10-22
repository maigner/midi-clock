//! The MIDI clock thread. It reads grid snapshots and emits 24 ticks per beat
//! at absolute times computed from the grid, plus Start/Stop when the grid's
//! `running` flag changes. Tempo and phase corrections in the grid stretch or
//! compress the tick spacing but never change the tick count, so slaves stay
//! on the right beat.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtio::{now, sleep_until, MidiOut};
use tracker::Grid;

#[derive(Clone, Debug)]
pub struct ClockConfig {
    /// Send every tick this much early (seconds) to absorb USB and slave latency.
    pub lead_s: f64,
    pub send_start_stop: bool,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self { lead_s: 0.0015, send_start_stop: true }
    }
}

/// Lateness histogram buckets (upper bounds in microseconds).
pub const LATE_BUCKETS_US: [u64; 8] = [50, 100, 200, 500, 1000, 2000, 5000, u64::MAX];

#[derive(Default)]
pub struct ClockStats {
    pub ticks: AtomicU64,
    pub starts: AtomicU64,
    pub stops: AtomicU64,
    pub late_sum_us: AtomicU64,
    pub late_max_us: AtomicU64,
    pub late_hist: [AtomicU64; 8],
    pub errors: AtomicU64,
}

impl ClockStats {
    fn record(&self, late: f64) {
        let us = (late.max(0.0) * 1e6) as u64;
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.late_sum_us.fetch_add(us, Ordering::Relaxed);
        self.late_max_us.fetch_max(us, Ordering::Relaxed);
        let b = LATE_BUCKETS_US.iter().position(|&ub| us < ub).unwrap_or(7);
        self.late_hist[b].fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> String {
        let n = self.ticks.load(Ordering::Relaxed);
        if n == 0 {
            return "no ticks".into();
        }
        let mean = self.late_sum_us.load(Ordering::Relaxed) as f64 / n as f64;
        let max = self.late_max_us.load(Ordering::Relaxed);
        let mut acc = 0u64;
        let mut p99 = 0;
        for (i, b) in self.late_hist.iter().enumerate() {
            acc += b.load(Ordering::Relaxed);
            if acc as f64 >= 0.99 * n as f64 {
                p99 = LATE_BUCKETS_US[i];
                break;
            }
        }
        format!(
            "{n} ticks, lateness mean {mean:.0} us, p99 < {} us, max {max} us, errors {}",
            if p99 == u64::MAX { "inf".to_string() } else { p99.to_string() },
            self.errors.load(Ordering::Relaxed)
        )
    }
}

/// Run the clock loop until `stop` is set. `get_grid` is called frequently
/// and must be cheap and lock-free.
pub fn run<F: FnMut() -> Grid>(mut get_grid: F, midi: &MidiOut, cfg: &ClockConfig, stop: &AtomicBool, stats: &ClockStats) {
    let mut started = false;
    let mut epoch = 0u32;
    let mut n = 0i64;
    let mut m = 0u32;
    let poll = 0.0015;
    loop {
        if stop.load(Ordering::Relaxed) {
            if started && cfg.send_start_stop {
                let _ = midi.stop();
            }
            return;
        }
        let g = get_grid();
        if !g.running {
            if started {
                started = false;
                if cfg.send_start_stop && midi.stop().is_err() {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                }
                stats.stops.fetch_add(1, Ordering::Relaxed);
            }
            sleep_until(now() + 0.002);
            continue;
        }
        if !started || g.epoch != epoch {
            // (Re)start: wait for the first beat of this epoch, send Start just before it.
            let t = g.beat_time(g.n0) - cfg.lead_s;
            let tnow = now();
            if t - tnow > poll {
                sleep_until(tnow + poll);
                continue;
            }
            epoch = g.epoch;
            n = g.n0;
            if started && cfg.send_start_stop {
                let _ = midi.stop();
            }
            sleep_until(t - 0.0005);
            if cfg.send_start_stop && midi.start().is_err() {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            stats.starts.fetch_add(1, Ordering::Relaxed);
            started = true;
            sleep_until(t);
            if midi.clock().is_err() {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            stats.record(now() - t);
            m = 1;
            continue;
        }
        let t = g.tick_time(n, m) - cfg.lead_s;
        let tnow = now();
        if t - tnow > g.period {
            // The grid's beat numbers jumped ahead (a relock numbered the
            // beats differently): pick up at the beat that is now.
            let (nb, _) = g.nearest_beat(tnow + cfg.lead_s);
            n = nb;
            m = 0;
            continue;
        }
        if t - tnow > poll {
            sleep_until(tnow + poll);
            continue;
        }
        sleep_until(t);
        if midi.clock().is_err() {
            stats.errors.fetch_add(1, Ordering::Relaxed);
        }
        stats.record(now() - t);
        m += 1;
        if m == 24 {
            m = 0;
            n += 1;
        }
        // If the grid jumped back by more than a beat (relock), resynchronise
        // rather than bursting a beat's worth of ticks.
        if g.tick_time(n, m) - cfg.lead_s < now() - g.period {
            let (nb, _) = g.nearest_beat(now() + cfg.lead_s);
            n = nb.max(n);
            m = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_summary_counts_buckets() {
        let s = ClockStats::default();
        s.record(0.00002);
        s.record(0.0003);
        s.record(0.003);
        assert_eq!(s.ticks.load(Ordering::Relaxed), 3);
        assert_eq!(s.late_max_us.load(Ordering::Relaxed), 3000);
        assert!(s.summary().contains("3 ticks"));
    }
}
