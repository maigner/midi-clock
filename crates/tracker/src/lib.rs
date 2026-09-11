//! Predictive beat tracker.
//!
//! Input: one ODF value per hop plus onset events. Output: a `Grid`, i.e. the
//! time of a reference beat, its index, the period, and whether the clock is
//! running. Consumers (click, MIDI clock) run *from the grid* and therefore
//! emit on the predicted beat, not after the detected one. The grid is
//! corrected by a phase-locked loop fed with onsets and by periodic tempo
//! re-estimation from the ODF autocorrelation.

pub mod grid;
pub mod manual;
pub mod tempo;

pub use grid::{Grid, State};
pub use manual::{ManualClock, ManualConfig};

use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct TrackerConfig {
    /// Seconds per hop.
    pub hop_dt: f64,
    pub bpm_min: f64,
    pub bpm_max: f64,
    /// Centre of the log-normal tempo prior.
    pub bpm_center: f64,
    /// Width of the tempo prior in octaves.
    pub bpm_width_octaves: f64,
    /// ODF history kept for tempo estimation (seconds).
    pub history_s: f64,
    pub estimate_interval_s: f64,
    /// Minimum normalised comb score for a tempo estimate to count.
    pub min_tempo_strength: f64,
    /// Bonus factor for candidates within 5 % of the current tempo.
    pub tempo_hysteresis: f64,
    /// Phase correction gain per accepted onset.
    pub kp: f64,
    /// Period correction gain per accepted onset (seconds of period per second of error).
    pub ki: f64,
    /// Blend factor towards each new period estimate.
    pub period_blend: f64,
    /// Onsets farther than this fraction of a period from a beat are ignored.
    pub accept_window: f64,
    /// Maximum phase shift per correction, as fraction of a period.
    pub max_phase_slew: f64,
    /// Consecutive agreeing tempo estimates required before locking.
    pub lock_estimates: usize,
    /// Missed beats before the tracker declares it is coasting.
    pub coast_beats: u32,
    /// Seconds without onsets before the clock is stopped.
    pub stop_after_s: f64,
    /// Systematic delay between a true onset and its ODF peak (seconds).
    pub onset_offset_s: f64,
    pub bar_len: u32,
    /// Onset strength at which a correction gets full weight.
    pub full_weight_strength: f64,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            hop_dt: 128.0 / 48000.0,
            bpm_min: 60.0,
            bpm_max: 200.0,
            bpm_center: 110.0,
            bpm_width_octaves: 0.8,
            history_s: 4.0,
            estimate_interval_s: 0.25,
            min_tempo_strength: 0.12,
            tempo_hysteresis: 0.3,
            kp: 0.45,
            ki: 0.1,
            period_blend: 0.3,
            accept_window: 0.2,
            max_phase_slew: 0.1,
            lock_estimates: 2,
            coast_beats: 3,
            stop_after_s: 8.0,
            onset_offset_s: 0.0,
            bar_len: 4,
            full_weight_strength: 6.0,
        }
    }
}

/// One hop of analysis input.
#[derive(Clone, Copy, Debug)]
pub struct HopInput {
    /// Monotonic time of the end of this hop.
    pub time: f64,
    pub odf: f32,
    pub kick: f32,
    /// Onset (already corrected for `onset_offset_s`): time and strength.
    pub onset: Option<(f64, f32)>,
}

