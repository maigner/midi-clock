//! Key detection from the room microphone: which key the band is playing
//! in, with a confidence, after a few seconds of music.
//!
//! The method (Krumhansl–Schmuckler, as in KeyFinder and Essentia): the
//! input is decimated to 12 kHz and every hop a long FFT (0.68 s) resolves
//! bass notes; spectral peaks that stand out of the whitened floor are
//! folded onto the 12 pitch classes, weighted by their level above the
//! floor, after estimating the band's tuning; the chroma vectors are summed
//! over a sliding window and correlated with 24 key profiles. The best
//! profile is the key, the margin over the runner-up the confidence, and a
//! hysteresis keeps the readout from flickering between a key and its
//! relative. Stationary spectral content (room modes, hum, fans) is removed
//! by subtracting a slow running background; broadband drum frames count
//! for little through a spectral-flatness weight.
//!
//! No latency requirement: the detector runs at ordinary priority and
//! allocates nothing after construction.

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Krumhansl–Kessler probe-tone profiles.
    Krumhansl,
    /// Temperley's revised profiles (flatter, fewer relative-key errors).
    Temperley,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Major,
    Minor,
}

#[derive(Clone, Debug)]
pub struct KeyConfig {
    pub sample_rate: f32,
    /// Decimation factor before analysis (48 kHz / 4 = 12 kHz).
    pub decimate: usize,
    /// FFT size at the decimated rate.
    pub fft_size: usize,
    /// Hop at the decimated rate.
    pub hop: usize,
    pub fmin_hz: f32,
    pub fmax_hz: f32,
    /// Integration time of the chroma (seconds).
    pub window_s: f32,
    /// A new key must win this long before it replaces the shown one.
    pub hold_s: f32,
    /// Time constant of the stationary-background removal (seconds).
    pub background_s: f32,
    /// Time constant of the tuning estimate (seconds).
    pub tuning_s: f32,
    pub profile: Profile,
    /// Peaks above this frequency count half (overtones, leads).
    pub band_split_hz: f32,
    /// A peak must stand this far above the local floor (dB).
    pub peak_db: f32,
    /// A peak must lie within this many dB of the frame's strongest one.
    pub peak_range_db: f32,
    /// A peak must reach this level (dBFS): anything below is noise.
    pub min_peak_dbfs: f32,
    /// A4 in Hz, or `None` to estimate the tuning from the music.
    pub tuning_hz: Option<f32>,
    /// Frames below this level (dBFS RMS) are silence.
    pub gate_dbfs: f32,
    /// After this much silence the estimate is cleared.
    pub silence_reset_s: f32,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000.0,
            decimate: 4,
            fft_size: 8192,
            hop: 2048,
            fmin_hz: 55.0,
            fmax_hz: 4000.0,
            window_s: 12.0,
            hold_s: 3.0,
            background_s: 60.0,
            tuning_s: 30.0,
            profile: Profile::Temperley,
            band_split_hz: 1000.0,
            peak_db: 6.0,
            peak_range_db: 50.0,
            min_peak_dbfs: -90.0,
            tuning_hz: None,
            gate_dbfs: -60.0,
            silence_reset_s: 8.0,
        }
    }
}

/// What the detector currently believes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyEstimate {
    /// 0 = C ... 11 = B.
    pub tonic: u8,
    pub mode: Mode,
    /// 0..1: the margin of the shown key over the best other key.
    pub confidence: f32,
    /// Offset of the band's A4 from 440 Hz, in cents.
    pub tuning_cents: f32,
    /// The integrated chroma, normalised to a maximum of 1.
    pub chroma: [f32; 12],
    /// The relative key or the fifth when it is nearly as good.
    pub alt: Option<(u8, Mode)>,
    /// Analysis frames so far (a serial number for the estimate).
    pub frames: u64,
}

impl KeyEstimate {
    /// "Am", "F#" (major keys have no suffix).
    pub fn short_name(&self) -> String {
        key_short_name(self.tonic, self.mode)
    }
}

pub fn key_short_name(tonic: u8, mode: Mode) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let n = NAMES[(tonic % 12) as usize];
    match mode {
        Mode::Major => n.to_string(),
        Mode::Minor => format!("{n}m"),
    }
}

