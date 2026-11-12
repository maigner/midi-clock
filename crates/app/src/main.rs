//! midi-clock: listen to a microphone, track the beat, emit MIDI clock and a click.

mod config;
mod live;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
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
use tracker::{Grid, HopInput, ManualClock, MidiFollower, State, Tracker};
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
    /// Start in this mode: auto (follow the microphone), manual (tap / set tempo) or midi (follow the MIDI input's clock).
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
        "midi" | "midi-in" | "midiin" => Ok(Mode::MidiIn),
        _ => Err("expected 'auto', 'manual' or 'midi'".into()),
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
        clipping: false,
        peak_db: -18.0,
        midi_in: Grid::default(),
        xruns: 0,
        ticks: 12345,
        midi: true,
        taps: 0,
        now,
        seq: Arc::new(SeqState::default()),
        key: None,
        bpm_shown: None,
    };
    std::fs::write(path, ui::snapshot(w, h, &st, Page::Tempo, None))?;
    let manual = path.with_extension("manual.ppm");
    let st2 = ui::Status { mode: Mode::Manual, taps: 3, ..st.clone() };
    std::fs::write(&manual, ui::snapshot(w, h, &st2, Page::Tempo, Some(ui::Button::Tap)))?;
    let seqp = path.with_extension("lead.ppm");
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
    seq.apply(sequencer::SeqAction::CellDown(3, 4)); // an open hat ringing 4 steps
    seq.apply(sequencer::SeqAction::CellDrag(7));
    seq.apply(sequencer::SeqAction::CellUp);
    seq.apply(sequencer::SeqAction::SelectTrack(0));
    seq.apply(sequencer::SeqAction::Bar(0)); // locked to bar 1
    let st3 = ui::Status { seq: Arc::new(seq.clone()), ..st.clone() };
    std::fs::write(&seqp, ui::snapshot(w, h, &st3, Page::Lead, None))?;
    let bassp = path.with_extension("bass.ppm");
    use sequencer::SeqAction::{BassBars, BassSet};
    seq.apply(BassBars(4));
    for (row, k) in [(0, 0), (0, 3), (7, 6), (10, 8), (0, 10), (3, 12), (5, 14)] {
        seq.apply(BassSet(row, k));
    }
    use sequencer::SeqAction::{BassBar, BassCellDown, BassCellDrag, BassCellDragBar, BassCellUp};
    seq.apply(BassCellDown(7, 6)); // two steps
    seq.apply(BassCellDrag(7));
    seq.apply(BassCellUp);
    seq.apply(BassCellDown(5, 14)); // across the bar into step 4 of bar 2
    seq.apply(BassCellDrag(15));
    seq.apply(BassCellDragBar(1));
    seq.apply(BassCellDrag(3));
    seq.apply(BassCellUp);
    seq.apply(BassBar(0));
    let st4 = ui::Status { seq: Arc::new(seq.clone()), ..st.clone() };
    std::fs::write(&bassp, ui::snapshot(w, h, &st4, Page::Bass, None))?;
    let drumsp = path.with_extension("drums.ppm");
    use sequencer::SeqAction::{DrumAdjust, DrumBar, DrumBars, DrumPad, DrumStep};
    seq.apply(DrumBars(2));
    for (pad, steps) in [(0, &[0, 8, 10][..]), (1, &[4, 12]), (3, &[4, 12]), (4, &[0, 2, 4, 6, 8, 10, 12, 14]), (5, &[14])] {
        seq.apply(DrumPad(pad, None));
        for k in steps {
            seq.apply(DrumStep(*k));
        }
    }
    seq.apply(DrumPad(4, None));
    seq.apply(DrumStep(0)); // accent
    seq.apply(DrumStep(8));
    seq.apply(DrumAdjust(sequencer::DrumParam::Swing, 8));
    seq.apply(DrumPad(0, None));
    seq.apply(DrumBar(0));
    let st5 = ui::Status { seq: Arc::new(seq.clone()), ..st.clone() };
    std::fs::write(&drumsp, ui::snapshot(w, h, &st5, Page::Drums, None))?;
    let setupp = path.with_extension("setup.ppm");
    seq.apply(sequencer::SeqAction::Channel(sequencer::Instrument::Bass, 1)); // bass on channel 2
    let st6 = ui::Status { seq: Arc::new(seq.clone()), ..st.clone() };
    std::fs::write(&setupp, ui::snapshot(w, h, &st6, Page::Settings, None))?;
    // the key page: heard (A minor, sure, the band a little sharp) and by hand
    let keyp = path.with_extension("key.ppm");
    let mut chroma = [0.15f32; 12];
    for (pc, c) in chroma.iter_mut().enumerate() {
        if sequencer::Key::new(9, sequencer::Scale::Minor).contains(pc as u8) {
            *c = 0.55;
        }
    }
    chroma[9] = 1.0;
    chroma[4] = 0.8;
    let heard = key::KeyEstimate { tonic: 9, mode: key::Mode::Minor, confidence: 0.82, tuning_cents: 7.0, chroma, alt: Some((0, key::Mode::Major)), frames: 100 };
    let st7 = ui::Status { key: Some(heard), ..st6.clone() };
    std::fs::write(&keyp, ui::snapshot(w, h, &st7, Page::Key, None))?;
    let keym = path.with_extension("key.manual.ppm");
    seq.apply(sequencer::SeqAction::KeyRoot(2));
    seq.apply(sequencer::SeqAction::KeyScale(sequencer::Scale::Dorian));
    let st8 = ui::Status { seq: Arc::new(seq), ..st7 };
    std::fs::write(&keym, ui::snapshot(w, h, &st8, Page::Key, None))?;
    eprintln!(
        "wrote {}, {}, {}, {}, {}, {}, {} and {}",
        path.display(),
        manual.display(),
        seqp.display(),
        bassp.display(),
        drumsp.display(),
        setupp.display(),
        keyp.display(),
        keym.display()
    );
    Ok(())
}