pub struct Tracker {
    cfg: TrackerConfig,
    hist_len: usize,
    odf: Vec<f32>,
    kick: Vec<f32>,
    hops: u64,
    last_hop_time: f64,
    grid: Grid,
    last_onset_time: f64,
    last_est_hop: u64,
    est_hops: usize,
    est_history: Vec<f64>,
    disagree: u32,
    next_eval_beat: i64,
    hit_beat: i64,
    misses: u32,
    beat_kick: VecDeque<(i64, f32)>,
    est: tempo::Estimator,
    /// Diagnostics.
    pub last_tempo_strength: f64,
    pub corrections: u64,
    pub relocks: u64,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        let hist_len = (cfg.history_s / cfg.hop_dt).round() as usize;
        let est_hops = (cfg.estimate_interval_s / cfg.hop_dt).round().max(1.0) as usize;
        let est = tempo::Estimator::new(hist_len, cfg.hop_dt, cfg.bpm_min, cfg.bpm_max);
        Self {
            hist_len,
            odf: vec![0.0; hist_len],
            kick: vec![0.0; hist_len],
            hops: 0,
            last_hop_time: 0.0,
            grid: Grid { bar_len: cfg.bar_len, ..Grid::default() },
            last_onset_time: f64::NEG_INFINITY,
            last_est_hop: 0,
            est_hops,
            est_history: Vec::new(),
            disagree: 0,
            next_eval_beat: 0,
            hit_beat: i64::MIN,
            misses: 0,
            beat_kick: VecDeque::new(),
            est,
            last_tempo_strength: 0.0,
            corrections: 0,
            relocks: 0,
            cfg,
        }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn config(&self) -> &TrackerConfig {
        &self.cfg
    }

    /// Continue from a grid produced elsewhere (e.g. the manual clock). The
    /// clock keeps running at that tempo and phase; onsets correct it from
    /// there, and the usual coast/stop rules apply.
    pub fn adopt(&mut self, g: Grid, now: f64) {
        self.grid = g;
        self.grid.bar_len = self.cfg.bar_len;
        self.grid.state = if g.running { State::Coasting } else { State::Idle };
        self.grid.confidence = if g.running { 0.3 } else { 0.0 };
        self.next_eval_beat = if g.running { self.grid.first_beat_at_or_after(now) } else { 0 };
        self.hit_beat = i64::MIN;
        self.misses = 0;
        self.disagree = 0;
        self.last_onset_time = now;
        self.last_est_hop = self.hops;
        self.est_history.clear();
        self.beat_kick.clear();
    }

    /// Throw away the tempo estimate and lock again from the ODF history. The
    /// clock keeps running until the new estimate is in (hot relock).
    pub fn relock(&mut self) {
        if self.grid.state == State::Idle {
            return;
        }
        self.grid.state = State::Locking;
        self.est_history.clear();
        self.last_est_hop = 0;
        self.disagree = 0;
    }

    #[inline]
    fn slot(&self, hop: i64) -> Option<usize> {
        if hop < 0 || hop >= self.hops as i64 || self.hops as i64 - hop > self.hist_len as i64 {
            None
        } else {
            Some((hop as usize) % self.hist_len)
        }
    }

    fn hop_of_time(&self, t: f64) -> i64 {
        self.hops as i64 - 1 - ((self.last_hop_time - t) / self.cfg.hop_dt).round() as i64
    }

    /// Feed one hop. Returns true when the published grid changed.
    pub fn process_hop(&mut self, inp: HopInput) -> bool {
        let slot = (self.hops as usize) % self.hist_len;
        self.odf[slot] = inp.odf.max(0.0);
        self.kick[slot] = inp.kick.max(0.0);
        self.hops += 1;
        self.last_hop_time = inp.time;
        if let Some((t, _)) = inp.onset {
            self.last_onset_time = t;
        }
        let before = self.grid;
        match self.grid.state {
            State::Idle | State::Stopped => {
                if inp.onset.is_some() {
                    self.grid.state = State::Locking;
                    self.est_history.clear();
                    self.last_est_hop = self.hops;
                }
            }
            State::Locking => self.step_locking(&inp),
            State::Running | State::Coasting => self.step_running(&inp),
        }
        self.grid != before
    }

