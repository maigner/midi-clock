//! eval: run the onset detector and tracker on a WAV file exactly as the app
//! would, simulate the clock thread, and score the *emitted* beats against
//! annotations. Also synthesises test material.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracker::{HopInput, Tracker, TrackerConfig};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Synthesise a drum-kit-like test signal with annotations.
    Synth {
        #[arg(long, default_value_t = 120.0)]
        bpm: f64,
        /// Ramp linearly to this BPM (0 = no ramp) starting at --ramp-start.
        #[arg(long, default_value_t = 0.0)]
        ramp_to: f64,
        #[arg(long, default_value_t = 10.0)]
        ramp_start: f64,
        #[arg(long, default_value_t = 10.0)]
        ramp_len: f64,
        #[arg(long, default_value_t = 30.0)]
        seconds: f64,
        /// White noise level (0..1) mixed in, e.g. room/bleed.
        #[arg(long, default_value_t = 0.01)]
        noise: f32,
        /// Silence from..to seconds, e.g. "15:18" (a break).
        #[arg(long)]
        gap: Option<String>,
        /// Random timing jitter per hit, standard deviation in ms (a human).
        #[arg(long, default_value_t = 4.0)]
        jitter_ms: f64,
        /// Pattern: rock (kick 1,3 snare 2,4 hats 8ths), four (kick on every beat), hats (hats only)
        #[arg(long, default_value = "rock")]
        pattern: String,
        #[arg(long, default_value_t = 48000)]
        rate: u32,
        #[arg(short, long)]
        out: PathBuf,
        /// Annotation file (one beat time in seconds per line). Default: <out>.beats
        #[arg(long)]
        ann: Option<PathBuf>,
    },
    /// Run the tracker on a WAV file and report.
    Run {
        wav: PathBuf,
        /// Annotation file with one beat time (seconds) per line.
        #[arg(long)]
        ann: Option<PathBuf>,
        #[arg(long, default_value_t = 7.0)]
        onset_offset_ms: f64,
        /// Tracker parameter overrides, e.g. --set kp=0.4 --set history_s=4
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        #[arg(long, default_value_t = 60.0)]
        bpm_min: f64,
        #[arg(long, default_value_t = 200.0)]
        bpm_max: f64,
        /// Write emitted beat times to this file.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Print every emitted beat.
        #[arg(short, long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    match Args::parse().cmd {
        Cmd::Synth { bpm, ramp_to, ramp_start, ramp_len, seconds, noise, gap, jitter_ms, pattern, rate, out, ann } => {
            synth(bpm, ramp_to, ramp_start, ramp_len, seconds, noise, gap, jitter_ms, &pattern, rate, &out, ann)
        }
        Cmd::Run { wav, ann, onset_offset_ms, set, bpm_min, bpm_max, dump, verbose } => {
            run(&wav, ann, onset_offset_ms, &set, bpm_min, bpm_max, dump, verbose)
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let u1 = self.next().max(1e-12);
        let u2 = self.next();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

#[allow(clippy::too_many_arguments)]
fn synth(
    bpm: f64,
    ramp_to: f64,
    ramp_start: f64,
    ramp_len: f64,
    seconds: f64,
    noise: f32,
    gap: Option<String>,
    jitter_ms: f64,
    pattern: &str,
    rate: u32,
    out: &PathBuf,
    ann: Option<PathBuf>,
) -> Result<()> {
    let fs = rate as f64;
    let n = (seconds * fs) as usize;
    let mut audio = vec![0f32; n];
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let gap: Option<(f64, f64)> = gap.map(|g| {
        let (a, b) = g.split_once(':').expect("gap as from:to");
        (a.parse().unwrap(), b.parse().unwrap())
    });
    let in_gap = |t: f64| gap.map(|(a, b)| t >= a && t < b).unwrap_or(false);
    let add = |audio: &mut [f32], t: f64, mut f: Box<dyn FnMut(usize) -> f32>, len: usize| {
        let s0 = (t * fs) as usize;
        for i in 0..len {
            if s0 + i < audio.len() {
                audio[s0 + i] += f(i);
            }
        }
    };
    let mut beats = Vec::new();
    let mut t = 0.5;
    let mut beat_idx = 0usize;
    while t < seconds - 0.1 {
        let cur_bpm = if ramp_to > 0.0 && t > ramp_start {
            bpm + (ramp_to - bpm) * ((t - ramp_start) / ramp_len).min(1.0)
        } else {
            bpm
        };
        let period = 60.0 / cur_bpm;
        beats.push(t);
        let jitter = |rng: &mut Rng| rng.gauss() * jitter_ms / 1000.0;
        for eighth in 0..2 {
            let te = t + eighth as f64 * period / 2.0 + jitter(&mut rng);
            if in_gap(te) || te < 0.0 {
                continue;
            }
            let kick = match pattern {
                "four" => eighth == 0,
                "hats" => false,
                _ => eighth == 0 && beat_idx % 2 == 0,
            };
            let snare = pattern == "rock" && eighth == 0 && beat_idx % 2 == 1;
            if kick {
                let len = (0.18 * fs) as usize;
                let mut ph = 0.0f64;
                add(
                    &mut audio,
                    te,
                    Box::new(move |i| {
                        let x = i as f64 / fs;
                        let f = 50.0 + 60.0 * (-x / 0.02).exp();
                        ph += 2.0 * std::f64::consts::PI * f / fs;
                        (0.8 * (-x / 0.06).exp() * ph.sin()) as f32
                    }),
                    len,
                );
            }
            if snare {
                let len = (0.15 * fs) as usize;
                let mut r = Rng(rng.0 ^ 0x1234);
                let mut prev = 0.0f32;
                add(
                    &mut audio,
                    te,
                    Box::new(move |i| {
                        let x = i as f64 / fs;
                        let w = (r.next() as f32 - 0.5) * 2.0;
                        let hp = w - 0.7 * prev;
                        prev = w;
                        let tone = (2.0 * std::f64::consts::PI * 190.0 * x).sin() as f32 * (-x as f32 / 0.05).exp();
                        0.5 * hp * (-x as f32 / 0.04).exp() + 0.3 * tone
                    }),
                    len,
                );
            }
            if pattern != "four" {
                let len = (0.025 * fs) as usize;
                let mut r = Rng(rng.0 ^ 0xABCD ^ (i64::from(eighth) as u64));
                let mut prev = 0.0f32;
                add(
                    &mut audio,
                    te,
                    Box::new(move |i| {
                        let x = i as f64 / fs;
                        let w = (r.next() as f32 - 0.5) * 2.0;
                        let hp = w - prev;
                        prev = w;
                        0.25 * hp * (-x as f32 / 0.008).exp()
                    }),
                    len,
                );
            }
            rng.0 = rng.0.wrapping_add(1);
        }
        t += period;
        beat_idx += 1;
    }
    if noise > 0.0 {
        for s in audio.iter_mut() {
            *s += (rng.next() as f32 - 0.5) * 2.0 * noise;
        }
    }
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(out, spec)?;
    for s in &audio {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    let ann = ann.unwrap_or_else(|| out.with_extension("beats"));
    let text: String = beats.iter().map(|b| format!("{b:.6}\n")).collect();
    std::fs::write(&ann, text)?;
    println!("wrote {} ({:.1} s, {} beats) and {}", out.display(), seconds, beats.len(), ann.display());
    Ok(())
}

fn read_wav(path: &PathBuf) -> Result<(Vec<f32>, u32)> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u32 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.unwrap() as f32 * scale).collect()
        }
    };
    let mono: Vec<f32> = samples.chunks(ch).map(|c| c[0]).collect();
    Ok((mono, spec.sample_rate))
}

