//! The MIDI clock thread. It reads grid snapshots and emits 24 ticks per beat
//! at absolute times computed from the grid, plus Start/Stop when the grid's
//! `running` flag changes. Tempo and phase corrections in the grid stretch or
//! compress the tick spacing but never change the tick count, so slaves stay
//! on the right beat.
//!
//! A grid with a `transport` comes from an external master (MIDI IN mode):
//! then no Start or Stop is made up here. The master's Start, Continue and
//! Stop go out as they came in, and the ticks take up the master's count.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtio::midi::{CLOCK, CONTINUE, START, STOP};
use rtio::{now, sleep_until, MidiOut};
use tracker::{Command, Grid};

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

/// The first tick after a Start or Continue handed on from the master waits
/// this long (seconds), so that a slave has taken the message in (the MIDI
/// specification recommends 1 ms).
const START_GAP: f64 = 0.001;

/// Where the clock goes. `MidiOut` for real, a recorder for tests.
pub trait ClockOut {
    /// Send one realtime message (`CLOCK`, `START`, `CONTINUE`, `STOP`).
    fn realtime(&self, msg: u8) -> bool;
}

impl ClockOut for MidiOut {
    fn realtime(&self, msg: u8) -> bool {
        self.send(&[msg]).is_ok()
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
pub fn run<F: FnMut() -> Grid>(mut get_grid: F, midi: &dyn ClockOut, cfg: &ClockConfig, stop: &AtomicBool, stats: &ClockStats) {
    // The slaves were told to play: Start or Continue went out, no Stop since.
    let mut started = false;
    // `n`/`m` is the next tick of `epoch`.
    let mut ticking = false;
    let mut epoch = 0u32;
    let mut n = 0i64;
    let mut m = 0u32;
    // With an external master: how many of its transport messages are dealt
    // with. `None` while the grid decides Start and Stop.
    let mut seen: Option<u32> = None;
    // A Continue went out and the master's clock has not resumed yet.
    let mut resume = false;
    // Earliest time of the next tick (`START_GAP`).
    let mut not_before = 0.0;
    let poll = 0.0015;
    loop {
        if stop.load(Ordering::Relaxed) {
            if started && cfg.send_start_stop {
                let _ = midi.realtime(STOP);
            }
            return;
        }
        let g = get_grid();
        match g.transport {
            None => seen = None,
            Some(tr) if seen == Some(tr.seq) => {}
            Some(tr) => {
                let msg = match (seen, tr.last) {
                    // The master has just taken over. What it said before
                    // is not repeated, but slaves started from here do not
                    // play on while the master is stopped.
                    (None, Some(Command::Start | Command::Continue)) => None,
                    (None, _) => started.then_some(STOP),
                    (Some(_), Some(Command::Start)) => Some(START),
                    (Some(_), Some(Command::Continue)) => Some(CONTINUE),
                    (Some(_), Some(Command::Stop)) => Some(STOP),
                    (Some(_), None) => None,
                };
                if seen.is_none() {
                    // Its ticks are counted differently from ours so far.
                    ticking = false;
                }
                seen = Some(tr.seq);
                if let Some(msg) = msg {
                    if cfg.send_start_stop && !midi.realtime(msg) {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    started = msg != STOP;
                    if started {
                        stats.starts.fetch_add(1, Ordering::Relaxed);
                        not_before = now() + START_GAP;
                    } else {
                        stats.stops.fetch_add(1, Ordering::Relaxed);
                    }
                    resume = msg == CONTINUE && (!ticking || !g.running || g.epoch != epoch);
                }
            }
        }
        if !g.running {
            if started && seen.is_none() {
                started = false;
                if cfg.send_start_stop && !midi.realtime(STOP) {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                }
                stats.stops.fetch_add(1, Ordering::Relaxed);
            }
            ticking = false;
            sleep_until(now() + 0.002);
            continue;
        }
        if let Some(tr) = g.transport {
            if !ticking || g.epoch != epoch {
                // The master's clock began or resumed: tick along from its
                // first tick, so that the slaves count what the master counts.
                let own = n * 24 + m as i64;
                let due = g.n0 * 24 + ((now() + cfg.lead_s - g.t0) / g.period * 24.0).ceil() as i64;
                let k = if resume && (own - tr.first_tick).abs() < 24 {
                    // After a Continue the slaves are where our ticks left
                    // them, which can be a tick off the master's count.
                    own
                } else if due - tr.first_tick > 24 {
                    // Joined a clock that has been running for a while.
                    due
                } else {
                    tr.first_tick
                };
                n = k.div_euclid(24);
                m = k.rem_euclid(24) as u32;
                epoch = g.epoch;
                ticking = true;
                resume = false;
            }
        } else if !started || g.epoch != epoch {
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
                let _ = midi.realtime(STOP);
            }
            sleep_until(t - 0.0005);
            if cfg.send_start_stop && !midi.realtime(START) {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            stats.starts.fetch_add(1, Ordering::Relaxed);
            started = true;
            ticking = true;
            sleep_until(t);
            if !midi.realtime(CLOCK) {
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
        sleep_until(t.max(not_before));
        if !midi.realtime(CLOCK) {
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
    use std::sync::Mutex;
    use tracker::Transport;

    /// (time, message)
    struct Recorder(Mutex<Vec<(f64, u8)>>);
    impl ClockOut for Recorder {
        fn realtime(&self, msg: u8) -> bool {
            self.0.lock().unwrap().push((now(), msg));
            true
        }
    }

    /// Run the clock on a grid that `script` changes as it goes; `script`
    /// gets the grid and a function that sleeps for some seconds.
    fn record(first: Grid, script: impl FnOnce(&Mutex<Grid>, &dyn Fn(f64)) + Send) -> Vec<(f64, u8)> {
        let grid = Mutex::new(first);
        let rec = Recorder(Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = ClockStats::default();
        let cfg = ClockConfig { lead_s: 0.0, send_start_stop: true };
        std::thread::scope(|sc| {
            sc.spawn(|| {
                script(&grid, &|s| sleep_until(now() + s));
                stop.store(true, Ordering::Relaxed);
            });
            run(|| *grid.lock().unwrap(), &rec, &cfg, &stop, &stats);
        });
        rec.0.into_inner().unwrap()
    }

    fn transport(ev: &[(f64, u8)]) -> Vec<u8> {
        ev.iter().map(|e| e.1).filter(|&b| b != CLOCK).collect()
    }

    /// Ticks between transport message `i` (from 0) and the next one, or the end.
    fn ticks_after(ev: &[(f64, u8)], i: usize) -> usize {
        let mut seen = 0;
        let mut n = 0;
        for e in ev {
            if e.1 != CLOCK {
                seen += 1;
            } else if seen == i + 1 {
                n += 1;
            }
        }
        n
    }

    /// The master's grid: 10 ms per tick, the beat holding tick `k` placed so that tick `k` is at `t`.
    fn master(epoch: u32, running: bool, seq: u32, last: Option<Command>, first_tick: i64, t: f64) -> Grid {
        let period = 0.24;
        let n0 = first_tick.div_euclid(24);
        let t0 = t - first_tick.rem_euclid(24) as f64 * period / 24.0;
        Grid { epoch, running, t0, n0, period, transport: Some(Transport { seq, last, first_tick }), ..Grid::default() }
    }

    #[test]
    fn the_grid_decides_start_and_stop_without_a_master() {
        let g = Grid { running: true, epoch: 1, t0: now() + 0.02, n0: 0, period: 0.24, ..Grid::default() };
        let ev = record(g, |grid, sleep| {
            sleep(0.075);
            grid.lock().unwrap().running = false;
            sleep(0.03);
        });
        assert_eq!(transport(&ev), [START, STOP], "{ev:?}");
        assert_eq!(ev[0].1, START);
        assert!((ev[0].0 - g.t0).abs() < 0.003, "Start {} s off the first beat", ev[0].0 - g.t0);
        // ticks at 0, 10, .. 50 ms; the Stop came at about 55 ms
        assert_eq!(ticks_after(&ev, 0), 6, "{ev:?}");
    }

    #[test]
    fn a_masters_start_stop_and_continue_are_passed_on() {
        // The master sends clock but has not said Start: ticks only.
        let ev = record(master(1, true, 0, None, 0, now()), |grid, sleep| {
            sleep(0.045);
            // Start, the first tick 5 ms later
            *grid.lock().unwrap() = master(1, false, 1, Some(Command::Start), 0, 0.0);
            sleep(0.005);
            *grid.lock().unwrap() = master(2, true, 1, Some(Command::Start), 0, now());
            sleep(0.045);
            // Stop after ticks 0..=4, the master goes on sending clock
            *grid.lock().unwrap() = master(2, false, 2, Some(Command::Stop), 0, 0.0);
            sleep(0.03);
            // Continue and tick 5 at once
            *grid.lock().unwrap() = master(3, true, 3, Some(Command::Continue), 5, now());
            sleep(0.025);
        });
        // the last Stop is ours: the program ends with slaves playing
        assert_eq!(transport(&ev), [START, STOP, CONTINUE, STOP], "{ev:?}");
        let before = ev.iter().take_while(|e| e.1 == CLOCK).count();
        assert!((4..=6).contains(&before), "{before} ticks before the Start: {ev:?}");
        assert_eq!(ticks_after(&ev, 0), 5, "ticks 0..=4 after the Start: {ev:?}");
        assert_eq!(ticks_after(&ev, 1), 0, "ticks while stopped: {ev:?}");
        assert_eq!(ticks_after(&ev, 2), 3, "ticks 5, 6, 7 after the Continue: {ev:?}");
        // a slave gets a moment between the Continue and the tick that came with it
        let i = ev.iter().position(|e| e.1 == CONTINUE).unwrap();
        let gap = ev[i + 1].0 - ev[i].0;
        assert!((START_GAP - 1e-4..0.004).contains(&gap), "tick {gap} s after the Continue");
    }

    #[test]
    fn ticks_missed_before_the_grid_ran_are_made_up_after_a_start() {
        // No tempo known at the Start: the grid runs from the master's
        // fourth tick on. The slaves still get every tick.
        let ev = record(master(0, false, 0, None, 0, 0.0), |grid, sleep| {
            sleep(0.01);
            *grid.lock().unwrap() = master(1, true, 1, Some(Command::Start), 0, now() - 0.035);
            sleep(0.03);
        });
        assert_eq!(transport(&ev)[0], START, "{ev:?}");
        // ticks 0..=3 at once, then 4, 5, 6 on time
        assert_eq!(ticks_after(&ev, 0), 7, "{ev:?}");
        assert!(ev[4].1 == CLOCK && ev[4].0 - ev[0].0 < 0.004, "{ev:?}");
    }

    #[test]
    fn a_master_taking_over_stops_what_the_grid_started() {
        let g = Grid { running: true, epoch: 1, t0: now() + 0.01, n0: 0, period: 0.24, ..Grid::default() };
        let ev = record(g, |grid, sleep| {
            sleep(0.035);
            // MIDI IN: a master that was stopped long ago and sends clock.
            // Its beat 50 is now; nothing of the 1200 ticks before is sent.
            *grid.lock().unwrap() = master(1, true, 8, Some(Command::Stop), 0, now() - 12.0);
            sleep(0.03);
            *grid.lock().unwrap() = master(1, false, 9, Some(Command::Start), 0, 0.0);
            sleep(0.01);
        });
        assert_eq!(transport(&ev), [START, STOP, START, STOP], "{ev:?}");
        let n = ticks_after(&ev, 1);
        assert!((2..=4).contains(&n), "{n} ticks of the stopped master: {ev:?}");
    }

    #[test]
    fn a_playing_master_taking_over_changes_nothing() {
        let g = Grid { running: true, epoch: 1, t0: now() + 0.01, n0: 0, period: 0.24, ..Grid::default() };
        let ev = record(g, |grid, sleep| {
            sleep(0.035);
            *grid.lock().unwrap() = master(4, true, 3, Some(Command::Start), 0, now() - 12.0);
            sleep(0.03);
        });
        assert_eq!(transport(&ev), [START, STOP], "only our Start and the Stop at the end: {ev:?}");
    }

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