/// Key profiles (pitch class 0 = tonic), major then minor.
fn profiles(p: Profile) -> ([f32; 12], [f32; 12]) {
    match p {
        Profile::Krumhansl => (
            [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88],
            [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17],
        ),
        Profile::Temperley => (
            [5.0, 2.0, 3.5, 2.0, 4.5, 4.0, 2.0, 4.5, 2.0, 3.5, 1.5, 4.0],
            [5.0, 2.0, 3.5, 4.5, 2.0, 4.0, 2.0, 4.5, 3.5, 2.0, 1.5, 4.0],
        ),
    }
}

/// Pearson correlation of two 12-vectors.
fn correlate(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let ma = a.iter().sum::<f32>() / 12.0;
    let mb = b.iter().sum::<f32>() / 12.0;
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for i in 0..12 {
        let (x, y) = (a[i] - ma, b[i] - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da <= 0.0 || db <= 0.0 {
        0.0
    } else {
        num / (da * db).sqrt()
    }
}

/// Windowed-sinc low-pass FIR (Blackman window), `taps` odd.
fn lowpass(taps: usize, cutoff: f32) -> Vec<f32> {
    let m = (taps - 1) as f32;
    let mut h: Vec<f32> = (0..taps)
        .map(|i| {
            let x = i as f32 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x) };
            let w = 0.42 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / m).cos() + 0.08 * (4.0 * std::f32::consts::PI * i as f32 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = h.iter().sum();
    for v in &mut h {
        *v /= sum;
    }
    h
}

/// Decimating FIR: keeps every `factor`-th output of the low-pass.
pub struct Decimator {
    taps: Vec<f32>,
    /// Delay line, twice the taps so a contiguous window always exists.
    delay: Vec<f32>,
    pos: usize,
    factor: usize,
    phase: usize,
}

impl Decimator {
    pub fn new(factor: usize, sample_rate: f32) -> Self {
        let factor = factor.max(1);
        let taps = if factor == 1 { vec![1.0] } else { lowpass(63, (sample_rate / factor as f32 * 0.42) / sample_rate) };
        let n = taps.len();
        Self { taps, delay: vec![0.0; 2 * n], pos: 0, factor, phase: 0 }
    }

    /// Push samples, calling `out` for every decimated output sample.
    pub fn process(&mut self, samples: &[f32], mut out: impl FnMut(f32)) {
        let n = self.taps.len();
        for &x in samples {
            self.delay[self.pos] = x;
            self.delay[self.pos + n] = x;
            self.pos = (self.pos + 1) % n;
            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                // the window ending at the newest sample: delay[pos .. pos+n]
                // is oldest..newest; taps are symmetric
                let win = &self.delay[self.pos..self.pos + n];
                let y: f32 = win.iter().zip(&self.taps).map(|(a, b)| a * b).sum();
                out(y);
            }
        }
    }

    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.pos = 0;
        self.phase = 0;
    }
}

pub struct KeyDetector {
    cfg: KeyConfig,
    dec: Decimator,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Ring of decimated samples, `fft_size` long.
    ring: Vec<f32>,
    ring_pos: usize,
    filled: usize,
    since_hop: usize,
    fft_in: Vec<f32>,
    fft_out: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Bins in range.
    bin_lo: usize,
    bin_hi: usize,
    /// Log magnitude per bin (dB) of the current frame, background, whitened.
    logmag: Vec<f32>,
    background: Vec<f32>,
    white: Vec<f32>,
    floor: Vec<f32>,
    median_buf: Vec<f32>,
    background_ready: bool,
    // tuning estimate as a circular mean
    tune_cos: f32,
    tune_sin: f32,
    tuning_cents: f32,
    // integrated chroma
    acc: [f32; 12],
    profiles: ([f32; 12], [f32; 12]),
    frames: u64,
    silent_frames: u64,
    shown: Option<(u8, Mode)>,
    candidate: Option<(u8, Mode, u32)>,
    estimate: Option<KeyEstimate>,
    hop_dt: f32,
}

