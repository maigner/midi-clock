//! External MIDI clock as master: 24 ticks per beat arrive on the MIDI
//! input and this turns them into the same `Grid` the tracker and the manual
//! clock produce, so the click, the MIDI clock output and the sequencers do
//! not care who is in charge.
//!
//! The tick times are noisy (USB delivers them a millisecond or two late,
//! in bursts), so the grid is a least-squares fit over the last `FIT_TICKS`
//! ticks rather than the last tick itself: the tempo is the slope, the phase
//! the intercept. That follows a DAW tempo ramp within a beat and keeps a
//! steady clock's jitter out of the notes.
//!
//! Start makes the next tick beat one. Without a Start (a device that only
//! ever sends clock, or one that was started before the mode was switched)
//! the first tick heard counts as a beat but the one is unknown; the ONE
//! button then names it.

use std::collections::VecDeque;

use crate::grid::{Grid, State};

/// Ticks the fit covers: two beats.
const FIT_TICKS: usize = 48;
/// Ticks needed for a tempo when none is known yet (a quarter beat).
const MIN_TICKS: usize = 6;
/// No tick for this long and the clock is gone (seconds).
pub const SILENCE_S: f64 = 1.0;
/// The most a beat may move per tick while running (seconds): the fit's
/// noise (USB delivers ticks in bursts) must not become audible.
const MAX_SLEW: f64 = 0.0005;
/// How much of the fitted period is taken over per tick.
const PERIOD_BLEND: f64 = 0.1;
/// A move of the fit this far from the grid is a jump (restart, doubled
/// clock detected), not jitter: snap instead of slewing.
const SNAP_S: f64 = 0.03;
/// Two ticks closer than this are never two real ticks (that would be
/// over 800 bpm): a device that sends every clock byte twice.
const DUP_GAP: f64 = 0.003;
/// Gaps looked at to decide whether the ticks come doubled.
const PAIR_HISTORY: usize = 32;
/// Tempo range accepted from the input; anything outside is treated as noise.
const PERIOD_MIN: f64 = 60.0 / 400.0;
const PERIOD_MAX: f64 = 60.0 / 20.0;

pub struct MidiFollower {
    grid: Grid,
    /// Tick index (24 per beat, counted from the last Start or first tick)
    /// and time of the recent ticks, oldest first.
    ticks: VecDeque<(i64, f64)>,
    next_tick: i64,
    /// Ticks while stopped only measure the tempo; they get their own count.
    idle_tick: i64,
    last_tick_t: Option<f64>,
    /// The last tempo measured, so a restart runs from its first tick.
    prior_period: Option<f64>,
    /// A Stop was received and no Start/Continue since.
    stopped: bool,
    /// Tick 0 came right after a Start, so beat 0 is the one.
    started_beat: bool,
    /// RMS residual of the last fit (seconds): how noisy the input is.
    residual: f64,
    /// Whether each of the last gaps was shorter than `DUP_GAP`, oldest first.
    gaps: VecDeque<bool>,
    /// The clock arrives doubled: every second tick is dropped.
    paired: bool,
}

