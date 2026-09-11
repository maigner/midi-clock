//! midi-clock: listen to a microphone, track the beat, emit MIDI clock and a click.

mod config;
mod live;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

use config::Config;
use live::Live;
use sequencer::SeqState;
use tracker::{Grid, HopInput, ManualClock, State, Tracker};
use ui::{Action, Mode, Page};

#[derive(Parser, Debug)]
#[command(version, about = "Live beat tracking to MIDI master clock and click")]
struct Args {
    /// Configuration file (TOML). Missing file = defaults.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// List sound cards and MIDI outputs, then exit.
    #[arg(long)]
    list_devices: bool,
    /// Print the effective configuration as TOML, then exit.
    #[arg(long)]
    print_config: bool,
    /// Run without MIDI output (audio and click only).
    #[arg(long)]
    no_midi: bool,
    /// Run without any audio interface (UI and MIDI only; manual mode is all that can run).
    #[arg(long)]
    no_audio: bool,
    /// Exit after this many seconds (0 = run until Ctrl-C).
    #[arg(long, default_value_t = 0.0)]
    seconds: f64,
    /// Print a status line every interval on its own line instead of in place.
    #[arg(short, long)]
    verbose: bool,
    /// Run without the touch-screen UI even if the config enables it.
    #[arg(long)]
    no_ui: bool,
    /// Start in this mode: auto (follow the microphone) or manual (tap / set tempo).
    #[arg(long, value_parser = parse_mode)]
    mode: Option<Mode>,
    /// Render one UI frame with example data to this PPM file and exit (layout check).
    #[arg(long, value_name = "FILE.ppm")]
    ui_snapshot: Option<PathBuf>,
    /// Screen size for --ui-snapshot, WIDTHxHEIGHT.
    #[arg(long, default_value = "800x480")]
    ui_size: String,
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(Mode::Auto),
        "manual" => Ok(Mode::Manual),
        _ => Err("expected 'auto' or 'manual'".into()),
    }
}

fn snapshot(path: &std::path::Path, size: &str) -> Result<()> {
    let (w, h) = size
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .context("--ui-size must be WIDTHxHEIGHT")?;
    let now = rtio::now();
    let grid = Grid { running: true, t0: now - 0.03, n0: 40, period: 0.5, state: State::Running, confidence: 0.82, ..Grid::default() };
    let st = ui::Status {
        mode: Mode::Auto,
        live: grid,
        tracker: grid,
        tempo_strength: 0.41,
        level_db: -18.0,
        xruns: 0,
        ticks: 12345,
        late_max_us: 82,
        midi: true,
        taps: 0,
        now,
        seq: Arc::new(SeqState::default()),
    };
    std::fs::write(path, ui::snapshot(w, h, &st, Page::Tempo, None))?;
    let manual = path.with_extension("manual.ppm");
    let st2 = ui::Status { mode: Mode::Manual, taps: 3, ..st.clone() };
    std::fs::write(&manual, ui::snapshot(w, h, &st2, Page::Tempo, Some(ui::Button::Tap)))?;
    let seqp = path.with_extension("seq.ppm");
    let mut seq = SeqState::default();
    seq.apply(sequencer::SeqAction::Bars(2));
    for k in [0, 4, 8, 12] {
        seq.apply(sequencer::SeqAction::ToggleStep(0, k));
    }
    for k in [4, 12] {
        seq.apply(sequencer::SeqAction::ToggleStep(1, k));
    }
    for k in (0..16).step_by(2) {
        seq.apply(sequencer::SeqAction::ToggleStep(2, k));
    }
    seq.apply(sequencer::SeqAction::SelectTrack(0));
    seq.apply(sequencer::SeqAction::Bar(0)); // locked to bar 1
    let st3 = ui::Status { seq: Arc::new(seq.clone()), ..st.clone() };
    std::fs::write(&seqp, ui::snapshot(w, h, &st3, Page::Sequence, None))?;
    let bassp = path.with_extension("bass.ppm");
    use sequencer::SeqAction::{BassBars, BassSet};
    seq.apply(BassBars(4));
    for (row, k) in [(0, 0), (0, 3), (7, 6), (10, 8), (0, 10), (3, 12), (5, 14)] {
        seq.apply(BassSet(row, k));
    }
    let st4 = ui::Status { seq: Arc::new(seq), ..st };
    std::fs::write(&bassp, ui::snapshot(w, h, &st4, Page::Bass, None))?;
    eprintln!("wrote {}, {}, {} and {}", path.display(), manual.display(), seqp.display(), bassp.display());
    Ok(())
}