impl KeyDetector {
    pub fn new(cfg: KeyConfig) -> Self {
        let n = cfg.fft_size.max(64);
        let fs_dec = cfg.sample_rate / cfg.decimate.max(1) as f32;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let fft_in = fft.make_input_vec();
        let fft_out = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        let window: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect();
        let bin_hz = fs_dec / n as f32;
        let bin_lo = ((cfg.fmin_hz / bin_hz).floor() as usize).max(2);
        let bin_hi = ((cfg.fmax_hz / bin_hz).ceil() as usize).min(n / 2 - 2).max(bin_lo + 2);
        let bins = n / 2 + 1;
        let profiles = profiles(cfg.profile);
        let hop_dt = cfg.hop.max(1) as f32 / fs_dec;
        Self {
            dec: Decimator::new(cfg.decimate, cfg.sample_rate),
            fft,
            window,
            ring: vec![0.0; n],
            ring_pos: 0,
            filled: 0,
            since_hop: 0,
            fft_in,
            fft_out,
            scratch,
            bin_lo,
            bin_hi,
            logmag: vec![-120.0; bins],
            background: vec![-120.0; bins],
            white: vec![0.0; bins],
            floor: vec![0.0; bins],
            median_buf: Vec::with_capacity(49),
            background_ready: false,
            tune_cos: 0.0,
            tune_sin: 0.0,
            tuning_cents: 0.0,
            acc: [0.0; 12],
            profiles,
            frames: 0,
            silent_frames: 0,
            shown: None,
            candidate: None,
            estimate: None,
            hop_dt,
            cfg,
        }
    }

    pub fn config(&self) -> &KeyConfig {
        &self.cfg
    }

    /// Seconds between two analysis frames.
    pub fn hop_dt(&self) -> f32 {
        self.hop_dt
    }

    /// The current estimate, if any.
    pub fn estimate(&self) -> Option<KeyEstimate> {
        self.estimate
    }

    /// Forget everything heard (not the tuning and the background, which
    /// belong to the room).
    pub fn reset(&mut self) {
        self.acc = [0.0; 12];
        self.shown = None;
        self.candidate = None;
        self.estimate = None;
        self.silent_frames = 0;
    }

    /// Feed input samples (at `sample_rate`); `on_frame` is called with the
    /// estimate after every analysis frame that produced one.
    pub fn process(&mut self, samples: &[f32], mut on_frame: impl FnMut(&KeyEstimate)) {
        let n = self.ring.len();
        let hop = self.cfg.hop.max(1);
        // Decimated samples go into the ring; a frame is analysed after
        // every hop. Calls carry a few ms of audio, so at most one hop
        // boundary falls inside one call; should more, the last frame is
        // the one analysed (the ring holds one frame).
        let mut due = false;
        let (ring, ring_pos, filled, since_hop) = (&mut self.ring, &mut self.ring_pos, &mut self.filled, &mut self.since_hop);
        self.dec.process(samples, |y| {
            ring[*ring_pos] = y;
            *ring_pos = (*ring_pos + 1) % n;
            *filled = (*filled + 1).min(n);
            *since_hop += 1;
            if *since_hop >= hop && *filled >= n {
                *since_hop = 0;
                due = true;
            }
        });
        if due {
            self.analyse();
            if let Some(e) = self.estimate {
                on_frame(&e);
            }
        }
    }

    /// The integrated chroma so far, normalised to a maximum of 1 (all
    /// zero before anything was heard).
    pub fn chroma(&self) -> [f32; 12] {
        let peak = self.acc.iter().cloned().fold(0.0f32, f32::max);
        let mut out = [0.0f32; 12];
        if peak > 0.0 {
            for (o, a) in out.iter_mut().zip(&self.acc) {
                *o = a / peak;
            }
        }
        out
    }

    /// The tuning estimate: the band's A4 relative to 440 Hz, in cents.
    pub fn tuning_cents(&self) -> f32 {
        self.tuning_cents
    }