impl MidiFollower {
    pub fn new(bar_len: u32) -> Self {
        Self {
            grid: Grid { bar_len: bar_len.max(1), confidence: 1.0, ..Grid::default() },
            ticks: VecDeque::with_capacity(FIT_TICKS + 1),
            next_tick: 0,
            idle_tick: 0,
            last_tick_t: None,
            prior_period: None,
            stopped: false,
            started_beat: false,
            residual: 0.0,
            gaps: VecDeque::with_capacity(PAIR_HISTORY + 1),
            paired: false,
        }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    /// RMS of the last fit's residuals in seconds: the input's own jitter.
    pub fn residual_s(&self) -> f64 {
        self.residual
    }

    /// A clock is arriving (a tick within `SILENCE_S` of `now`).
    pub fn alive(&self, now: f64) -> bool {
        self.last_tick_t.is_some_and(|t| now - t < SILENCE_S)
    }

    /// MIDI Start (0xFA): the next tick is tick 0 of beat 0, the one.
    pub fn start(&mut self, _t: f64) -> bool {
        self.stopped = false;
        self.ticks.clear();
        self.next_tick = 0;
        self.last_tick_t = None;
        self.started_beat = true;
        // The grid restarts on the first tick; until then keep it stopped so
        // the clock output sends Stop/Start around the new beat 0.
        self.set_running(false)
    }

    /// MIDI Continue (0xFB): ticks resume where they stopped.
    pub fn cont(&mut self, _t: f64) -> bool {
        self.stopped = false;
        self.ticks.clear();
        self.last_tick_t = None;
        false
    }

    /// MIDI Stop (0xFC).
    pub fn stop(&mut self, _t: f64) -> bool {
        self.stopped = true;
        self.ticks.clear();
        self.idle_tick = 0;
        self.set_running(false)
    }

    /// The clock is heard doubled (every byte twice) and is being halved.
    pub fn paired(&self) -> bool {
        self.paired
    }

    /// MIDI Clock (0xF8) at monotonic time `t`. Returns whether the grid changed.
    pub fn tick(&mut self, t: f64) -> bool {
        if let Some(last) = self.last_tick_t {
            let gap = t - last;
            // Doubled clock: a strict alternation of near-zero and real
            // gaps. Decided over `PAIR_HISTORY` gaps so that USB delivering
            // a few ticks in one burst does not look like it.
            let short = gap < DUP_GAP;
            self.gaps.push_back(short);
            while self.gaps.len() > PAIR_HISTORY {
                self.gaps.pop_front();
            }
            if self.gaps.len() == PAIR_HISTORY {
                let alternating = self.gaps.iter().zip(self.gaps.iter().skip(1)).all(|(a, b)| a != b);
                let none_short = self.gaps.iter().all(|&g| !g);
                if alternating && !self.paired {
                    // Half of the ticks counted so far were duplicates.
                    self.paired = true;
                    self.next_tick = self.next_tick.div_euclid(2);
                    self.idle_tick = 0;
                    self.ticks.clear();
                    self.prior_period = self.prior_period.map(|p| p * 2.0);
                } else if none_short && self.paired {
                    self.paired = false;
                    self.ticks.clear();
                    self.prior_period = self.prior_period.map(|p| p / 2.0);
                }
            }
            if self.paired && short {
                self.last_tick_t = Some(t);
                return false;
            }
            if gap >= SILENCE_S && !self.stopped {
                // The clock was gone; this is a fresh start of unknown phase.
                self.ticks.clear();
                self.next_tick = 0;
                self.started_beat = false;
                self.grid.downbeat_known = false;
            } else if gap >= SILENCE_S || gap > PERIOD_MAX / 24.0 || gap < 0.0 {
                self.ticks.clear();
            }
        }
        self.last_tick_t = Some(t);
        let k = if self.stopped {
            self.idle_tick += 1;
            self.idle_tick - 1
        } else {
            self.next_tick += 1;
            self.next_tick - 1
        };
        self.ticks.push_back((k, t));
        while self.ticks.len() > FIT_TICKS {
            self.ticks.pop_front();
        }
        let fit = self.fit();
        if let Some((p, _, r)) = fit {
            self.residual = r;
            if (PERIOD_MIN..=PERIOD_MAX).contains(&p) {
                self.prior_period = Some(p);
            }
        }
        if self.stopped {
            return false;
        }
        let (period, t_at_k) = match fit {
            Some((p, t_fit, _)) if (PERIOD_MIN..=PERIOD_MAX).contains(&p) => (p, t_fit),
            // Too few ticks since the (re)start: run from this tick at the
            // last known tempo rather than half a beat late.
            _ if self.ticks.len() == 1 && self.prior_period.is_some() => (self.prior_period.unwrap_or(0.5), t),
            _ => return false,
        };
        // The fit gives the time of tick k; the beat holding it is the grid's reference.
        let beat = k.div_euclid(24);
        let m = k.rem_euclid(24);
        let t_beat = t_at_k - m as f64 / 24.0 * period;
        let was_running = self.grid.running;
        let t_cur = self.grid.beat_time(beat);
        if was_running && (t_beat - t_cur).abs() < SNAP_S && (period / self.grid.period - 1.0).abs() < 0.1 {
            // Running: move towards the fit, never jump. Re-anchor at the
            // same beat first so the slew is measured on one beat.
            self.grid.n0 = beat;
            self.grid.t0 = t_cur + (t_beat - t_cur).clamp(-MAX_SLEW, MAX_SLEW);
            self.grid.period += PERIOD_BLEND * (period - self.grid.period);
        } else {
            self.grid.period = period;
            self.grid.t0 = t_beat;
            self.grid.n0 = beat;
        }
        if !was_running {
            self.grid.epoch = self.grid.epoch.wrapping_add(1);
            self.grid.running = true;
            if self.started_beat {
                self.grid.downbeat = 0;
                self.grid.downbeat_known = true;
            }
        }
        self.grid.state = State::Running;
        true
    }

    /// Nothing arrived for a while; call every few ms. Returns whether the grid changed.
    pub fn idle(&mut self, now: f64) -> bool {
        if self.grid.running && !self.alive(now) {
            self.ticks.clear();
            return self.set_running(false);
        }
        false
    }

    /// The ONE button: the beat nearest `t` is beat one. With a device that
    /// never sends Start the beat boundaries themselves are a guess, so the
    /// tick nearest `t` becomes a beat boundary too.
    pub fn set_one(&mut self, t: f64) -> bool {
        if !self.grid.running {
            return false;
        }
        if self.started_beat {
            let (n, _) = self.grid.nearest_beat(t);
            self.grid.downbeat = n.rem_euclid(self.grid.bar_len.max(1) as i64) as u32;
            self.grid.downbeat_known = true;
            return true;
        }
        // Renumber the ticks so that the one nearest `t` is tick 0 of a beat
        // that is the one. `n0` stays, so consumers see no jump in the count.
        let tick_dt = self.grid.period / 24.0;
        let k_near = ((t - self.grid.t0) / tick_dt).round() as i64 + self.grid.n0 * 24;
        let shift = k_near.rem_euclid(24);
        if shift != 0 {
            for (k, _) in self.ticks.iter_mut() {
                *k -= shift;
            }
            self.next_tick -= shift;
            self.grid.t0 += shift as f64 * tick_dt;
        }
        let n_one = (k_near - shift).div_euclid(24);
        self.grid.downbeat = n_one.rem_euclid(self.grid.bar_len.max(1) as i64) as u32;
        self.grid.downbeat_known = true;
        self.started_beat = true;
        true
    }

    fn set_running(&mut self, run: bool) -> bool {
        if self.grid.running == run {
            return false;
        }
        self.grid.running = run;
        self.grid.state = if run { State::Running } else { State::Stopped };
        true
    }

    /// Least squares over the recent ticks: (seconds per beat, fitted time of
    /// the newest tick, RMS residual in seconds).
    fn fit(&self) -> Option<(f64, f64, f64)> {
        let n = self.ticks.len();
        if n < MIN_TICKS {
            return None;
        }
        let (k0, t0) = self.ticks[0];
        let mut sx = 0.0;
        let mut sy = 0.0;
        let mut sxx = 0.0;
        let mut sxy = 0.0;
        for &(k, t) in &self.ticks {
            let x = (k - k0) as f64;
            let y = t - t0;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let nf = n as f64;
        let den = nf * sxx - sx * sx;
        if den <= 0.0 {
            return None;
        }
        let slope = (nf * sxy - sx * sy) / den;
        let icpt = (sy - slope * sx) / nf;
        let (k_last, _) = self.ticks[n - 1];
        let ss: f64 = self
            .ticks
            .iter()
            .map(|&(k, t)| {
                let r = (t - t0) - (icpt + slope * (k - k0) as f64);
                r * r
            })
            .sum();
        Some((slope * 24.0, t0 + icpt + slope * (k_last - k0) as f64, (ss / nf).sqrt()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` ticks from `from` at `period` per beat, each up to `jitter` late. Returns the next tick's time.
    fn run_ticks(f: &mut MidiFollower, from: f64, n: usize, period: f64, jitter: f64) -> f64 {
        let mut seed = 7u64;
        let mut t = from;
        for _ in 0..n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = ((seed >> 33) as f64 / (1u64 << 31) as f64) * jitter;
            f.tick(t + j);
            t += period / 24.0;
        }
        t
    }

    #[test]
    fn follows_a_started_clock_with_the_one_on_start() {
        let mut f = MidiFollower::new(4);
        assert!(!f.grid().running);
        f.start(10.0);
        let p = 60.0 / 120.0;
        run_ticks(&mut f, 10.0, 96, p, 0.002);
        let g = f.grid();
        assert!(g.running && g.downbeat_known);
        assert!((g.bpm() - 120.0).abs() < 0.5, "bpm {}", g.bpm());
        // tick 0 at 10.0 is beat 0, the one; USB lateness shows as a small positive bias
        let (n, err) = g.nearest_beat(10.0);
        assert_eq!(n, 0);
        assert!(err.abs() < 0.003, "err {err}");
        assert!(g.is_downbeat(0) && g.is_downbeat(4) && !g.is_downbeat(2));
    }

    #[test]
    fn runs_from_the_first_tick_once_a_tempo_is_known() {
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let p = 0.5;
        let t = run_ticks(&mut f, 0.0, 48, p, 0.0);
        f.stop(t);
        f.start(t + 3.0);
        assert!(!f.grid().running);
        assert!(f.tick(t + 3.0), "the first tick after a restart runs the clock");
        let g = f.grid();
        assert!(g.running && g.downbeat_known);
        assert_eq!(g.n0, 0);
        assert!((g.beat_time(0) - (t + 3.0)).abs() < 1e-9);
        assert!((g.period - p).abs() < 1e-6);
    }

    #[test]
    fn stop_and_continue() {
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let p = 0.5;
        let t = run_ticks(&mut f, 0.0, 48, p, 0.0);
        assert!(f.stop(t));
        assert!(!f.grid().running);
        let t2 = run_ticks(&mut f, t + 2.0, 24, p, 0.0);
        assert!(!f.grid().running, "ticks while stopped do not run the clock");
        f.cont(t2);
        run_ticks(&mut f, t2, 24, p, 0.0);
        let g = f.grid();
        assert!(g.running && g.downbeat_known);
        // 48 ticks before the Stop: the clock continues at beat 2
        assert!((g.beat_time(2) - t2).abs() < 1e-6, "beat 2 at {} not {}", g.beat_time(2), t2);
    }

    #[test]
    fn a_clock_only_device_runs_on_its_ticks_and_one_can_be_set() {
        let mut f = MidiFollower::new(4);
        let p = 60.0 / 100.0;
        let t = run_ticks(&mut f, 5.0, 60, p, 0.001);
        let g = f.grid();
        assert!(g.running && !g.downbeat_known);
        assert!((g.bpm() - 100.0).abs() < 0.5);
        // press ONE 5 ticks after a beat boundary: beats move onto that tick
        let press = 5.0 + 36.0 * p / 24.0 + 5.0 * p / 24.0;
        assert!(f.set_one(press));
        let g = f.grid();
        let (n, err) = g.nearest_beat(press);
        assert!(err.abs() < 0.005, "err {err}");
        assert!(g.downbeat_known && g.is_downbeat(n));
        // later ticks keep that phase
        run_ticks(&mut f, t, 48, p, 0.001);
        let g2 = f.grid();
        assert!((g2.nearest_beat(press).1).abs() < 0.005);
        assert!(g2.is_downbeat(n));
    }

    #[test]
    fn silence_stops_the_clock_and_a_tempo_change_is_followed() {
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let t = run_ticks(&mut f, 0.0, 48, 0.5, 0.0);
        assert!(f.grid().running);
        assert!(!f.idle(t + 0.5));
        assert!(f.idle(t + 1.5));
        assert!(!f.grid().running);
        // back, faster, without a Start: runs, the one unknown
        run_ticks(&mut f, t + 2.0, 72, 60.0 / 140.0, 0.001);
        let g = f.grid();
        assert!(g.running && !g.downbeat_known);
        assert!((g.bpm() - 140.0).abs() < 1.0, "bpm {}", g.bpm());
    }

    #[test]
    fn ticks_arriving_in_bursts_give_a_steady_grid() {
        // USB holds every third tick back until the next one, so two ticks
        // arrive 0.5 ms apart now and then. The grid must not wobble; it
        // may sit a constant third of a tick late (a latency that the
        // lead settings absorb). Not a strict alternation, so not a
        // doubled clock.
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let p = 60.0 / 176.0;
        let dt = p / 24.0;
        let mut worst_move: f64 = 0.0;
        let mut errs = Vec::new();
        for k in 0..480i64 {
            let t = if k % 3 == 0 { (k + 1) as f64 * dt } else if k % 3 == 1 { k as f64 * dt + 0.0005 } else { k as f64 * dt };
            let before = f.grid();
            f.tick(t);
            let g = f.grid();
            if k >= 96 {
                if before.running {
                    worst_move = worst_move.max((g.beat_time(g.n0) - before.beat_time(g.n0)).abs());
                }
                let n = k / 24;
                errs.push(g.beat_time(n) - n as f64 * p);
            }
        }
        assert!(!f.paired());
        let (lo, hi) = errs.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &e| (lo.min(e), hi.max(e)));
        assert!(worst_move <= MAX_SLEW + 1e-9, "a beat moved by {worst_move}");
        assert!(hi - lo < 0.0015, "beats wobble by {} ms", (hi - lo) * 1000.0);
        let mean = errs.iter().sum::<f64>() / errs.len() as f64;
        assert!((mean - dt / 3.0).abs() < 0.002, "offset {} ms, expected about a third of a tick", mean * 1000.0);
        assert!((f.grid().bpm() - 176.0).abs() < 0.5, "bpm {}", f.grid().bpm());
    }

    #[test]
    fn a_doubled_clock_is_halved() {
        // Every clock byte twice, 0.5 ms apart: 48 ticks per beat at 88 bpm.
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let p = 60.0 / 88.0;
        let dt = p / 24.0;
        for k in 0..(24 * 16) {
            let t = k as f64 * dt;
            f.tick(t);
            f.tick(t + 0.0005);
        }
        assert!(f.paired());
        let g = f.grid();
        assert!((g.bpm() - 88.0).abs() < 0.5, "bpm {}", g.bpm());
        // beat 8 was at tick 8*24, i.e. t = 8 p
        let (n, err) = g.nearest_beat(8.0 * p);
        assert_eq!(n, 8, "beat numbering after the halving");
        assert!(err.abs() < 0.003, "err {err}");
        assert!(g.downbeat_known && g.is_downbeat(8));
    }

    #[test]
    fn jitter_does_not_shake_the_grid() {
        let mut f = MidiFollower::new(4);
        f.start(0.0);
        let p = 0.5;
        run_ticks(&mut f, 0.0, 48, p, 0.004);
        let mut worst: f64 = 0.0;
        let mut t = 48.0 * p / 24.0;
        for i in 48..192 {
            let j = if i % 3 == 0 { 0.004 } else { 0.0 };
            f.tick(t + j);
            let g = f.grid();
            let n = i / 24;
            worst = worst.max((g.beat_time(n) - n as f64 * p).abs());
            t += p / 24.0;
        }
        assert!(worst < 0.003, "grid moved by {worst}");
    }
}