    fn step_locking(&mut self, inp: &HopInput) {
        if inp.time - self.last_onset_time > self.cfg.stop_after_s {
            self.grid.state = if self.grid.running { State::Stopped } else { State::Idle };
            self.grid.running = false;
            return;
        }
        if self.hops - self.last_est_hop < self.est_hops as u64 {
            return;
        }
        self.last_est_hop = self.hops;
        let current = if self.grid.running { Some(self.grid.period / self.cfg.hop_dt) } else { None };
        let Some((p, strength)) = self.estimate_period(current) else {
            return;
        };
        self.last_tempo_strength = strength;
        if strength < self.cfg.min_tempo_strength {
            return;
        }
        self.est_history.push(p);
        let n = self.est_history.len();
        if n < self.cfg.lock_estimates {
            return;
        }
        let recent = &self.est_history[n - self.cfg.lock_estimates..];
        let mean = recent.iter().sum::<f64>() / recent.len() as f64;
        if recent.iter().any(|&x| (x / mean - 1.0).abs() > 0.04) {
            return;
        }
        let Some(last_beat) = self.est.phase(&self.odf, self.hops, self.hist_len, mean) else {
            return;
        };
        let period = mean * self.cfg.hop_dt;
        let mut t_last = self.last_hop_time - last_beat * self.cfg.hop_dt - self.cfg.onset_offset_s;
        // First clocked beat must be far enough in the future to schedule Start.
        let mut t0 = t_last + period;
        while t0 < inp.time + 0.03 {
            t_last = t0;
            t0 += period;
        }
        let _ = t_last;
        self.grid.period = period;
        if self.grid.running {
            // Hot relock: keep counting beats, just re-place the grid.
            let (n, _) = self.grid.nearest_beat(t0);
            self.grid.t0 = t0;
            self.grid.n0 = n;
            self.next_eval_beat = n;
            self.relocks += 1;
        } else {
            self.grid.t0 = t0;
            self.grid.n0 = 0;
            self.grid.epoch = self.grid.epoch.wrapping_add(1);
            self.grid.running = true;
            self.grid.downbeat = 0;
            self.next_eval_beat = 0;
            self.beat_kick.clear();
        }
        self.grid.confidence = 0.5;
        self.grid.state = State::Running;
        self.hit_beat = i64::MIN;
        self.misses = 0;
        self.disagree = 0;
        self.est_history.clear();
    }

    fn step_running(&mut self, inp: &HopInput) {
        let p = self.grid.period;
        // 1. Phase-locked loop on onsets.
        if let Some((t_on, strength)) = inp.onset {
            let (n, err) = self.grid.nearest_beat(t_on);
            if err.abs() <= self.cfg.accept_window * p && n >= self.next_eval_beat - 1 {
                let w = (strength as f64 / self.cfg.full_weight_strength).clamp(0.15, 1.0);
                let max_slew = self.cfg.max_phase_slew * p;
                let dphase = (self.cfg.kp * err * w).clamp(-max_slew, max_slew);
                let dperiod = (self.cfg.ki * err * w).clamp(-0.02 * p, 0.02 * p);
                self.grid.t0 += dphase;
                self.grid.period += dperiod;
                self.hit_beat = n;
                self.corrections += 1;
                if self.grid.state == State::Coasting {
                    self.grid.state = State::Running;
                    self.misses = 0;
                }
            }
        }
        // 2. Evaluate beats whose acceptance window has closed.
        while self.grid.beat_time(self.next_eval_beat) + self.cfg.accept_window * self.grid.period < inp.time {
            let n = self.next_eval_beat;
            let hit = self.hit_beat == n;
            self.grid.confidence = (self.grid.confidence * 0.85 + if hit { 0.15 } else { 0.0 }).clamp(0.0, 1.0);
            if hit {
                self.misses = 0;
            } else {
                self.misses += 1;
            }
            self.accumulate_downbeat(n);
            self.next_eval_beat += 1;
        }
        if self.misses >= self.cfg.coast_beats && self.grid.state == State::Running {
            self.grid.state = State::Coasting;
        }
        if self.grid.state == State::Coasting && inp.time - self.last_onset_time > self.cfg.stop_after_s {
            self.grid.state = State::Stopped;
            self.grid.running = false;
            self.grid.confidence = 0.0;
            self.est_history.clear();
            return;
        }
        // 3. Periodic tempo re-estimation.
        if self.hops - self.last_est_hop >= self.est_hops as u64 {
            self.last_est_hop = self.hops;
            let cur = self.grid.period / self.cfg.hop_dt;
            if let Some((p_est, strength)) = self.estimate_period(Some(cur)) {
                self.last_tempo_strength = strength;
                if strength >= self.cfg.min_tempo_strength {
                    let ratio = p_est / cur;
                    if (ratio - 1.0).abs() < 0.15 {
                        let new_p = self.grid.period + self.cfg.period_blend * (p_est * self.cfg.hop_dt - self.grid.period);
                        // Re-anchor at the most recent beat so the change does not move past beats.
                        let (n_ref, _) = self.grid.nearest_beat(inp.time);
                        let t_ref = self.grid.beat_time(n_ref);
                        self.grid.t0 = t_ref;
                        self.grid.n0 = n_ref;
                        self.grid.period = new_p;
                        self.disagree = 0;
                    } else {
                        self.disagree += 1;
                        if self.disagree >= 3 {
                            self.disagree = 0;
                            self.grid.state = State::Locking;
                            self.est_history.clear();
                            self.last_est_hop = 0;
                        }
                    }
                }
            }
        }
    }