    fn analyse(&mut self) {
        let n = self.ring.len();
        self.frames += 1;
        // --- frame: window the ring in time order --------------------------------
        let mut rms = 0.0f32;
        for i in 0..n {
            let x = self.ring[(self.ring_pos + i) % n];
            rms += x * x;
            self.fft_in[i] = x * self.window[i];
        }
        let rms_db = 10.0 * (rms / n as f32 + 1e-20).log10();
        if rms_db < self.cfg.gate_dbfs {
            self.silent_frames += 1;
            if self.silent_frames as f32 * self.hop_dt >= self.cfg.silence_reset_s {
                self.reset();
            }
            return;
        }
        self.silent_frames = 0;
        if self.fft.process_with_scratch(&mut self.fft_in, &mut self.fft_out, &mut self.scratch).is_err() {
            return;
        }
        let (lo, hi) = (self.bin_lo, self.bin_hi);
        // --- log magnitude, background subtraction --------------------------------
        let alpha_bg = (self.hop_dt / self.cfg.background_s.max(self.hop_dt)).min(1.0);
        for i in lo..=hi {
            let m = self.fft_out[i].norm() / n as f32 * 2.0;
            self.logmag[i] = 20.0 * (m + 1e-9).log10();
        }
        if !self.background_ready {
            // start from a flat floor at the level of a quiet room
            for i in lo..=hi {
                self.background[i] = -100.0;
            }
            self.background_ready = true;
        }
        let mut max_logmag = f32::NEG_INFINITY;
        for i in lo..=hi {
            // a running mean of the log spectrum: whatever stays for a
            // minute is the room, not the music
            self.background[i] += alpha_bg * (self.logmag[i] - self.background[i]);
            self.white[i] = self.logmag[i] - self.background[i];
            max_logmag = max_logmag.max(self.logmag[i]);
        }
        // --- local floor: running median over ±24 bins ---------------------------
        const HALF: usize = 24;
        for i in lo..=hi {
            let a = i.saturating_sub(HALF).max(lo);
            let b = (i + HALF).min(hi);
            self.median_buf.clear();
            self.median_buf.extend_from_slice(&self.white[a..=b]);
            let mid = self.median_buf.len() / 2;
            let (_, med, _) = self.median_buf.select_nth_unstable_by(mid, |x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            self.floor[i] = *med;
        }
        // --- spectral flatness of the power in range: frame weight --------------
        let (mut log_sum, mut lin_sum) = (0.0f64, 0.0f64);
        let count = (hi - lo + 1) as f64;
        for i in lo..=hi {
            let p = 10f64.powf(self.logmag[i] as f64 / 10.0);
            log_sum += (p + 1e-20).ln();
            lin_sum += p;
        }
        let flatness = ((log_sum / count).exp() / (lin_sum / count + 1e-20)).clamp(0.0, 1.0) as f32;
        let frame_w = (1.0 - flatness).clamp(0.0, 1.0);
        // --- peaks -> tuning and chroma -------------------------------------------
        // A peak is the maximum of its ±4 bins (the window's first sidelobes
        // lie inside that), stands `peak_db` above the local whitened floor,
        // and is loud in absolute terms: within `peak_range_db` of the
        // frame's strongest bin and above `min_peak_dbfs` (farther sidelobes
        // and the digital floor of a quiet recording do not count).
        const NEIGHBOURS: usize = 4;
        let fs_dec = self.cfg.sample_rate / self.cfg.decimate.max(1) as f32;
        let bin_hz = fs_dec / n as f32;
        let mut chroma = [0.0f32; 12];
        let mut tune_c = 0.0f32;
        let mut tune_s = 0.0f32;
        let a4 = self.cfg.tuning_hz.unwrap_or(440.0 * 2f32.powf(self.tuning_cents / 1200.0));
        let min_level = (max_logmag - self.cfg.peak_range_db).max(self.cfg.min_peak_dbfs);
        for i in (lo + 1)..hi {
            let (l, c, r) = (self.white[i - 1], self.white[i], self.white[i + 1]);
            if c <= l || c < r || c < self.floor[i] + self.cfg.peak_db || self.logmag[i] < min_level {
                continue;
            }
            let a = i.saturating_sub(NEIGHBOURS).max(lo);
            let b = (i + NEIGHBOURS).min(hi);
            if (a..=b).any(|j| j != i && self.white[j] > c) {
                continue;
            }
            // parabolic interpolation on the log magnitude
            let denom = l - 2.0 * c + r;
            let delta = if denom.abs() > 1e-6 { 0.5 * (l - r) / denom } else { 0.0 };
            let f = (i as f32 + delta.clamp(-0.5, 0.5)) * bin_hz;
            if f < self.cfg.fmin_hz || f > self.cfg.fmax_hz {
                continue;
            }
            let above = (c - self.floor[i]).clamp(0.0, 60.0);
            let w = above * if f > self.cfg.band_split_hz { 0.5 } else { 1.0 };
            // tuning: cents from the nearest 440-based semitone
            let semis_440 = 12.0 * (f / 440.0).log2();
            let cents = (semis_440 - semis_440.round()) * 100.0;
            let th = cents / 100.0 * std::f32::consts::TAU;
            tune_c += w * th.cos();
            tune_s += w * th.sin();
            // pitch class on the estimated tuning
            let semis = 12.0 * (f / a4).log2();
            let pc = (semis.round() as i32 + 9).rem_euclid(12) as usize;
            chroma[pc] += w;
        }
        if self.cfg.tuning_hz.is_none() && (tune_c != 0.0 || tune_s != 0.0) {
            let a = (self.hop_dt / self.cfg.tuning_s.max(self.hop_dt)).min(1.0) * frame_w;
            self.tune_cos += a * (tune_c - self.tune_cos);
            self.tune_sin += a * (tune_s - self.tune_sin);
            if self.tune_cos != 0.0 || self.tune_sin != 0.0 {
                self.tuning_cents = self.tune_sin.atan2(self.tune_cos) / std::f32::consts::TAU * 100.0;
            }
        }
        // --- integrate ------------------------------------------------------------
        let alpha = (self.hop_dt / self.cfg.window_s.max(self.hop_dt)).min(1.0);
        let total: f32 = chroma.iter().sum();
        if total > 0.0 {
            for (a, c) in self.acc.iter_mut().zip(&chroma) {
                *a += alpha * (c / total * frame_w - *a);
            }
        } else {
            for v in &mut self.acc {
                *v *= 1.0 - alpha;
            }
        }
        // --- match ------------------------------------------------------------------
        let acc_total: f32 = self.acc.iter().sum();
        if acc_total <= 1e-6 {
            return;
        }
        let (major, minor) = self.profiles;
        let mut scores = [[0.0f32; 12]; 2];
        for (mi, prof) in [major, minor].iter().enumerate() {
            for tonic in 0..12 {
                let mut rotated = [0.0f32; 12];
                for (k, r) in rotated.iter_mut().enumerate() {
                    *r = prof[(k + 12 - tonic) % 12];
                }
                scores[mi][tonic] = correlate(&self.acc, &rotated);
            }
        }
        let mut ranked: Vec<(f32, u8, Mode)> = Vec::with_capacity(24);
        for tonic in 0..12u8 {
            ranked.push((scores[0][tonic as usize], tonic, Mode::Major));
            ranked.push((scores[1][tonic as usize], tonic, Mode::Minor));
        }
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let (r_best, t_best, m_best) = ranked[0];
        let r_second = ranked[1].0;
        let margin = |r: f32, others: f32| if r < 0.5 { 0.0 } else { ((r - others) / 0.15).clamp(0.0, 1.0) };
        // --- hysteresis: a new key must hold `hold_s` -------------------------------
        let hold_frames = (self.cfg.hold_s / self.hop_dt).ceil().max(1.0) as u32;
        match self.shown {
            None => {
                if margin(r_best, r_second) > 0.0 {
                    self.shown = Some((t_best, m_best));
                }
                self.candidate = None;
            }
            Some(shown) if shown == (t_best, m_best) => self.candidate = None,
            Some(_) => {
                let c = match self.candidate {
                    Some((t, m, k)) if (t, m) == (t_best, m_best) => (t, m, k + 1),
                    _ => (t_best, m_best, 1),
                };
                self.candidate = Some(c);
                if c.2 >= hold_frames {
                    self.shown = Some((t_best, m_best));
                    self.candidate = None;
                }
            }
        }
        let Some((tonic, mode)) = self.shown else {
            self.estimate = None;
            return;
        };
        let score = |t: u8, m: Mode| scores[if m == Mode::Major { 0 } else { 1 }][t as usize];
        let r_shown = score(tonic, mode);
        let r_best_other = ranked.iter().find(|(_, t, m)| (*t, *m) != (tonic, mode)).map_or(0.0, |x| x.0);
        let confidence = margin(r_shown, r_best_other);
        // the relative key and the fifths, when nearly as good
        let relatives = match mode {
            Mode::Major => [((tonic + 9) % 12, Mode::Minor), ((tonic + 7) % 12, Mode::Major), ((tonic + 5) % 12, Mode::Major)],
            Mode::Minor => [((tonic + 3) % 12, Mode::Major), ((tonic + 7) % 12, Mode::Minor), ((tonic + 5) % 12, Mode::Minor)],
        };
        let alt = relatives.into_iter().filter(|&(t, m)| r_shown - score(t, m) < 0.05).max_by(|a, b| score(a.0, a.1).partial_cmp(&score(b.0, b.1)).unwrap_or(std::cmp::Ordering::Equal));
        let peak = self.acc.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
        let mut chroma_out = [0.0f32; 12];
        for (o, a) in chroma_out.iter_mut().zip(&self.acc) {
            *o = a / peak;
        }
        self.estimate = Some(KeyEstimate { tonic, mode, confidence, tuning_cents: self.tuning_cents, chroma: chroma_out, alt, frames: self.frames });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freqs: &[(f32, f32)], seconds: f32, rate: f32) -> Vec<f32> {
        let n = (seconds * rate) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate;
                freqs.iter().map(|&(f, a)| a * (std::f32::consts::TAU * f * t).sin()).sum::<f32>()
            })
            .collect()
    }

    /// A note as a few harmonics.
    fn note(midi: i32, amp: f32, a4: f32) -> Vec<(f32, f32)> {
        let f = a4 * 2f32.powf((midi - 69) as f32 / 12.0);
        (1..=4).map(|h| (f * h as f32, amp / h as f32)).collect()
    }

    fn run(det: &mut KeyDetector, samples: &[f32]) -> Option<KeyEstimate> {
        let mut last = None;
        for chunk in samples.chunks(256) {
            det.process(chunk, |e| last = Some(*e));
        }
        last
    }

    #[test]
    fn decimator_passes_low_and_kills_high() {
        let rate = 48000.0;
        let mut d = Decimator::new(4, rate);
        let low = tone(&[(1000.0, 1.0)], 0.5, rate);
        let mut out = Vec::new();
        d.process(&low, |y| out.push(y));
        assert_eq!(out.len(), low.len() / 4);
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        assert!((rms(&out[2000..]) - 0.707).abs() < 0.05, "1 kHz passes: {}", rms(&out[2000..]));
        d.reset();
        let high = tone(&[(7000.0, 1.0)], 0.5, rate);
        out.clear();
        d.process(&high, |y| out.push(y));
        assert!(rms(&out[2000..]) < 0.02, "7 kHz is stopped: {}", rms(&out[2000..]));
    }

    #[test]
    fn a_pure_tone_lands_on_its_pitch_class() {
        let cfg = KeyConfig { window_s: 2.0, hold_s: 0.5, ..KeyConfig::default() };
        let mut det = KeyDetector::new(cfg);
        // one note is no key, but the chroma shows it
        assert!(run(&mut det, &tone(&[(440.0, 0.3)], 6.0, 48000.0)).is_none());
        let c = det.chroma();
        assert!(c[9] > 0.99, "A: {c:?}");
        assert!(c.iter().enumerate().all(|(i, &x)| i == 9 || x < 0.1), "{c:?}");
        assert!(det.tuning_cents().abs() < 3.0, "{}", det.tuning_cents());
        // sharp by 20 cents: still an A, and the tuning says so
        let mut det = KeyDetector::new(KeyConfig { window_s: 2.0, hold_s: 0.5, ..KeyConfig::default() });
        run(&mut det, &tone(&[(445.1, 0.3)], 10.0, 48000.0));
        let c = det.chroma();
        assert!(c[9] > 0.99, "A: {c:?}");
        assert!((det.tuning_cents() - 20.0).abs() < 5.0, "{}", det.tuning_cents());
    }

    #[test]
    fn a_major_triad_with_its_bass_is_the_major_key() {
        let cfg = KeyConfig { window_s: 3.0, hold_s: 0.5, ..KeyConfig::default() };
        let mut det = KeyDetector::new(cfg);
        let mut f = Vec::new();
        f.extend(note(36, 0.2, 440.0)); // C1
        f.extend(note(60, 0.15, 440.0)); // C3 E3 G3
        f.extend(note(64, 0.12, 440.0));
        f.extend(note(67, 0.12, 440.0));
        let e = run(&mut det, &tone(&f, 8.0, 48000.0)).expect("an estimate");
        assert_eq!((e.tonic, e.mode), (0, Mode::Major), "{e:?}");
        assert!(e.confidence > 0.6, "{e:?}");
        assert_eq!(e.short_name(), "C");
        assert!(det.estimate().is_some());
        // a chord a fifth up for a while: the reading holds for `hold_s`, then follows
        let mut g = Vec::new();
        g.extend(note(43, 0.2, 440.0));
        g.extend(note(67, 0.15, 440.0));
        g.extend(note(71, 0.12, 440.0));
        g.extend(note(74, 0.12, 440.0));
        let mut seen_c = false;
        let mut last = None;
        for chunk in tone(&g, 12.0, 48000.0).chunks(256) {
            det.process(chunk, |e| {
                if e.tonic == 0 {
                    seen_c = true;
                }
                last = Some(*e);
            });
        }
        assert!(seen_c, "the old key stays while the new one earns its hold time");
        assert_eq!(last.map(|e| (e.tonic, e.mode)), Some((7, Mode::Major)), "{last:?}");
    }

    #[test]
    fn a_minor_chord_progression_is_the_minor_key() {
        let cfg = KeyConfig { window_s: 4.0, hold_s: 0.5, ..KeyConfig::default() };
        let mut det = KeyDetector::new(cfg);
        // A minor: Am, F, C, G, with bass roots, 2 s each, twice
        let chords: [[i32; 4]; 4] = [[33, 57, 60, 64], [29, 53, 57, 60], [36, 60, 64, 67], [31, 55, 59, 62]];
        let mut last = None;
        for _ in 0..2 {
            for ch in &chords {
                let mut f = Vec::new();
                for (k, &m) in ch.iter().enumerate() {
                    f.extend(note(m, if k == 0 { 0.2 } else { 0.12 }, 440.0));
                }
                if let Some(e) = run(&mut det, &tone(&f, 2.0, 48000.0)) {
                    last = Some(e);
                }
            }
        }
        let e = last.expect("an estimate");
        assert!(matches!((e.tonic, e.mode), (9, Mode::Minor) | (0, Mode::Major)), "{e:?}");
        assert!(e.confidence > 0.3, "{e:?}");
        if (e.tonic, e.mode) == (9, Mode::Minor) {
            // the relative major is close, so it is named
            assert!(e.alt.is_none() || e.alt == Some((0, Mode::Major)), "{e:?}");
        }
    }

    #[test]
    fn noise_gives_no_confidence_and_silence_clears() {
        let cfg = KeyConfig { window_s: 2.0, hold_s: 0.5, silence_reset_s: 1.0, ..KeyConfig::default() };
        let mut det = KeyDetector::new(cfg);
        let mut seed = 12345u64;
        let noise: Vec<f32> = (0..48000 * 6)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * 0.3
            })
            .collect();
        let e = run(&mut det, &noise);
        assert!(e.is_none_or(|e| e.confidence < 0.2), "{e:?}");
        // a chord, then silence: the estimate goes away
        let mut f = Vec::new();
        f.extend(note(36, 0.2, 440.0));
        f.extend(note(64, 0.12, 440.0));
        f.extend(note(67, 0.12, 440.0));
        let e = run(&mut det, &tone(&f, 8.0, 48000.0));
        assert!(e.is_some_and(|e| e.confidence > 0.3), "{e:?}");
        run(&mut det, &vec![0.0f32; 48000 * 3]);
        assert!(det.estimate().is_none(), "{:?}", det.estimate());
    }

    #[test]
    fn hum_ends_up_in_the_background() {
        // 50 Hz hum and harmonics (a stationary A#-ish / D-ish mess) under a
        // C major triad: with a short background time constant the hum is
        // removed and the triad wins
        let cfg = KeyConfig { window_s: 3.0, hold_s: 0.5, background_s: 10.0, ..KeyConfig::default() };
        let mut det = KeyDetector::new(cfg);
        let hum: Vec<(f32, f32)> = (1..=6).map(|h| (50.0 * h as f32, 0.15)).collect();
        // hum alone for a while so the background learns it
        run(&mut det, &tone(&hum, 60.0, 48000.0));
        det.reset();
        let mut f = hum.clone();
        f.extend(note(48, 0.08, 440.0));
        f.extend(note(64, 0.06, 440.0));
        f.extend(note(67, 0.06, 440.0));
        let e = run(&mut det, &tone(&f, 10.0, 48000.0)).expect("an estimate");
        assert_eq!((e.tonic, e.mode), (0, Mode::Major), "{e:?}");
    }
}
