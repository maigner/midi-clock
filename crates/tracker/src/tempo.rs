//! Tempo (period) and phase estimation from the ODF history.

pub struct Estimator {
    hop_dt: f64,
    min_lag: usize,
    max_lag: usize,
    x: Vec<f32>,
    y: Vec<f32>,
    acf: Vec<f32>,
    score: Vec<f64>,
}

const COMB_WEIGHTS: [f64; 4] = [1.0, 0.7, 0.5, 0.3];

impl Estimator {
    pub fn new(hist_len: usize, hop_dt: f64, bpm_min: f64, bpm_max: f64) -> Self {
        let min_lag = ((60.0 / bpm_max) / hop_dt).floor().max(2.0) as usize;
        let max_lag = ((60.0 / bpm_min) / hop_dt).ceil() as usize;
        let acf_len = (4 * max_lag + 8).min(hist_len);
        Self {
            hop_dt,
            min_lag,
            max_lag: max_lag.min(hist_len / 2),
            x: vec![0.0; hist_len],
            y: vec![0.0; hist_len],
            acf: vec![0.0; acf_len],
            score: vec![0.0; max_lag + 2],
        }
    }

    /// Copy the ring buffer into `x` oldest-first, remove the local mean
    /// (0.5 s moving average) and half-wave rectify into `y`.
    fn prepare(&mut self, odf: &[f32], hops: u64, hist_len: usize) -> usize {
        let n = (hops as usize).min(hist_len);
        let start = hops as usize - n;
        for i in 0..n {
            self.x[i] = odf[(start + i) % hist_len];
        }
        let m = ((0.5 / self.hop_dt) as usize).max(1);
        let mut acc = 0.0f32;
        for i in 0..n {
            acc += self.x[i];
            if i >= m {
                acc -= self.x[i - m];
            }
            let mean = acc / (i + 1).min(m) as f32;
            self.y[i] = (self.x[i] - mean).max(0.0);
        }
        n
    }

    /// Returns `(period_in_hops, strength)` where strength is the comb score
    /// normalised to 0..1 (without prior), or None if there is not enough data.
    #[allow(clippy::too_many_arguments)]
    pub fn estimate(
        &mut self,
        odf: &[f32],
        hops: u64,
        hist_len: usize,
        bpm_center: f64,
        width_octaves: f64,
        current: Option<f64>,
        hysteresis: f64,
    ) -> Option<(f64, f64)> {
        let n = self.prepare(odf, hops, hist_len);
        if n < 2 * self.max_lag.min(n / 2 + 1) || n < 4 * self.min_lag {
            return None;
        }
        let max_acf = self.acf.len().min(n / 2);
        if max_acf <= self.max_lag {
            return None;
        }
        let y = &self.y[..n];
        for lag in 0..max_acf {
            let mut s = 0.0f32;
            let mut i = 0;
            let (a, b) = (&y[..n - lag], &y[lag..]);
            while i < a.len() {
                s += a[i] * b[i];
                i += 1;
            }
            self.acf[lag] = s / (n - lag) as f32;
        }
        let norm = self.acf[0];
        if norm <= 1e-9 {
            return None;
        }
        let wsum: f64 = COMB_WEIGHTS.iter().sum();
        let mut best = self.min_lag;
        let mut best_s = f64::MIN;
        for lag in self.min_lag..=self.max_lag {
            let mut s = 0.0;
            for (k, w) in COMB_WEIGHTS.iter().enumerate() {
                let c = (k + 1) * lag;
                let r = k + 1;
                if c + r >= max_acf {
                    break;
                }
                let mut m = f32::MIN;
                for j in (c - r)..=(c + r) {
                    m = m.max(self.acf[j]);
                }
                s += w * (m / norm) as f64;
            }
            let bpm = 60.0 / (lag as f64 * self.hop_dt);
            let z = (bpm / bpm_center).log2() / width_octaves;
            let prior = (-0.5 * z * z).exp();
            let mut score = s / wsum * prior;
            if let Some(cur) = current {
                if (lag as f64 / cur - 1.0).abs() < 0.05 {
                    score *= 1.0 + hysteresis;
                }
            }
            self.score[lag] = score;
            if score > best_s {
                best_s = score;
                best = lag;
            }
        }
        // Parabolic refinement of the lag.
        let mut refined = best as f64;
        if best > self.min_lag && best < self.max_lag {
            let (a, b, c) = (self.score[best - 1], self.score[best], self.score[best + 1]);
            let d = a - 2.0 * b + c;
            if d.abs() > 1e-12 {
                refined += 0.5 * (a - c) / d;
            }
        }
        // Strength without prior/hysteresis, so thresholds mean the same everywhere.
        let mut raw = 0.0;
        for (k, w) in COMB_WEIGHTS.iter().enumerate() {
            let c = (k + 1) * best;
            let r = k + 1;
            if c + r >= max_acf {
                break;
            }
            let mut m = f32::MIN;
            for j in (c - r)..=(c + r) {
                m = m.max(self.acf[j]);
            }
            raw += w * (m / norm) as f64;
        }
        Some((refined, raw / wsum))
    }

