//! Manual clock: tap tempo and direct tempo setting. It produces the same
//! `Grid` as the tracker, so the click and the MIDI clock do not care who is
//! in charge.
//!
//! Tap rules:
//! - Taps closer together than `tap_timeout_s` form a sequence; the tempo is
//!   the mean interval of the last `max_taps` taps.
//! - Every tap puts a beat exactly on the tap ("the beat is where I tap").
//! - When stopped, the second tap arms the clock: the first tap of the
//!   sequence is beat 0 (the downbeat) and the clock starts on the next
//!   multiple of `bar_len` beats. Tap "1 2 3 4" and the sequencers start on
//!   the following "1". The click already sounds during the count-in.
//! - When running, the first tap of a new sequence also marks the downbeat.

use std::collections::VecDeque;

use crate::grid::{Grid, State};

#[derive(Clone, Debug)]
pub struct ManualConfig {
    pub bpm_min: f64,
    pub bpm_max: f64,
    /// Taps farther apart than this start a new sequence (seconds).
    pub tap_timeout_s: f64,
    /// Number of taps averaged for the tempo.
    pub max_taps: usize,
    /// Delay between pressing Start and the first beat (seconds).
    pub start_delay_s: f64,
    pub bar_len: u32,
}

impl Default for ManualConfig {
    fn default() -> Self {
        Self { bpm_min: 30.0, bpm_max: 300.0, tap_timeout_s: 2.0, max_taps: 8, start_delay_s: 0.15, bar_len: 4 }
    }
}

pub struct ManualClock {
    cfg: ManualConfig,
    grid: Grid,
    taps: VecDeque<f64>,
}

