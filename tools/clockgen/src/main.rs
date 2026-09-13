//! clockgen: proves the real-time path before any beat tracking exists.
//!
//! `clockgen run`     fixed-BPM MIDI clock + click; reports tick lateness and xruns.
//! `clockgen latency` loopback measurement: connect an output to an input with a
//!                    cable, and it reports the latency ALSA does not account for.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracker::Grid;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct AudioArgs {
    /// ALSA device for capture and playback, e.g. hw:CARD=US2x2
    #[arg(long, default_value = "hw:CARD=US2x2")]
    audio: String,
    #[arg(long, default_value_t = 48000)]
    rate: u32,
    #[arg(long, default_value_t = 64)]
    period: usize,
    #[arg(long, default_value_t = 3)]
    periods: u32,
    /// Real-time priority for the audio thread (0 = none)
    #[arg(long, default_value_t = 80)]
    audio_priority: i32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fixed-tempo MIDI clock and click.
    Run {
        #[arg(long, default_value_t = 120.0)]
        bpm: f64,
        /// rawmidi output, e.g. hw:CARD=US2x2,0,0. Omit for no MIDI.
        #[arg(long)]
        midi: Option<String>,
        /// Disable the audio click (MIDI only).
        #[arg(long)]
        no_audio: bool,
        #[arg(long, default_value_t = 30.0)]
        seconds: f64,
        #[arg(long, default_value_t = 85)]
        midi_priority: i32,
        #[arg(long, default_value_t = 1.5)]
        midi_lead_ms: f64,
        #[command(flatten)]
        audio: AudioArgs,
    },
    /// Loopback latency measurement (output -> input cable required).
    Latency {
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Capture channel that receives the loopback.
        #[arg(long, default_value_t = 0)]
        channel: usize,
        #[command(flatten)]
        audio: AudioArgs,
    },
}