/// Input samples at or above this are counted as clipped (full scale is 1.0).
const CLIP_LEVEL: f32 = 0.98;
/// The level meter and the clip flag hold for this long.
const LEVEL_HOLD_S: f64 = 1.5;
/// The gain hint on the TEMPO page judges the loudest moment of this long.
const GAIN_HOLD_S: f64 = 8.0;

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
    level_milli_db: AtomicU32, // input peak level in dB * -1000 (u32), held for ~1.5 s
    /// Input peak level in dB * -1000, held for ~8 s (drives the gain hint).
    peak_slow_milli_db: AtomicU32,
    /// Input samples at or above `CLIP_LEVEL` so far.
    clipped: AtomicU64,
    /// The input clipped within the last ~1.5 s.
    clipping: AtomicBool,
    clicks: AtomicU64,
    hops: AtomicU64,
    onsets: AtomicU64,
    tempo_strength_milli: AtomicU32,
    /// `Tracker::onset_bias_s` in microseconds.
    bias_us: AtomicI32,
    /// `Tracker::holding`.
    holding: AtomicBool,
    /// `Tracker::onset_spread` * 1000.
    spread_milli: AtomicU32,
    audio_error: AtomicBool,
    /// Samples the key detector's ring could not take (it fell behind).
    key_overflow: AtomicU64,
    /// Realtime messages (clock, start, stop) read from the MIDI input.
    midi_in_msgs: AtomicU64,
    /// Other messages from the MIDI input sent on to the output.
    midi_thru: AtomicU64,
    /// Largest move of a beat by one follower update since the last status line (us).
    midi_in_shift_us: AtomicU32,
    /// The follower's fit residual (input jitter), us.
    midi_in_jitter_us: AtomicU32,
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
        println!("MIDI inputs:");
        for l in rtio::midi::list_inputs() {
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
        "midi" | "midi-in" | "midiin" => Mode::MidiIn,
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
    shared.peak_slow_milli_db.store(120_000, Ordering::Relaxed);
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
    // The key detector's input: 4 s of the key channel, audio thread -> key thread.
    let key_on = cfg.key.enabled && cfg.audio.enabled && !args.no_audio;
    let krb = HeapRb::<f32>::new(if key_on { cfg.audio.sample_rate as usize * 4 } else { 16 });
    let (mut kprod, mut kcons) = krb.split();
    let key_est = Arc::new(arc_swap::ArcSwap::from_pointee(None::<key::KeyEstimate>));

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

    let thru = if cfg.midi.thru { midi.clone() } else { None };
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
                midiclock::run(|| **live.grid.load(), &*midi, &ccfg, &shared.stop, &stats);
            })
            .expect("spawn midi thread")
    });

    // --- MIDI input thread ---------------------------------------------------
    // Reads the MIDI input and keeps the external clock's grid up to date;
    // in MIDI IN mode that grid is the live one. Whatever else arrives
    // (notes, controllers, ...) goes straight on to the output.
    let midi_in = if cfg.midi.in_device.is_empty() || args.no_midi {
        eprintln!("midi in: disabled");
        None
    } else {
        match rtio::MidiIn::open(&cfg.midi.in_device) {
            Ok(m) => {
                eprintln!("midi in: {}", m.name());
                Some(m)
            }
            Err(e) => {
                eprintln!("midi in: {e:#}; MIDI IN mode will have no clock");
                None
            }
        }
    };
    let midi_in_thread = midi_in.map(|mut port| {
        let live = live.clone();
        let shared = shared.clone();
        let bar_len = cfg.tempo.bar_len;
        let prio = cfg.realtime.midi_priority;
        thread::Builder::new()
            .name("midi-in".into())
            .spawn(move || {
                for w in rtio::rt::setup_thread("midi-in", prio, cpu) {
                    eprintln!("warning: {w}");
                }
                let mut f = MidiFollower::new(bar_len);
                let mut parser = rtio::midi::Parser::default();
                let mut buf = [0u8; 256];
                // Ticks that queued up before we listened would all carry
                // this moment's timestamp: throw them away.
                while matches!(port.read(&mut buf, 0), Ok(n) if n > 0) {}
                while !shared.stop.load(Ordering::Relaxed) {
                    let one = live.set_one_midi.swap(0, Ordering::Relaxed);
                    if one != 0 && f.set_one(f64::from_bits(one)) {
                        live.publish_midi_in(f.grid());
                    }
                    match port.read(&mut buf, 20) {
                        Ok(0) => {
                            if f.idle(rtio::now()) {
                                live.publish_midi_in(f.grid());
                            }
                        }
                        Ok(n) => {
                            let t = rtio::now();
                            let mut changed = false;
                            let mut msgs = 0;
                            let mut sent = 0;
                            for &b in &buf[..n] {
                                let c = match b {
                                    rtio::midi::CLOCK => f.tick(t),
                                    rtio::midi::START => f.start(t),
                                    rtio::midi::CONTINUE => f.cont(t),
                                    rtio::midi::STOP => f.stop(t),
                                    _ => {
                                        if let (Some(out), Some(msg)) = (&thru, parser.push(b)) {
                                            sent += u64::from(out.send(msg).is_ok());
                                        }
                                        continue;
                                    }
                                };
                                msgs += 1;
                                changed |= c;
                            }
                            if msgs > 0 {
                                shared.midi_in_msgs.fetch_add(msgs, Ordering::Relaxed);
                            }
                            if sent > 0 {
                                shared.midi_thru.fetch_add(sent, Ordering::Relaxed);
                            }
                            if changed {
                                let old = **live.midi_in_grid.load();
                                let g = f.grid();
                                if old.running && g.running && old.epoch == g.epoch {
                                    let shift = (g.beat_time(g.n0) - old.beat_time(g.n0)).abs();
                                    shared.midi_in_shift_us.fetch_max((shift * 1e6) as u32, Ordering::Relaxed);
                                }
                                shared.midi_in_jitter_us.store((f.residual_s() * 1e6) as u32, Ordering::Relaxed);
                                live.publish_midi_in(g);
                            }
                        }
                        Err(e) => {
                            eprintln!("midi in: {e:#}; no more external clock");
                            break;
                        }
                    }
                }
            })
            .expect("spawn midi-in thread")
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
                    if live.adopt.swap(false, Ordering::Relaxed) {
                        tr.adopt(**live.grid.load(), rtio::now());
                        live.publish_tracker(tr.grid());
                    }
                    let one = live.set_one.swap(0, Ordering::Relaxed);
                    if one != 0 && tr.set_downbeat(f64::from_bits(one)) {
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
                        shared.bias_us.store((tr.onset_bias_s() * 1e6) as i32, Ordering::Relaxed);
                        shared.holding.store(tr.holding(), Ordering::Relaxed);
                        shared.spread_milli.store((tr.onset_spread() * 1000.0) as u32, Ordering::Relaxed);
                    }
                    thread::park_timeout(Duration::from_millis(2));
                }
            })
            .expect("spawn tracker thread")
    };
    let tracker_handle = tracker_thread.thread().clone();
    let tracker_handle_main = tracker_handle.clone();

    // --- Key thread ---------------------------------------------------------
    // Ordinary priority: a result that is right after a few seconds is all
    // that matters. Publishes the detector's estimate whenever it changes.
    let key_thread = key_on.then(|| {
        let shared = shared.clone();
        let est = key_est.clone();
        let kcfg = cfg.key();
        thread::Builder::new()
            .name("key".into())
            .spawn(move || {
                let mut det = key::KeyDetector::new(kcfg);
                let mut buf = vec![0f32; 4096];
                while !shared.stop.load(Ordering::Relaxed) {
                    loop {
                        let n = kcons.pop_slice(&mut buf);
                        if n == 0 {
                            break;
                        }
                        det.process(&buf[..n], |_| {});
                    }
                    let e = det.estimate();
                    if e != **est.load() {
                        est.store(Arc::new(e));
                    }
                    thread::park_timeout(Duration::from_millis(50));
                }
            })
            .expect("spawn key thread")
    });
    let key_handle = key_thread.as_ref().map(|t| t.thread().clone());

    // --- Audio thread ------------------------------------------------------
    let audio_thread = duplex.map(|mut duplex| {
        let live = live.clone();
        let shared = shared.clone();
        let ocfg = cfg.onset();
        let ccfg = cfg.click();
        let onset_offset = cfg.tempo.onset_offset_ms / 1000.0;
        let mic_channel = cfg.audio.mic_channel;
        let key_channel = if cfg.audio.key_channel >= 0 { cfg.audio.key_channel as usize } else { mic_channel };
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
                let key_ch = key_channel % cap_ch;
                let click_ch: Vec<usize> = click_channels.iter().map(|&c| c % play_ch).collect();
                let mut det = onset::OnsetDetector::new(ocfg);
                let hop_dt = det.hop_dt();
                let mut synth = click::ClickSynth::new(ccfg);
                let mut inbuf = vec![0f32; period * cap_ch];
                let mut mono = vec![0f32; period];
                let mut key_mono = vec![0f32; period];
                let mut click_mono = vec![0f32; period];
                let mut outbuf = vec![0f32; period * play_ch];
                let mut total: u64 = 0;
                // Peak hold for the level readout: one 1 ms period every 5 s
                // misses most peaks, so the loudest moment is held and decays.
                let mut hold = 0f32;
                let hold_decay = (-(period as f64 / fs) / LEVEL_HOLD_S).exp() as f32;
                let clip_hold_periods = (LEVEL_HOLD_S * fs / period as f64) as u32;
                let mut clip_periods = 0u32;
                let mut hold_slow = 0f32;
                let slow_decay = (-(period as f64 / fs) / GAIN_HOLD_S).exp() as f32;
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
                    let mut clipped = 0u64;
                    for (i, m) in mono.iter_mut().enumerate() {
                        *m = inbuf[i * cap_ch + mic];
                        let a = m.abs();
                        peak = peak.max(a);
                        clipped += u64::from(a >= CLIP_LEVEL);
                    }
                    hold = if peak >= hold { peak } else { hold * hold_decay };
                    let db = 20.0 * (hold + 1e-9).log10();
                    shared.level_milli_db.store((-db * 1000.0).max(0.0) as u32, Ordering::Relaxed);
                    hold_slow = if peak >= hold_slow { peak } else { hold_slow * slow_decay };
                    let db_slow = 20.0 * (hold_slow + 1e-9).log10();
                    shared.peak_slow_milli_db.store((-db_slow * 1000.0).max(0.0) as u32, Ordering::Relaxed);
                    if clipped > 0 {
                        shared.clipped.fetch_add(clipped, Ordering::Relaxed);
                        clip_periods = clip_hold_periods;
                    } else {
                        clip_periods = clip_periods.saturating_sub(1);
                    }
                    shared.clipping.store(clip_periods > 0, Ordering::Relaxed);
                    // the key detector gets a copy of its channel; nothing else here
                    if let Some(kh) = &key_handle {
                        let src: &[f32] = if key_ch == mic {
                            &mono
                        } else {
                            for (i, k) in key_mono.iter_mut().enumerate() {
                                *k = inbuf[i * cap_ch + key_ch];
                            }
                            &key_mono
                        };
                        let pushed = kprod.push_slice(src);
                        if pushed < src.len() {
                            shared.key_overflow.fetch_add((src.len() - pushed) as u64, Ordering::Relaxed);
                        }
                        kh.unpark();
                    }
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
        let key_est = key_est.clone();
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
                            clipping: shared.clipping.load(Ordering::Relaxed),
                            peak_db: -(shared.peak_slow_milli_db.load(Ordering::Relaxed) as f32) / 1000.0,
                            midi_in: **live.midi_in_grid.load(),
                            xruns: shared.xruns.load(Ordering::Relaxed),
                            ticks: stats.ticks.load(Ordering::Relaxed),
                            midi: midi_on,
                            taps: manual.borrow().taps(),
                            now: rtio::now(),
                            seq: seq.borrow().clone(),
                            key: **key_est.load(),
                            bpm_shown: None,
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
                            Action::SetMode(Mode::MidiIn) => {
                                if live.mode() != Mode::MidiIn {
                                    live.set_mode(Mode::MidiIn);
                                }
                            }
                            Action::Tap(t) => {
                                manual.tap(t);
                                live.publish_manual(manual.grid());
                            }
                            Action::SetOne(t) => match live.mode() {
                                Mode::Manual => {
                                    if manual.set_one(t) {
                                        live.publish_manual(manual.grid());
                                    }
                                }
                                Mode::Auto => live.set_one.store(t.to_bits(), Ordering::Relaxed),
                                Mode::MidiIn => live.set_one_midi.store(t.to_bits(), Ordering::Relaxed),
                            },
                            Action::Nudge(d) => {
                                manual.nudge_bpm(d, now);
                                live.publish_manual(manual.grid());
                            }
                            Action::ToggleRun => {
                                manual.toggle(now);
                                live.publish_manual(manual.grid());
                            }
                            Action::Generate(inst, seed) => {
                                let heard = key_est.load_full();
                                let k = ui::effective_key(&seq.borrow().key, (*heard).as_ref());
                                if let Some(k) = k {
                                    let mut arc = seq.borrow_mut();
                                    if Arc::make_mut(&mut arc).apply(sequencer::SeqAction::Generate(inst, k, seed)) {
                                        seq_snapshot.store(Arc::new(arc.snapshot()));
                                        seq_dirty.set(Some(now));
                                    }
                                }
                            }
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
            Mode::MidiIn => "midi",
        };
        let level = -(shared.level_milli_db.load(Ordering::Relaxed) as f32) / 1000.0;
        let key_s = match **key_est.load() {
            Some(e) if e.tuning_cents.abs() >= 5.0 => format!("{} {:.2} ({:+.0}c)", e.short_name(), e.confidence, e.tuning_cents),
            Some(e) => format!("{} {:.2}", e.short_name(), e.confidence),
            None => "--".to_string(),
        };
        let clipped = shared.clipped.load(Ordering::Relaxed);
        let clip = if clipped > 0 { format!("  CLIP {clipped}") } else { String::new() };
        let hold = if shared.holding.load(Ordering::Relaxed) { " HOLD" } else { "" };
        let spread = shared.spread_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        let ext = {
            let g = **live.midi_in_grid.load();
            let n = shared.midi_in_msgs.load(Ordering::Relaxed);
            let shift = shared.midi_in_shift_us.swap(0, Ordering::Relaxed) as f64 / 1000.0;
            let jit = shared.midi_in_jitter_us.load(Ordering::Relaxed) as f64 / 1000.0;
            // The master's last transport message and how many it sent.
            let cmd = match g.transport {
                Some(tracker::Transport { seq, last: Some(c), .. }) => format!(" {c:?} #{seq}"),
                _ => String::new(),
            };
            match g.state {
                State::Idle if n == 0 => String::new(),
                State::Running => format!("  ext {:.1} bpm jit {jit:.1} shift {shift:.2} ms{cmd}", g.bpm()),
                _ => format!("  ext stopped{cmd}"),
            }
        };
        let line = format!(
            "{mode:6} {state:8} {:6.1} bpm  conf {:.2}  tempo-str {:.2}  spread {spread:.2}{hold}  bias {:+.0} ms{ext}  in {level:5.1} dBFS{clip}  key {key_s}  onsets {}  clicks {}  ticks {}  starts {}  stops {}  thru {}  xruns {}",
            g.bpm(),
            g.confidence,
            shared.tempo_strength_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            shared.bias_us.load(Ordering::Relaxed) as f64 / 1000.0,
            shared.onsets.load(Ordering::Relaxed),
            shared.clicks.load(Ordering::Relaxed),
            clock_stats.ticks.load(Ordering::Relaxed),
            clock_stats.starts.load(Ordering::Relaxed),
            clock_stats.stops.load(Ordering::Relaxed),
            shared.midi_thru.load(Ordering::Relaxed),
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
    if let Some(t) = key_thread {
        t.thread().unpark();
        let _ = t.join();
        let lost = shared.key_overflow.load(Ordering::Relaxed);
        if lost > 0 {
            eprintln!("key: {lost} samples dropped (detector fell behind)");
        }
    }
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
    if let Some(t) = midi_in_thread {
        let _ = t.join();
        eprintln!(
            "midi in: {} realtime messages, {} others sent on",
            shared.midi_in_msgs.load(Ordering::Relaxed),
            shared.midi_thru.load(Ordering::Relaxed)
        );
    }
    eprintln!("xruns: {}", shared.xruns.load(Ordering::Relaxed));
    if shared.audio_error.load(Ordering::Relaxed) {
        anyhow::bail!("audio thread stopped with an error");
    }
    Ok(())
}