    fn estimate_period(&mut self, current: Option<f64>) -> Option<(f64, f64)> {
        self.est.estimate(
            &self.odf,
            self.hops,
            self.hist_len,
            self.cfg.bpm_center,
            self.cfg.bpm_width_octaves,
            current,
            self.cfg.tempo_hysteresis,
        )
    }

    fn accumulate_downbeat(&mut self, n: i64) {
        let t = self.grid.beat_time(n);
        let half = ((0.15 * self.grid.period) / self.cfg.hop_dt).round() as i64;
        let c = self.hop_of_time(t);
        let mut e = 0.0f32;
        for h in (c - half)..=(c + half) {
            if let Some(s) = self.slot(h) {
                e += self.kick[s];
            }
        }
        self.beat_kick.push_back((n, e));
        while self.beat_kick.len() > 16 {
            self.beat_kick.pop_front();
        }
        let bar = self.grid.bar_len as i64;
        if self.beat_kick.len() < 2 * bar as usize {
            return;
        }
        let mut sums = vec![0.0f32; bar as usize];
        for &(b, e) in &self.beat_kick {
            sums[(b.rem_euclid(bar)) as usize] += e;
        }
        let (best, best_v) = sums.iter().enumerate().fold((0, f32::MIN), |a, (i, &v)| if v > a.1 { (i, v) } else { a });
        let second = sums.iter().enumerate().filter(|(i, _)| *i != best).map(|(_, &v)| v).fold(0.0f32, f32::max);
        if best_v > 1.3 * second {
            self.grid.downbeat = best as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_pulse_train(bpm: f64, seconds: f64) -> (Tracker, Vec<f64>, Vec<f64>) {
        let cfg = TrackerConfig::default();
        let dt = cfg.hop_dt;
        let mut tr = Tracker::new(cfg);
        let period = 60.0 / bpm;
        let mut next_beat = 0.5;
        let mut truth = Vec::new();
        let mut emitted = Vec::new();
        let mut last_emitted = i64::MIN;
        let hops = (seconds / dt) as u64;
        for i in 0..hops {
            let t = (i + 1) as f64 * dt;
            let mut odf = 0.3;
            let mut onset = None;
            if t >= next_beat && t - dt < next_beat {
                odf = 8.0;
                onset = Some((next_beat, 8.0));
                truth.push(next_beat);
                next_beat += period;
            }
            tr.process_hop(HopInput { time: t, odf, kick: odf, onset });
            let g = tr.grid();
            if g.running {
                // emulate the clock thread: emit a beat when its predicted time passes
                let n = g.nearest_beat(t).0;
                if g.beat_time(n) <= t && n > last_emitted {
                    last_emitted = n;
                    emitted.push(g.beat_time(n));
                }
            }
        }
        (tr, truth, emitted)
    }

    #[test]
    fn locks_to_steady_pulse_train() {
        let (tr, truth, emitted) = run_pulse_train(120.0, 12.0);
        let g = tr.grid();
        assert!(g.running, "tracker did not start running");
        assert!((g.bpm() - 120.0).abs() < 1.0, "bpm {}", g.bpm());
        assert!(!emitted.is_empty());
        // every emitted beat after lock should be within 10 ms of a true beat
        let mut worst = 0.0f64;
        for e in emitted.iter().skip(2) {
            let err = truth.iter().map(|t| (t - e).abs()).fold(f64::MAX, f64::min);
            worst = worst.max(err);
        }
        assert!(worst < 0.010, "worst predicted-beat error {worst}s");
        let first_lock = emitted[0];
        assert!(first_lock < 5.0, "took {first_lock}s to lock");
    }

    #[test]
    fn follows_tempo_ramp() {
        let cfg = TrackerConfig::default();
        let dt = cfg.hop_dt;
        let mut tr = Tracker::new(cfg);
        let mut t_beat = 0.5;
        let mut bpm = 100.0;
        let hops = (30.0 / dt) as u64;
        let mut last_emitted = i64::MIN;
        let mut truth = Vec::new();
        let mut emitted = Vec::new();
        for i in 0..hops {
            let t = (i + 1) as f64 * dt;
            let mut odf = 0.3;
            let mut onset = None;
            if t >= t_beat && t - dt < t_beat {
                odf = 8.0;
                onset = Some((t_beat, 8.0));
                truth.push(t_beat);
                if t > 10.0 && bpm < 120.0 {
                    bpm += 1.0; // +1 BPM per beat: 100 -> 120 in ~11 s
                }
                t_beat += 60.0 / bpm;
            }
            tr.process_hop(HopInput { time: t, odf, kick: odf, onset });
            let g = tr.grid();
            if g.running && t > 25.0 {
                let n = g.nearest_beat(t).0;
                if g.beat_time(n) <= t && n > last_emitted {
                    last_emitted = n;
                    emitted.push(g.beat_time(n));
                }
            }
        }
        let g = tr.grid();
        assert!((g.bpm() - 120.0).abs() < 2.0, "bpm after ramp {}", g.bpm());
        let worst = emitted
            .iter()
            .map(|e| truth.iter().map(|x| (x - e).abs()).fold(f64::MAX, f64::min))
            .fold(0.0, f64::max);
        assert!(worst < 0.015, "worst error after ramp {worst}");
    }

    #[test]
    fn stops_after_silence() {
        let cfg = TrackerConfig::default();
        let dt = cfg.hop_dt;
        let stop_after = cfg.stop_after_s;
        let mut tr = Tracker::new(cfg);
        let mut next_beat = 0.5;
        let hops = (30.0 / dt) as u64;
        let mut was_running = false;
        for i in 0..hops {
            let t = (i + 1) as f64 * dt;
            let mut odf = 0.3;
            let mut onset = None;
            if t < 10.0 && t >= next_beat && t - dt < next_beat {
                odf = 8.0;
                onset = Some((next_beat, 8.0));
                next_beat += 0.5;
            }
            tr.process_hop(HopInput { time: t, odf, kick: odf, onset });
            was_running |= tr.grid().running;
            if t > 10.0 + stop_after + 1.0 {
                assert!(!tr.grid().running, "still running at {t}");
            }
        }
        assert!(was_running);
        assert_eq!(tr.grid().state, State::Stopped);
    }
}