/// One hop of analysis, audio thread -> tracker thread.
#[derive(Clone, Copy)]
struct HopMsg {
    time: f64,
    odf: f32,
    kick: f32,
    onset: Option<(f64, f32)>,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    xruns: AtomicU64,
    level_milli_db: AtomicU32, // input peak level in dB * -1000 (u32)
    clicks: AtomicU64,
    hops: AtomicU64,
    onsets: AtomicU64,
    tempo_strength_milli: AtomicU32,
    audio_error: AtomicBool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.list_devices {
        println!("Sound cards:");
        for l in rtio::pcm::list_cards() {
            println!("  {l}");
        }
        println!("MIDI outputs:");
        for l in rtio::midi::list_outputs() {
            println!("  {l}");
        }
        println!("Framebuffer:");
        match ui::fb::Framebuffer::open("/dev/fb0") {
            Ok(fb) => println!("  /dev/fb0: {}", fb.describe()),
            Err(e) => println!("  /dev/fb0: {e}"),
        }
        println!("Touch devices:");
        let touch = ui::touch::list();
        if touch.is_empty() {
            println!("  none found (are you in the 'input' group?)");
        }
        for (p, n) in touch {
            println!("  {p}: {n}");
        }
        return Ok(());
    }
    let cfg = if args.config.exists() {
        Config::load(&args.config).with_context(|| format!("reading {}", args.config.display()))?
    } else {
        eprintln!("config {} not found, using defaults", args.config.display());
        Config::default()
    };
    if args.print_config {
        print!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }
    if let Some(p) = &args.ui_snapshot {
        return snapshot(p, &args.ui_size);
    }
    let start_mode = args.mode.unwrap_or(match cfg.ui.start_mode.to_ascii_lowercase().as_str() {
        "manual" => Mode::Manual,
        _ => Mode::Auto,
    });

    if cfg.realtime.lock_memory {
        if let Err(e) = rtio::rt::lock_memory() {
            eprintln!("warning: {e}");
        }
    }

    let duplex = if cfg.audio.enabled && !args.no_audio {
        let d = rtio::Duplex::open(&cfg.pcm()).context("opening audio interface")?;
        eprintln!("audio: {}", d.describe());
        Some(d)
    } else {
        eprintln!("audio: disabled (no capture, no click)");
        None
    };
    let midi = if cfg.midi.enabled && !args.no_midi {
        let m = rtio::MidiOut::open(&cfg.midi.device).context("opening MIDI output")?;
        eprintln!("midi: {}", m.name());
        Some(Arc::new(m))
    } else {
        eprintln!("midi: disabled");
        None
    };

    let shared = Arc::new(Shared::default());
    shared.level_milli_db.store(120_000, Ordering::Relaxed); // -120 dB until audio reports
    let live = Arc::new(Live::new(start_mode));
    let mut manual = ManualClock::new(cfg.manual(), cfg.ui.manual_bpm);
    if start_mode == Mode::Manual {
        live.publish_manual(manual.grid());
    }
    let clock_stats = Arc::new(midiclock::ClockStats::default());
    {
        let s = shared.clone();
        ctrlc::set_handler(move || s.stop.store(true, Ordering::Relaxed))?;
    }

    let rb = HeapRb::<HopMsg>::new(4096);
    let (mut prod, mut cons) = rb.split();
    let cpu = if cfg.realtime.cpu >= 0 { Some(cfg.realtime.cpu as usize) } else { None };

    // --- MIDI clock thread -------------------------------------------------
    // --- Sequencer state and thread -------------------------------------------
    let seq_path = PathBuf::from(&cfg.sequencer.file);
    let seq_state = if seq_path.exists() {
        match SeqState::load(&seq_path) {
            Ok(s) => {
                eprintln!("sequencer: loaded {}", seq_path.display());
                s
            }
            Err(e) => {
                eprintln!("sequencer: {e}; starting with empty patterns");
                SeqState::default()
            }
        }
    } else {
        SeqState::default()
    };
    let seq_snapshot = Arc::new(arc_swap::ArcSwap::from_pointee(seq_state.snapshot()));
    let seq_stats = Arc::new(sequencer::SeqStats::default());
    let seq_thread = match (&midi, cfg.sequencer.enabled) {
        (Some(midi), true) => {
            let midi = midi.clone();
            let live = live.clone();
            let shared = shared.clone();
            let snap = seq_snapshot.clone();
            let stats = seq_stats.clone();
            let lead = (cfg.midi.lead_ms + cfg.sequencer.lead_ms) / 1000.0;
            let prio = cfg.realtime.sequencer_priority;
            Some(
                thread::Builder::new()
                    .name("sequencer".into())
                    .spawn(move || {
                        for w in rtio::rt::setup_thread("sequencer", prio, cpu) {
                            eprintln!("warning: {w}");
                        }
                        sequencer::run(|| **live.grid.load(), || snap.load_full(), &*midi, lead, &shared.stop, &stats);
                    })
                    .expect("spawn sequencer thread"),
            )
        }
        _ => None,
    };

