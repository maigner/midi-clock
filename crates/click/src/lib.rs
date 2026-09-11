//! Renders a synthetic click at the grid's beat times, sample-accurately,
//! `lead_s` early to compensate for latency that ALSA does not report.

use tracker::Grid;

#[derive(Clone, Debug)]
pub struct ClickConfig {
    pub sample_rate: f32,
    pub freq_hz: f32,
    pub accent_freq_hz: f32,
    pub duration_s: f32,
    pub gain: f32,
    pub accent_gain: f32,
    /// Emit the click this much earlier than the beat time (seconds).
    pub lead_s: f64,
}

impl Default for ClickConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000.0,
            freq_hz: 1000.0,
            accent_freq_hz: 1500.0,
            duration_s: 0.006,
            gain: 0.5,
            accent_gain: 0.8,
            lead_s: 0.0,
        }
    }
}

struct Voice {
    remaining: usize,
    phase: f32,
    inc: f32,
    amp: f32,
    decay: f32,
}

pub struct ClickSynth {
    cfg: ClickConfig,
    voice: Option<Voice>,
    last_beat: i64,
    epoch: u32,
    length: usize,
    pub clicks_rendered: u64,
}

impl ClickSynth {
    pub fn new(cfg: ClickConfig) -> Self {
        let length = (cfg.duration_s * cfg.sample_rate).round().max(8.0) as usize;
        Self { cfg, voice: None, last_beat: i64::MIN, epoch: u32::MAX, length, clicks_rendered: 0 }
    }

    pub fn config(&self) -> &ClickConfig {
        &self.cfg
    }

    fn trigger(&mut self, accent: bool) {
        let f = if accent { self.cfg.accent_freq_hz } else { self.cfg.freq_hz };
        let amp = if accent { self.cfg.accent_gain } else { self.cfg.gain };
        // Decay to ~5 % over the click length.
        let decay = (-3.0 / self.length as f32).exp();
        self.voice = Some(Voice {
            remaining: self.length,
            phase: 0.0,
            inc: 2.0 * std::f32::consts::PI * f / self.cfg.sample_rate,
            amp,
            decay,
        });
        self.clicks_rendered += 1;
    }

    /// Render a mono buffer whose first sample plays at `buf_start` (monotonic seconds).
    pub fn render(&mut self, grid: &Grid, buf_start: f64, mono: &mut [f32]) {
        mono.fill(0.0);
        if grid.epoch != self.epoch {
            self.epoch = grid.epoch;
            self.last_beat = i64::MIN;
        }
        let fs = self.cfg.sample_rate as f64;
        let frames = mono.len();
        let mut triggers: [(usize, bool); 4] = [(usize::MAX, false); 4];
        let mut nt = 0;
        if grid.running && grid.period > 0.01 {
            let buf_end = buf_start + frames as f64 / fs;
            let lead = self.cfg.lead_s;
            let mut n = grid.first_beat_at_or_after(buf_start + lead);
            // A beat whose predicted time jumped into the past (grid correction
            // between buffers) is rendered now rather than dropped, if it is recent.
            if self.last_beat != i64::MIN && n > self.last_beat + 1 {
                let missed = self.last_beat + 1;
                if buf_start + lead - grid.beat_time(missed) < 0.03 {
                    n = missed;
                }
            }
            while grid.beat_time(n) - lead < buf_end && nt < triggers.len() {
                if n > self.last_beat {
                    let idx = ((grid.beat_time(n) - lead - buf_start) * fs).round().max(0.0) as usize;
                    triggers[nt] = (idx.min(frames - 1), grid.is_downbeat(n));
                    nt += 1;
                    self.last_beat = n;
                }
                n += 1;
            }
        }
        let mut ti = 0;
        for (i, out) in mono.iter_mut().enumerate() {
            while ti < nt && triggers[ti].0 == i {
                self.trigger(triggers[ti].1);
                ti += 1;
            }
            if let Some(v) = &mut self.voice {
                *out = v.amp * v.phase.sin();
                v.phase += v.inc;
                v.amp *= v.decay;
                v.remaining -= 1;
                if v.remaining == 0 {
                    self.voice = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_lands_on_the_predicted_sample() {
        let cfg = ClickConfig { lead_s: 0.001, ..ClickConfig::default() };
        let fs = cfg.sample_rate as f64;
        let mut synth = ClickSynth::new(cfg);
        let grid = Grid { running: true, epoch: 1, t0: 100.0, n0: 0, period: 0.5, ..Grid::default() };
        let frames = 64;
        let mut buf = vec![0.0f32; frames];
        let mut t = 99.99;
        let mut first_nonzero: Option<f64> = None;
        let mut clicks = 0;
        while t < 101.3 {
            synth.render(&grid, t, &mut buf);
            if let Some(i) = buf.iter().position(|&v| v != 0.0) {
                let ts = t + i as f64 / fs;
                if first_nonzero.is_none() {
                    first_nonzero = Some(ts);
                }
            }
            t += frames as f64 / fs;
            clicks = synth.clicks_rendered;
        }
        let f = first_nonzero.expect("no click rendered");
        // first audible sample is one after the trigger (sine starts at 0)
        assert!((f - (100.0 - 0.001)).abs() < 2.0 / fs, "first click at {f}");
        assert_eq!(clicks, 3, "beats at 100.0, 100.5, 101.0");
    }
}