fn apply_overrides(cfg: &mut TrackerConfig, set: &[String]) -> Result<()> {
    for kv in set {
        let (k, v) = kv.split_once('=').with_context(|| format!("--set {kv}: expected KEY=VALUE"))?;
        let f: f64 = v.parse().with_context(|| format!("--set {kv}: bad number"))?;
        match k {
            "kp" => cfg.kp = f,
            "ki" => cfg.ki = f,
            "period_blend" => cfg.period_blend = f,
            "history_s" => cfg.history_s = f,
            "estimate_interval_s" => cfg.estimate_interval_s = f,
            "accept_window" => cfg.accept_window = f,
            "max_phase_slew" => cfg.max_phase_slew = f,
            "tempo_hysteresis" => cfg.tempo_hysteresis = f,
            "min_tempo_strength" => cfg.min_tempo_strength = f,
            "full_weight_strength" => cfg.full_weight_strength = f,
            "bpm_center" => cfg.bpm_center = f,
            "bpm_width_octaves" => cfg.bpm_width_octaves = f,
            "coast_beats" => cfg.coast_beats = f as u32,
            "stop_after_s" => cfg.stop_after_s = f,
            "lock_estimates" => cfg.lock_estimates = f as usize,
            _ => anyhow::bail!("unknown tracker parameter {k}"),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    wav: &PathBuf,
    ann: Option<PathBuf>,
    onset_offset_ms: f64,
    set: &[String],
    bpm_min: f64,
    bpm_max: f64,
    dump: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    let (audio, rate) = read_wav(wav)?;
    let fs = rate as f64;
    let ocfg = onset::OnsetConfig { sample_rate: fs as f32, ..onset::OnsetConfig::default() };
    let hop = ocfg.hop;
    let hop_dt = hop as f64 / fs;
    let mut det = onset::OnsetDetector::new(ocfg);
    let mut tcfg = TrackerConfig { hop_dt, bpm_min, bpm_max, onset_offset_s: onset_offset_ms / 1000.0, ..TrackerConfig::default() };
    apply_overrides(&mut tcfg, set)?;
    let mut tr = Tracker::new(tcfg);
    let truth: Vec<f64> = match &ann {
        Some(p) => std::fs::read_to_string(p)?
            .lines()
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect(),
        None => Vec::new(),
    };

    let mut emitted: Vec<(f64, f64)> = Vec::new(); // (time, bpm at emission)
    let mut onsets_raw: Vec<f64> = Vec::new();
    let mut last_emitted = i64::MIN;
    let mut last_epoch = u32::MAX;
    let mut starts = Vec::new();
    let mut stops = Vec::new();
    let mut was_running = false;
    let chunk = 256;
    for block in audio.chunks(chunk) {
        det.process(block, |h| {
            let time = (h.end_sample) as f64 / fs;
            let onset = h.onset.map(|o| {
                let raw = (o.hop_index + 1) as f64 * hop_dt;
                onsets_raw.push(raw);
                (raw - onset_offset_ms / 1000.0, o.strength)
            });
            tr.process_hop(HopInput { time, odf: h.odf, kick: h.kick, onset });
            let g = tr.grid();
            if g.running != was_running {
                if g.running {
                    starts.push(g.t0);
                } else {
                    stops.push(time);
                }
                was_running = g.running;
            }
            if g.running {
                if g.epoch != last_epoch {
                    last_epoch = g.epoch;
                    last_emitted = g.n0 - 1;
                }
                // Emulate the clock/click thread: the beat is emitted when its
                // predicted time passes, at the time predicted at that moment.
                let n = last_emitted + 1;
                if g.beat_time(n) <= time {
                    last_emitted = n;
                    emitted.push((g.beat_time(n), g.bpm()));
                    if verbose {
                        println!("beat {n:5} at {:9.4}  {:6.2} bpm  conf {:.2}", g.beat_time(n), g.bpm(), g.confidence);
                    }
                }
            }
        });
    }
    println!("{}: {:.1} s, {} onsets, {} beats emitted, starts at {:?}, stops at {:?}",
        wav.display(), audio.len() as f64 / fs, onsets_raw.len(), emitted.len(),
        starts.iter().map(|s| format!("{s:.2}")).collect::<Vec<_>>(),
        stops.iter().map(|s| format!("{s:.2}")).collect::<Vec<_>>());
    println!("final: {:?}, {:.2} bpm, confidence {:.2}, corrections {}, relocks {}",
        tr.grid().state, tr.grid().bpm(), tr.grid().confidence, tr.corrections, tr.relocks);
    if let Some(p) = dump {
        let text: String = emitted.iter().map(|(t, b)| format!("{t:.6} {b:.3}\n")).collect();
        std::fs::write(p, text)?;
    }
    if truth.is_empty() {
        return Ok(());
    }
    // Onset offset calibration: raw onset time minus nearest true beat, for
    // onsets that are within 60 ms of a true beat.
    let mut offs: Vec<f64> = onsets_raw
        .iter()
        .filter_map(|o| {
            let (d, _) = nearest(&truth, *o);
            if d.abs() < 0.06 { Some(d) } else { None }
        })
        .collect();
    if !offs.is_empty() {
        offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("onset offset (ODF peak minus true beat): median {:.1} ms over {} on-beat onsets -> use onset_offset_ms = {:.1}",
            offs[offs.len() / 2] * 1000.0, offs.len(), offs[offs.len() / 2] * 1000.0);
    }
    if emitted.is_empty() {
        println!("no beats emitted");
        return Ok(());
    }
    let lock_t = emitted[0].0;
    let first_truth = truth[0];
    println!("lock: first emitted beat at {lock_t:.2} s ({:.2} s after the first true beat)", lock_t - first_truth);
    let truth_end = truth[truth.len() - 1] + 0.5 * (truth[truth.len() - 1] - truth[0]) / (truth.len() - 1).max(1) as f64;
    let errs: Vec<f64> = emitted.iter().filter(|(t, _)| *t <= truth_end).map(|(t, _)| nearest(&truth, *t).0).collect();
    let mut abs: Vec<f64> = errs.iter().map(|e| e.abs()).collect();
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| abs[((abs.len() as f64 - 1.0) * p) as usize] * 1000.0;
    let within = |ms: f64| abs.iter().filter(|&&e| e * 1000.0 <= ms).count() as f64 / abs.len() as f64 * 100.0;
    let signed_med = {
        let mut s = errs.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2] * 1000.0
    };
    println!("emitted-beat error vs truth: median {:.1} ms (signed median {signed_med:+.1} ms), p90 {:.1} ms, max {:.1} ms",
        pct(0.5), pct(0.9), pct(1.0));
    println!("within 10 ms: {:.0}%   within 20 ms: {:.0}%   within 70 ms: {:.0}%", within(10.0), within(20.0), within(70.0));
    // Recall: true beats (after lock) that have an emitted beat within 70 ms.
    let after: Vec<&f64> = truth.iter().filter(|&&t| t >= lock_t).collect();
    let em_times: Vec<f64> = emitted.iter().map(|e| e.0).collect();
    let hit = after.iter().filter(|&&&t| nearest(&em_times, t).0.abs() <= 0.07).count();
    println!("true beats after lock covered within 70 ms: {}/{} ({:.0}%)", hit, after.len(), hit as f64 / after.len().max(1) as f64 * 100.0);
    // Octave check.
    let truth_period = if truth.len() > 1 { (truth[truth.len() - 1] - truth[0]) / (truth.len() - 1) as f64 } else { 0.0 };
    let mut ratios: Vec<f64> = emitted.iter().map(|(_, b)| (60.0 / b) / truth_period).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("period ratio tracker/truth: median {:.3} (1.0 = right octave; 0.5/2.0 = octave error)", ratios[ratios.len() / 2]);
    Ok(())
}

/// Signed distance from `t` to the nearest element of sorted `v`, and its index.
fn nearest(v: &[f64], t: f64) -> (f64, usize) {
    if v.is_empty() {
        return (f64::INFINITY, 0);
    }
    let i = v.partition_point(|&x| x < t);
    let mut best = (f64::INFINITY, 0usize);
    for j in [i.wrapping_sub(1), i] {
        if j < v.len() {
            let d = t - v[j];
            if d.abs() < best.0.abs() {
                best = (d, j);
            }
        }
    }
    best
}
