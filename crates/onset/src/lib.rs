//! Onset detection function (ODF) and causal peak picking.
//!
//! Every `hop` samples a frame of `frame_size` samples is windowed and
//! transformed. Three half-wave-rectified log-spectral-flux measures are
//! computed: the full band, a kick band and a snare/hat band. Each is
//! normalised by its own slow running mean (automatic gain), then weighted
//! and summed into one ODF value per hop. A causal peak picker reports an
//! onset one hop after the peak.
//!
//! The hot path allocates nothing after construction.

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct OnsetConfig {
    pub sample_rate: f32,
    pub frame_size: usize,
    pub hop: usize,
    pub kick_band_hz: (f32, f32),
    pub hi_band_hz: (f32, f32),
    pub w_flux: f32,
    pub w_kick: f32,
    pub w_hi: f32,
    /// Time constant of the running means used for normalisation (seconds).
    pub agc_tau_s: f32,
    /// An onset needs odf > threshold_ratio * running mean of the odf.
    pub threshold_ratio: f32,
    /// Minimum gap between two onsets (seconds).
    pub min_onset_gap_s: f32,
    /// Frames whose RMS is below this level (dBFS) produce no ODF at all.
    pub gate_dbfs: f32,
}

impl Default for OnsetConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000.0,
            frame_size: 1024,
            hop: 128,
            kick_band_hz: (40.0, 130.0),
            hi_band_hz: (1500.0, 8000.0),
            w_flux: 1.0,
            w_kick: 2.0,
            w_hi: 1.0,
            agc_tau_s: 2.0,
            threshold_ratio: 2.5,
            min_onset_gap_s: 0.05,
            gate_dbfs: -55.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Onset {
    /// Hop index at which the ODF peaked.
    pub hop_index: u64,
    /// Peak value divided by the running mean of the ODF (>= threshold_ratio).
    pub strength: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hop {
    /// 0-based hop index. Hop `i` covers samples `[i*hop, (i+1)*hop)`.
    pub index: u64,
    /// Number of samples processed so far (the sample after this hop's last one).
    pub end_sample: u64,
    /// Combined, normalised ODF value.
    pub odf: f32,
    /// Normalised kick-band ODF value.
    pub kick: f32,
    /// RMS of this hop's input in dBFS.
    pub rms_db: f32,
    /// An onset detected at the previous hop (`index - 1`), if any.
    pub onset: Option<Onset>,
}

pub struct OnsetDetector {
    cfg: OnsetConfig,
    ring: Vec<f32>,
    ring_pos: usize,
    since_hop: usize,
    total_samples: u64,
    hop_index: u64,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    in_buf: Vec<f32>,
    spec: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    prev_mag: Vec<f32>,
    mag: Vec<f32>,
    kick_bins: (usize, usize),
    hi_bins: (usize, usize),
    mean_flux: f32,
    mean_kick: f32,
    mean_hi: f32,
    mean_odf: f32,
    agc_alpha: f32,
    primed: bool,
    o1: f32,
    o2: f32,
    last_onset_hop: i64,
    min_gap_hops: i64,
    rms_acc: f32,
}

impl OnsetDetector {
    pub fn new(cfg: OnsetConfig) -> Self {
        let n = cfg.frame_size;
        assert!(n >= 2 * cfg.hop, "frame_size must be at least twice the hop");
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
            .collect();
        let bins = n / 2 + 1;
        let hz_per_bin = cfg.sample_rate / n as f32;
        let band = |lo: f32, hi: f32| {
            let a = ((lo / hz_per_bin).round() as usize).clamp(1, bins - 1);
            let b = ((hi / hz_per_bin).round() as usize).clamp(a + 1, bins);
            (a, b)
        };
        let kick_bins = band(cfg.kick_band_hz.0, cfg.kick_band_hz.1);
        let hi_bins = band(cfg.hi_band_hz.0, cfg.hi_band_hz.1);
        let hops_per_s = cfg.sample_rate / cfg.hop as f32;
        let agc_alpha = 1.0 - (-1.0 / (cfg.agc_tau_s * hops_per_s)).exp();
        let min_gap_hops = (cfg.min_onset_gap_s * hops_per_s).round() as i64;
        Self {
            in_buf: fft.make_input_vec(),
            spec: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            ring: vec![0.0; n],
            ring_pos: 0,
            since_hop: 0,
            total_samples: 0,
            hop_index: 0,
            window,
            prev_mag: vec![0.0; bins],
            mag: vec![0.0; bins],
            kick_bins,
            hi_bins,
            mean_flux: 0.0,
            mean_kick: 0.0,
            mean_hi: 0.0,
            mean_odf: 0.0,
            agc_alpha,
            primed: false,
            o1: 0.0,
            o2: 0.0,
            last_onset_hop: i64::MIN / 2,
            min_gap_hops,
            rms_acc: 0.0,
            cfg,
        }
    }

    pub fn config(&self) -> &OnsetConfig {
        &self.cfg
    }

    /// Seconds per hop.
    pub fn hop_dt(&self) -> f64 {
        self.cfg.hop as f64 / self.cfg.sample_rate as f64
    }

    /// Feed mono samples. `on_hop` is called once per completed hop.
    pub fn process(&mut self, samples: &[f32], mut on_hop: impl FnMut(Hop)) {
        for &s in samples {
            self.ring[self.ring_pos] = s;
            self.ring_pos += 1;
            if self.ring_pos == self.ring.len() {
                self.ring_pos = 0;
            }
            self.rms_acc += s * s;
            self.total_samples += 1;
            self.since_hop += 1;
            if self.since_hop == self.cfg.hop {
                self.since_hop = 0;
                let hop = self.compute_hop();
                on_hop(hop);
            }
        }
    }

    fn compute_hop(&mut self) -> Hop {
        let n = self.ring.len();
        // Oldest sample first, windowed.
        let (tail, head) = self.ring.split_at(self.ring_pos);
        let mut i = 0;
        for &s in head.iter().chain(tail.iter()) {
            self.in_buf[i] = s * self.window[i];
            i += 1;
        }
        // realfft returns an error only on length mismatch, which cannot happen here.
        let _ = self.fft.process_with_scratch(&mut self.in_buf, &mut self.spec, &mut self.scratch);
        let scale = 4.0 / n as f32;
        for (m, c) in self.mag.iter_mut().zip(self.spec.iter()) {
            *m = (1.0 + 100.0 * c.norm() * scale).ln();
        }
        let rms = (self.rms_acc / self.cfg.hop as f32).sqrt();
        self.rms_acc = 0.0;
        let rms_db = 20.0 * (rms + 1e-9).log10();

        let mut flux = 0.0f32;
        let mut kick = 0.0f32;
        let mut hi = 0.0f32;
        for k in 1..self.mag.len() {
            let d = self.mag[k] - self.prev_mag[k];
            if d > 0.0 {
                flux += d;
                if k >= self.kick_bins.0 && k < self.kick_bins.1 {
                    kick += d;
                }
                if k >= self.hi_bins.0 && k < self.hi_bins.1 {
                    hi += d;
                }
            }
        }
        std::mem::swap(&mut self.mag, &mut self.prev_mag);

        let gated = rms_db > self.cfg.gate_dbfs;
        let (odf, kick_n) = if gated {
            if !self.primed {
                self.mean_flux = flux.max(1e-3);
                self.mean_kick = kick.max(1e-3);
                self.mean_hi = hi.max(1e-3);
                self.primed = true;
            }
            let a = self.agc_alpha;
            self.mean_flux += a * (flux - self.mean_flux);
            self.mean_kick += a * (kick - self.mean_kick);
            self.mean_hi += a * (hi - self.mean_hi);
            let eps = 1e-3;
            let kick_n = kick / self.mean_kick.max(eps);
            let odf = self.cfg.w_flux * flux / self.mean_flux.max(eps)
                + self.cfg.w_kick * kick_n
                + self.cfg.w_hi * hi / self.mean_hi.max(eps);
            if self.mean_odf == 0.0 {
                self.mean_odf = odf.max(1e-3);
            }
            self.mean_odf += a * (odf - self.mean_odf);
            (odf, kick_n)
        } else {
            (0.0, 0.0)
        };

        // Causal peak picking on the previous hop.
        let mut onset = None;
        let prev_index = self.hop_index as i64 - 1;
        let thr = self.cfg.threshold_ratio * self.mean_odf;
        if prev_index >= 0
            && self.o1 > self.o2
            && self.o1 >= odf
            && self.o1 > thr
            && self.mean_odf > 0.0
            && prev_index - self.last_onset_hop >= self.min_gap_hops
        {
            self.last_onset_hop = prev_index;
            onset = Some(Onset { hop_index: prev_index as u64, strength: self.o1 / self.mean_odf });
        }
        self.o2 = self.o1;
        self.o1 = odf;

        let hop = Hop {
            index: self.hop_index,
            end_sample: self.total_samples,
            odf,
            kick: kick_n,
            rms_db,
            onset,
        };
        self.hop_index += 1;
        hop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click train at 120 BPM must produce exactly one onset per click,
    /// each within a few hops of the true position.
    #[test]
    fn detects_click_train() {
        let cfg = OnsetConfig::default();
        let fs = cfg.sample_rate as usize;
        let hop = cfg.hop;
        let mut det = OnsetDetector::new(cfg);
        let seconds = 8;
        let mut audio = vec![0.0f32; fs * seconds];
        // low-level noise floor so the gate opens
        let mut seed = 1u32;
        for s in audio.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *s = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002;
        }
        let period = fs / 2; // 120 BPM
        let mut truth = Vec::new();
        let mut t = fs / 4;
        while t + 2000 < audio.len() {
            for i in 0..2000 {
                let env = (-(i as f32) / 300.0).exp();
                audio[t + i] += 0.6 * env * (2.0 * std::f32::consts::PI * 180.0 * i as f32 / fs as f32).sin();
            }
            truth.push(t);
            t += period;
        }
        let mut onsets = Vec::new();
        det.process(&audio, |h| {
            if let Some(o) = h.onset {
                onsets.push(o.hop_index as usize * hop);
            }
        });
        // skip the first second where the AGC is settling
        let truth: Vec<_> = truth.into_iter().filter(|&t| t > fs).collect();
        let onsets: Vec<_> = onsets.into_iter().filter(|&t| t > fs).collect();
        assert_eq!(onsets.len(), truth.len(), "onsets {onsets:?} vs truth {truth:?}");
        for (o, t) in onsets.iter().zip(truth.iter()) {
            let err = *o as i64 - *t as i64;
            assert!(err.abs() < (4 * hop) as i64 + 1024, "onset at {o} for click at {t} (err {err} samples)");
        }
    }
}