    /// Given a period in hops, find how many hops ago the last beat fell.
    /// Cross-correlates the last four beats of ODF with a pulse train.
    pub fn phase(&mut self, odf: &[f32], hops: u64, hist_len: usize, period: f64) -> Option<f64> {
        let n = self.prepare(odf, hops, hist_len);
        let p = period.round() as usize;
        if p < 2 || n < 2 * p {
            return None;
        }
        let beats = (n / p).min(4);
        let end = n as i64 - 1;
        let mut best = 0usize;
        let mut best_s = f32::MIN;
        let mut scores = vec![0.0f32; p];
        for phi in 0..p {
            let mut s = 0.0;
            for j in 0..beats {
                let c = end - phi as i64 - (j as f64 * period).round() as i64;
                if c < 1 {
                    break;
                }
                let mut m = f32::MIN;
                for k in (c - 1)..=(c + 1).min(end) {
                    m = m.max(self.y[k as usize]);
                }
                s += m;
            }
            scores[phi] = s;
            if s > best_s {
                best_s = s;
                best = phi;
            }
        }
        if best_s <= 0.0 {
            return None;
        }
        let a = scores[(best + p - 1) % p];
        let b = scores[best];
        let c = scores[(best + 1) % p];
        let d = a - 2.0 * b + c;
        let mut refined = best as f64;
        if d.abs() > 1e-9 {
            refined += 0.5 * ((a - c) / d) as f64;
        }
        Some(refined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_period_of_pulse_train() {
        let hop_dt = 128.0 / 48000.0;
        let hist = (6.0 / hop_dt) as usize;
        let mut est = Estimator::new(hist, hop_dt, 60.0, 200.0);
        let period_hops = (0.5 / hop_dt) as usize; // 120 BPM
        let mut odf = vec![0.0f32; hist];
        for i in 0..hist {
            odf[i] = if i % period_hops == 3 { 10.0 } else { 0.2 };
            if i % (period_hops / 2) == 3 {
                odf[i] += 3.0; // off-beat eighths
            }
        }
        let (p, strength) = est.estimate(&odf, hist as u64, hist, 110.0, 0.8, None, 0.0).unwrap();
        let bpm = 60.0 / (p * hop_dt);
        assert!((bpm - 120.0).abs() < 1.0, "bpm {bpm}");
        assert!(strength > 0.3, "strength {strength}");
        let phi = est.phase(&odf, hist as u64, hist, p).unwrap();
        let last_beat_hop = hist as f64 - 1.0 - phi;
        let expected = ((hist - 1 - 3) / period_hops * period_hops + 3) as f64;
        assert!((last_beat_hop - expected).abs() < 1.5, "phase {last_beat_hop} vs {expected}");
    }
}