fn pcm_config(a: &AudioArgs) -> rtio::PcmConfig {
    rtio::PcmConfig {
        capture_device: a.audio.clone(),
        playback_device: a.audio.clone(),
        sample_rate: a.rate,
        period_frames: a.period,
        periods: a.periods,
        capture_channels: 2,
        playback_channels: 2,
        extra_input_latency: 0.0,
        extra_output_latency: 0.0,
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        ctrlc::set_handler(move || s.store(true, Ordering::Relaxed))?;
    }
    if let Err(e) = rtio::rt::lock_memory() {
        eprintln!("warning: {e}");
    }
    match args.cmd {
        Cmd::Run { bpm, midi, no_audio, seconds, midi_priority, midi_lead_ms, audio } => {
            run(bpm, midi, no_audio, seconds, midi_priority, midi_lead_ms, audio, stop)
        }
        Cmd::Latency { seconds, channel, audio } => latency(seconds, channel, audio, stop),
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    bpm: f64,
    midi: Option<String>,
    no_audio: bool,
    seconds: f64,
    midi_priority: i32,
    midi_lead_ms: f64,
    audio: AudioArgs,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let grid = Grid {
        epoch: 1,
        running: true,
        state: tracker::State::Running,
        t0: rtio::now() + 1.0,
        n0: 0,
        period: 60.0 / bpm,
        confidence: 1.0,
        bar_len: 4,
        downbeat: 0,
        downbeat_known: true,
    };
    let stats = Arc::new(midiclock::ClockStats::default());
    let midi_thread = match midi {
        Some(name) => {
            let m = rtio::MidiOut::open(&name)?;
            eprintln!("midi: {}", m.name());
            let stop = stop.clone();
            let stats = stats.clone();
            let cfg = midiclock::ClockConfig { lead_s: midi_lead_ms / 1000.0, send_start_stop: true };
            Some(thread::spawn(move || {
                for w in rtio::rt::setup_thread("midi", midi_priority, None) {
                    eprintln!("warning: {w}");
                }
                midiclock::run(|| grid, &m, &cfg, &stop, &stats);
            }))
        }
        None => None,
    };
    let audio_thread = if no_audio {
        None
    } else {
        let mut duplex = rtio::Duplex::open(&pcm_config(&audio)).context("opening audio")?;
        eprintln!("audio: {}", duplex.describe());
        let stop = stop.clone();
        let prio = audio.audio_priority;
        Some(thread::spawn(move || -> Result<(u64, u64)> {
            for w in rtio::rt::setup_thread("audio", prio, None) {
                eprintln!("warning: {w}");
            }
            let period = duplex.period();
            let cap_ch = duplex.capture.channels();
            let play_ch = duplex.playback.channels();
            let mut synth = click::ClickSynth::new(click::ClickConfig {
                sample_rate: duplex.sample_rate() as f32,
                ..click::ClickConfig::default()
            });
            let mut inbuf = vec![0f32; period * cap_ch];
            let mut mono = vec![0f32; period];
            let mut outbuf = vec![0f32; period * play_ch];
            duplex.start()?;
            while !stop.load(Ordering::Relaxed) {
                duplex.capture.read(&mut inbuf)?;
                let t_play = duplex.playback.next_write_time();
                synth.render(&grid, t_play, &mut mono);
                for (i, &c) in mono.iter().enumerate() {
                    for ch in 0..play_ch {
                        outbuf[i * play_ch + ch] = c;
                    }
                }
                duplex.playback.write(&outbuf)?;
            }
            Ok((duplex.xruns(), synth.clicks_rendered))
        }))
    };
    let t0 = rtio::now();
    eprintln!("running {bpm} BPM for {seconds} s (Ctrl-C to stop early)");
    while !stop.load(Ordering::Relaxed) && rtio::now() - t0 < seconds {
        thread::sleep(Duration::from_millis(500));
        eprint!("\rticks {}  {}   ", stats.ticks.load(Ordering::Relaxed), stats.summary());
    }
    stop.store(true, Ordering::Relaxed);
    eprintln!();
    if let Some(t) = midi_thread {
        let _ = t.join();
        println!("MIDI clock: {}", stats.summary());
        let hist: Vec<String> = stats
            .late_hist
            .iter()
            .zip(midiclock::LATE_BUCKETS_US.iter())
            .map(|(c, ub)| format!("<{}us:{}", if *ub == u64::MAX { "inf".into() } else { ub.to_string() }, c.load(Ordering::Relaxed)))
            .collect();
        println!("lateness histogram: {}", hist.join(" "));
    }
    if let Some(t) = audio_thread {
        match t.join().expect("audio thread panicked") {
            Ok((xruns, clicks)) => println!("audio: {clicks} clicks rendered, {xruns} xruns"),
            Err(e) => println!("audio error: {e:#}"),
        }
    }
    Ok(())
}

fn latency(seconds: f64, channel: usize, audio: AudioArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let mut duplex = rtio::Duplex::open(&pcm_config(&audio)).context("opening audio")?;
    eprintln!("audio: {}", duplex.describe());
    for w in rtio::rt::setup_thread("audio", audio.audio_priority, None) {
        eprintln!("warning: {w}");
    }
    let fs = duplex.sample_rate() as f64;
    let period = duplex.period();
    let cap_ch = duplex.capture.channels();
    let play_ch = duplex.playback.channels();
    let grid = Grid {
        epoch: 1,
        running: true,
        state: tracker::State::Running,
        t0: rtio::now() + 0.5,
        n0: 0,
        period: 0.5,
        confidence: 1.0,
        bar_len: 4,
        downbeat: 0,
        downbeat_known: true,
    };
    let mut synth = click::ClickSynth::new(click::ClickConfig {
        sample_rate: fs as f32,
        gain: 0.9,
        accent_gain: 0.9,
        duration_s: 0.004,
        ..click::ClickConfig::default()
    });
    let mut inbuf = vec![0f32; period * cap_ch];
    let mut mono = vec![0f32; period];
    let mut outbuf = vec![0f32; period * play_ch];
    let mut residuals = Vec::new();
    let mut armed = true;
    let mut noise_floor = 0.0f32;
    let t_start = rtio::now();
    duplex.start()?;
    eprintln!("measuring for {seconds} s; make sure a cable connects an output to input channel {channel}");
    while !stop.load(Ordering::Relaxed) && rtio::now() - t_start < seconds {
        let t_last = duplex.capture.read(&mut inbuf)?;
        let ch = channel % cap_ch;
        let mut peak_i = None;
        let mut peak = 0f32;
        for i in 0..period {
            let v = inbuf[i * cap_ch + ch].abs();
            if v > peak {
                peak = v;
                if peak_i.is_none() && v > 0.05 && v > 8.0 * noise_floor {
                    peak_i = Some(i);
                }
            }
        }
        if rtio::now() - t_start < 0.4 {
            noise_floor = noise_floor.max(peak);
        }
        if let Some(i) = peak_i {
            if armed {
                let t_hit = t_last - (period - 1 - i) as f64 / fs;
                let (n, err) = grid.nearest_beat(t_hit);
                if n >= 0 && err.abs() < 0.2 {
                    residuals.push(err);
                }
                armed = false;
            }
        } else if peak < 0.01 {
            armed = true;
        }
        let t_play = duplex.playback.next_write_time();
        synth.render(&grid, t_play, &mut mono);
        for (i, &c) in mono.iter().enumerate() {
            for ch in 0..play_ch {
                outbuf[i * play_ch + ch] = c;
            }
        }
        duplex.playback.write(&outbuf)?;
    }
    if residuals.is_empty() {
        println!("no loopback signal detected (noise floor {noise_floor:.4}). Check the cable and input gain.");
        return Ok(());
    }
    residuals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = residuals[residuals.len() / 2] * 1000.0;
    let min = residuals[0] * 1000.0;
    let max = residuals[residuals.len() - 1] * 1000.0;
    println!("{} clicks measured. Unreported round-trip latency: median {med:.2} ms (min {min:.2}, max {max:.2}).", residuals.len());
    println!("Suggested config: extra_input_latency_ms = {:.2}, extra_output_latency_ms = {:.2}", med / 2.0, med / 2.0);
    println!("(The click itself starts one sample late and the detector fires on the first strong sample, so ±0.1 ms is noise.)");
    println!("xruns: {}", duplex.xruns());
    Ok(())
}