    let midi_thread = midi.map(|midi| {
        let live = live.clone();
        let shared = shared.clone();
        let stats = clock_stats.clone();
        let ccfg = cfg.midiclock();
        let prio = cfg.realtime.midi_priority;
        thread::Builder::new()
            .name("midi-clock".into())
            .spawn(move || {
                for w in rtio::rt::setup_thread("midi", prio, cpu) {
                    eprintln!("warning: {w}");
                }
                midiclock::run(|| **live.grid.load(), &midi, &ccfg, &shared.stop, &stats);
            })
            .expect("spawn midi thread")
    });

    // --- Tracker thread ----------------------------------------------------
    let tracker_thread = {
        let live = live.clone();
        let shared = shared.clone();
        let tcfg = cfg.tracker();
        let prio = cfg.realtime.tracker_priority;
        thread::Builder::new()
            .name("tracker".into())
            .spawn(move || {
                for w in rtio::rt::setup_thread("tracker", prio, None) {
                    eprintln!("warning: {w}");
                }
                let mut tr = Tracker::new(tcfg);
                while !shared.stop.load(Ordering::Relaxed) {
                    if live.relock.swap(false, Ordering::Relaxed) {
                        tr.relock();
                        live.publish_tracker(tr.grid());
                    }
                    if live.adopt.swap(false, Ordering::Relaxed) {
                        tr.adopt(**live.grid.load(), rtio::now());
                        live.publish_tracker(tr.grid());
                    }
                    let mut any = false;
                    while let Some(m) = cons.try_pop() {
                        any = true;
                        let changed = tr.process_hop(HopInput { time: m.time, odf: m.odf, kick: m.kick, onset: m.onset });
                        if changed {
                            live.publish_tracker(tr.grid());
                        }
                    }
                    if any {
                        shared
                            .tempo_strength_milli
                            .store((tr.last_tempo_strength * 1000.0) as u32, Ordering::Relaxed);
                    }
                    thread::park_timeout(Duration::from_millis(2));
                }
            })
            .expect("spawn tracker thread")
    };
    let tracker_handle = tracker_thread.thread().clone();
    let tracker_handle_main = tracker_handle.clone();