impl ManualClock {
    pub fn new(cfg: ManualConfig, bpm: f64) -> Self {
        let bpm = bpm.clamp(cfg.bpm_min, cfg.bpm_max);
        let grid = Grid { period: 60.0 / bpm, bar_len: cfg.bar_len.max(1), confidence: 1.0, ..Grid::default() };
        Self { cfg, grid, taps: VecDeque::new() }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn bpm(&self) -> f64 {
        self.grid.bpm()
    }

    /// Number of taps in the current sequence.
    pub fn taps(&self) -> usize {
        self.taps.len()
    }

    /// Take over from another grid (mode switch): tempo, phase, beat count and
    /// running state continue unchanged.
    pub fn adopt(&mut self, g: &Grid) {
        self.grid = *g;
        self.grid.transport = None;
        self.grid.state = if g.running { State::Running } else { State::Stopped };
        self.grid.confidence = 1.0;
        self.grid.bar_len = self.cfg.bar_len.max(1);
        self.taps.clear();
    }

    /// Set the tempo, keeping the phase of the beat nearest to `now`.
    pub fn set_bpm(&mut self, bpm: f64, now: f64) {
        let bpm = bpm.clamp(self.cfg.bpm_min, self.cfg.bpm_max);
        if self.grid.running {
            let (n, _) = self.grid.nearest_beat(now);
            // Do not move a start beat that has not happened yet.
            let n = n.max(self.grid.n0.min(n));
            self.grid.t0 = self.grid.beat_time(n);
            self.grid.n0 = n;
        }
        self.grid.period = 60.0 / bpm;
        self.taps.clear();
    }

    /// Step the tempo to the next integer BPM in the direction of `delta`.
    pub fn nudge_bpm(&mut self, delta: f64, now: f64) {
        let bpm = self.bpm();
        let target = if delta > 0.0 { (bpm + 1e-6).floor() + delta } else { (bpm - 1e-6).ceil() + delta };
        self.set_bpm(target, now);
    }

    /// Register a tap at monotonic time `t` (already corrected for touch latency).
    pub fn tap(&mut self, t: f64) {
        if let Some(&last) = self.taps.back() {
            if t - last < 0.05 {
                return; // contact bounce
            }
            if t - last > self.cfg.tap_timeout_s {
                self.taps.clear();
            }
        }
        self.taps.push_back(t);
        while self.taps.len() > self.cfg.max_taps {
            self.taps.pop_front();
        }
        let k = self.taps.len();
        if k >= 2 {
            let first = self.taps[0];
            let p = (t - first) / (k - 1) as f64;
            self.grid.period = p.clamp(60.0 / self.cfg.bpm_max, 60.0 / self.cfg.bpm_min);
        }
        let bar = self.grid.bar_len.max(1) as i64;
        if self.grid.running {
            let (n, _) = self.grid.nearest_beat(t);
            if k == 1 {
                self.grid.downbeat = n.rem_euclid(bar) as u32;
                self.grid.downbeat_known = true;
            }
            if n < self.grid.n0 {
                // Still counting in towards the start beat: keep it, move the grid.
                self.grid.t0 = t + (self.grid.n0 - n) as f64 * self.grid.period;
            } else {
                self.grid.t0 = t;
                self.grid.n0 = n;
            }
        } else if k >= 2 {
            // The first tap was beat 0; start on the next multiple of the bar.
            let last = (k - 1) as i64;
            let start = ((last + bar) / bar) * bar;
            self.grid.t0 = t + (start - last) as f64 * self.grid.period;
            self.grid.n0 = start;
            self.grid.downbeat = 0;
            self.grid.downbeat_known = true;
            self.grid.epoch = self.grid.epoch.wrapping_add(1);
            self.grid.running = true;
            self.grid.state = State::Running;
        }
        self.grid.confidence = 1.0;
    }

    /// The beat nearest to `t` is beat one of its bar (the ONE button).
    /// Nothing happens while the clock is not running.
    pub fn set_one(&mut self, t: f64) -> bool {
        if !self.grid.running {
            return false;
        }
        let (n, _) = self.grid.nearest_beat(t);
        self.grid.downbeat = n.rem_euclid(self.grid.bar_len.max(1) as i64) as u32;
        self.grid.downbeat_known = true;
        true
    }

    /// Start the clock at the current tempo, first beat after `start_delay_s`.
    pub fn start(&mut self, now: f64) {
        if self.grid.running {
            return;
        }
        self.grid.t0 = now + self.cfg.start_delay_s;
        self.grid.n0 = 0;
        // START defines beat 0 as the one: the sequencers begin their bar there.
        self.grid.downbeat = 0;
        self.grid.downbeat_known = true;
        self.grid.epoch = self.grid.epoch.wrapping_add(1);
        self.grid.running = true;
        self.grid.state = State::Running;
        self.grid.confidence = 1.0;
        self.taps.clear();
    }

    pub fn stop(&mut self) {
        self.grid.running = false;
        self.grid.state = State::Stopped;
        self.taps.clear();
    }

    pub fn toggle(&mut self, now: f64) {
        if self.grid.running {
            self.stop();
        } else {
            self.start(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> ManualClock {
        ManualClock::new(ManualConfig::default(), 120.0)
    }

    #[test]
    fn four_taps_start_on_the_following_one() {
        let mut c = clock();
        let p = 0.5; // 120 bpm
        for i in 0..4 {
            c.tap(10.0 + i as f64 * p);
        }
        let g = c.grid();
        assert!(g.running);
        assert!((g.period - p).abs() < 1e-9);
        // beat 0 was the first tap; the clock starts on beat 4, one period after tap 4
        assert_eq!(g.n0, 4);
        assert!((g.beat_time(4) - 12.0).abs() < 1e-9);
        assert!((g.beat_time(0) - 10.0).abs() < 1e-9);
        assert!(g.is_downbeat(4));
        assert_eq!(g.epoch, 1);
    }

    #[test]
    fn two_taps_wait_for_the_bar() {
        let mut c = clock();
        c.tap(10.0);
        c.tap(10.6);
        let g = c.grid();
        assert!(g.running);
        assert!((g.bpm() - 100.0).abs() < 1e-6);
        assert_eq!(g.n0, 4);
        assert!((g.beat_time(4) - (10.6 + 3.0 * 0.6)).abs() < 1e-9);
    }

    #[test]
    fn tap_while_running_moves_the_beat_onto_the_tap() {
        let mut c = clock();
        c.start(10.0);
        let g = c.grid();
        let t = g.beat_time(6) + 0.03; // 30 ms late on beat 6
        c.tap(t);
        let g2 = c.grid();
        assert_eq!(g2.epoch, g.epoch);
        assert!((g2.beat_time(6) - t).abs() < 1e-9);
        assert_eq!(g2.downbeat, 2, "first tap of a sequence marks the downbeat");
        // Tempo unchanged with a single tap, then set by the next taps.
        assert!((g2.period - 0.5).abs() < 1e-9);
        c.tap(t + 0.4);
        c.tap(t + 0.8);
        assert!((c.grid().period - 0.4).abs() < 1e-9);
    }

    #[test]
    fn nudge_snaps_to_integers_and_keeps_phase() {
        let mut c = clock();
        c.start(0.0);
        c.set_bpm(121.37, 0.0);
        let g = c.grid();
        let now = g.beat_time(8) + 0.1;
        c.nudge_bpm(1.0, now);
        let g2 = c.grid();
        assert!((g2.bpm() - 122.0).abs() < 1e-9);
        assert!((g2.beat_time(8) - g.beat_time(8)).abs() < 1e-9, "beat 8 must not move");
        c.nudge_bpm(-5.0, now);
        assert!((c.grid().bpm() - 117.0).abs() < 1e-9);
    }

    #[test]
    fn adopt_keeps_the_grid_and_stop_clears() {
        let mut c = clock();
        let transport = Some(crate::Transport::default());
        let g = Grid { running: true, epoch: 7, t0: 5.0, n0: 12, period: 0.4, state: State::Coasting, transport, ..Grid::default() };
        c.adopt(&g);
        let a = c.grid();
        assert_eq!((a.epoch, a.n0, a.t0, a.period, a.state), (7, 12, 5.0, 0.4, State::Running));
        assert_eq!(a.transport, None, "the manual clock decides Start/Stop itself");
        c.stop();
        assert!(!c.grid().running);
        c.start(20.0);
        assert_eq!(c.grid().epoch, 8);
        assert!((c.grid().beat_time(0) - 20.15).abs() < 1e-9);
    }
}