    // --- Audio thread ------------------------------------------------------
    let audio_thread = duplex.map(|mut duplex| {
        let live = live.clone();
        let shared = shared.clone();
        let ocfg = cfg.onset();
        let ccfg = cfg.click();
        let onset_offset = cfg.tempo.onset_offset_ms / 1000.0;
        let mic_channel = cfg.audio.mic_channel;
        let click_channels = cfg.audio.click_channels.clone();
        let click_enabled = cfg.click.enabled;
        let monitor_gain = cfg.audio.monitor_gain;
        let prio = cfg.realtime.audio_priority;
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                for w in rtio::rt::setup_thread("audio", prio, cpu) {
                    eprintln!("warning: {w}");
                }
                let period = duplex.period();
                let fs = duplex.sample_rate() as f64;
                let cap_ch = duplex.capture.channels();
                let play_ch = duplex.playback.channels();
                let mic = mic_channel % cap_ch;
                let click_ch: Vec<usize> = click_channels.iter().map(|&c| c % play_ch).collect();
                let mut det = onset::OnsetDetector::new(ocfg);
                let hop_dt = det.hop_dt();
                let mut synth = click::ClickSynth::new(ccfg);
                let mut inbuf = vec![0f32; period * cap_ch];
                let mut mono = vec![0f32; period];
                let mut click_mono = vec![0f32; period];
                let mut outbuf = vec![0f32; period * play_ch];
                let mut total: u64 = 0;
                if let Err(e) = duplex.start() {
                    eprintln!("audio start failed: {e:#}");
                    shared.audio_error.store(true, Ordering::Relaxed);
                    shared.stop.store(true, Ordering::Relaxed);
                    return;
                }
                while !shared.stop.load(Ordering::Relaxed) {
                    let t_last = match duplex.capture.read(&mut inbuf) {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!("capture failed: {e:#}");
                            shared.audio_error.store(true, Ordering::Relaxed);
                            break;
                        }
                    };
                    total += period as u64;
                    let mut peak = 0f32;
                    for (i, m) in mono.iter_mut().enumerate() {
                        *m = inbuf[i * cap_ch + mic];
                        peak = peak.max(m.abs());
                    }
                    let db = 20.0 * (peak + 1e-9).log10();
                    shared.level_milli_db.store((-db * 1000.0).max(0.0) as u32, Ordering::Relaxed);
                    let buf_end = total;
                    let mut pushed = false;
                    det.process(&mono, |h| {
                        let time = t_last - (buf_end - h.end_sample) as f64 / fs;
                        let onset = h.onset.map(|o| {
                            let t_on = time - (h.index - o.hop_index) as f64 * hop_dt - onset_offset;
                            (t_on, o.strength)
                        });
                        if onset.is_some() {
                            shared.onsets.fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = prod.try_push(HopMsg { time, odf: h.odf, kick: h.kick, onset });
                        pushed = true;
                    });
                    if pushed {
                        shared.hops.fetch_add(1, Ordering::Relaxed);
                        tracker_handle.unpark();
                    }
                    outbuf.fill(0.0);
                    if click_enabled {
                        let g = **live.grid.load();
                        let t_play = duplex.playback.next_write_time();
                        synth.render(&g, t_play, &mut click_mono);
                        for (i, &c) in click_mono.iter().enumerate() {
                            for &ch in &click_ch {
                                outbuf[i * play_ch + ch] += c;
                            }
                        }
                        shared.clicks.store(synth.clicks_rendered, Ordering::Relaxed);
                    }
                    if monitor_gain > 0.0 {
                        for (i, &m) in mono.iter().enumerate() {
                            for &ch in &click_ch {
                                outbuf[i * play_ch + ch] += monitor_gain * m;
                            }
                        }
                    }
                    if let Err(e) = duplex.playback.write(&outbuf) {
                        eprintln!("playback failed: {e:#}");
                        shared.audio_error.store(true, Ordering::Relaxed);
                        break;
                    }
                    shared.xruns.store(duplex.xruns(), Ordering::Relaxed);
                }
                shared.stop.store(true, Ordering::Relaxed);
            })
            .expect("spawn audio thread")
    });

    // --- UI thread -----------------------------------------------------------
    let ui_thread = if cfg.ui.enabled && !args.no_ui {
        let live = live.clone();
        let shared = shared.clone();
        let stats = clock_stats.clone();
        let ucfg = cfg.ui();
        let midi_on = cfg.midi.enabled && !args.no_midi;
        let seq_snapshot = seq_snapshot.clone();
        let seq_path = seq_path.clone();
        Some(
            thread::Builder::new()
                .name("ui".into())
                .spawn(move || {
                    // SAFETY: plain syscall; lowers this thread's priority only.
                    unsafe {
                        libc::setpriority(libc::PRIO_PROCESS, 0, 5);
                    }
                    let manual = std::cell::RefCell::new(manual);
                    let seq = std::cell::RefCell::new(Arc::new(seq_state));
                    let seq_dirty = std::cell::Cell::new(None::<f64>);
                    let status = || {
                        // Save patterns one second after the last edit.
                        if let Some(t) = seq_dirty.get() {
                            if rtio::now() - t > 1.0 {
                                seq_dirty.set(None);
                                if let Err(e) = seq.borrow().save(&seq_path) {
                                    eprintln!("sequencer: save failed: {e}");
                                }
                            }
                        }
                        let level = -(shared.level_milli_db.load(Ordering::Relaxed) as f32) / 1000.0;
                        ui::Status {
                            mode: live.mode(),
                            live: **live.grid.load(),
                            tracker: **live.tracker_grid.load(),
                            tempo_strength: shared.tempo_strength_milli.load(Ordering::Relaxed) as f32 / 1000.0,
                            level_db: level,
                            xruns: shared.xruns.load(Ordering::Relaxed),
                            ticks: stats.ticks.load(Ordering::Relaxed),
                            late_max_us: stats.late_max_us.load(Ordering::Relaxed),
                            midi: midi_on,
                            taps: manual.borrow().taps(),
                            now: rtio::now(),
                            seq: seq.borrow().clone(),
                        }
                    };
                    let act = |a: Action| {
                        let now = rtio::now();
                        let mut manual = manual.borrow_mut();
                        match a {
                            Action::SetMode(Mode::Manual) => {
                                if live.mode() != Mode::Manual {
                                    let g = live.set_mode(Mode::Manual);
                                    manual.adopt(&g);
                                    live.publish_manual(manual.grid());
                                }
                            }
                            Action::SetMode(Mode::Auto) => {
                                if live.mode() != Mode::Auto {
                                    live.set_mode(Mode::Auto);
                                }
                            }
                            Action::Tap(t) => {
                                manual.tap(t);
                                live.publish_manual(manual.grid());
                            }
                            Action::Nudge(d) => {
                                manual.nudge_bpm(d, now);
                                live.publish_manual(manual.grid());
                            }
                            Action::ToggleRun => {
                                manual.toggle(now);
                                live.publish_manual(manual.grid());
                            }
                            Action::Relock => live.relock.store(true, Ordering::Relaxed),
                            Action::Seq(sa) => {
                                let mut arc = seq.borrow_mut();
                                if Arc::make_mut(&mut arc).apply(sa) && !sa.view_only() {
                                    seq_snapshot.store(Arc::new(arc.snapshot()));
                                    seq_dirty.set(Some(now));
                                }
                            }
                        }
                    };
                    if let Err(e) = ui::run(&ucfg, &shared.stop, status, act) {
                        eprintln!("ui: {e}; continuing without the screen");
                    }
                })
                .expect("spawn ui thread"),
        )
    } else {
        if start_mode == Mode::Manual {
            // Headless manual mode: a fixed clock at the configured tempo.
            manual.start(rtio::now() + 0.5);
            live.publish_manual(manual.grid());
        }
        None
    };

    // --- Status loop ---------------------------------------------------------
    let t_start = rtio::now();
    eprintln!("running. Ctrl-C to stop.");
    // SAFETY: isatty on a valid descriptor.
    let on_tty = unsafe { libc::isatty(2) } == 1;
    let interval = if on_tty || args.verbose { Duration::from_millis(250) } else { Duration::from_secs(5) };
    while !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(interval);
        if args.seconds > 0.0 && rtio::now() - t_start > args.seconds {
            shared.stop.store(true, Ordering::Relaxed);
        }
        let g = **live.grid.load();
        let state = match g.state {
            State::Idle => "idle",
            State::Locking => "locking",
            State::Running => "RUNNING",
            State::Coasting => "coasting",
            State::Stopped => "stopped",
        };
        let mode = match live.mode() {
            Mode::Auto => "auto",
            Mode::Manual => "manual",
        };
        let level = -(shared.level_milli_db.load(Ordering::Relaxed) as f32) / 1000.0;
        let line = format!(
            "{mode:6} {state:8} {:6.1} bpm  conf {:.2}  tempo-str {:.2}  in {level:5.1} dBFS  onsets {}  clicks {}  ticks {}  xruns {}",
            g.bpm(),
            g.confidence,
            shared.tempo_strength_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            shared.onsets.load(Ordering::Relaxed),
            shared.clicks.load(Ordering::Relaxed),
            clock_stats.ticks.load(Ordering::Relaxed),
            shared.xruns.load(Ordering::Relaxed),
        );
        if args.verbose || !on_tty {
            eprintln!("{line}");
        } else {
            eprint!("\r{line}   ");
        }
    }
    eprintln!();
    tracker_handle_main.unpark();
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    let _ = tracker_thread.join();
    if let Some(t) = ui_thread {
        let _ = t.join();
    }
    if let Some(t) = seq_thread {
        let _ = t.join();
        eprintln!(
            "sequencer: {} notes, {} errors",
            seq_stats.notes.load(Ordering::Relaxed),
            seq_stats.errors.load(Ordering::Relaxed)
        );
    }
    if let Some(t) = midi_thread {
        let _ = t.join();
        eprintln!("midi clock: {}", clock_stats.summary());
    }
    eprintln!("xruns: {}", shared.xruns.load(Ordering::Relaxed));
    if shared.audio_error.load(Ordering::Relaxed) {
        anyhow::bail!("audio thread stopped with an error");
    }
    Ok(())
}
