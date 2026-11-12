//! Run the touch UI in a desktop window. The mouse is the finger, a virtual
//! drummer feeds the real tracker so AUTO mode behaves like on stage.
//!
//! With `--midi virtual` the real clock and sequencer threads send MIDI
//! clock and notes to an ALSA sequencer port that other programs on the same
//! machine (Bitwig, `aseqdump`) can subscribe to.
//!
//! Keys:  D  drummer on/off      [ ]  drummer tempo -/+ 2 BPM
//!        J  toggle 15 ms timing jitter     Q / Esc  quit
//!        K  cycle the key the (imaginary) band plays in: none, C major,
//!           A minor, a doubtful G, a hopeless F# minor

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use rtio::MidiOut;
use sequencer::{NoteOut, SeqState, SeqStats};
use tracker::{Grid, HopInput, ManualClock, ManualConfig, Tracker, TrackerConfig};
use ui::{Action, Canvas, Mode, PixelFormat, Rotation, Session, Status, TouchEvent, UiConfig};

/// What the key detector would hear from a band: `K` steps through these.
const HEARD_KEYS: [Option<(u8, key::Mode, f32)>; 5] = [None, Some((0, key::Mode::Major, 0.85)), Some((9, key::Mode::Minor, 0.7)), Some((7, key::Mode::Major, 0.45)), Some((6, key::Mode::Minor, 0.15))];

fn heard(k: Option<(u8, key::Mode, f32)>) -> Option<key::KeyEstimate> {
    let (tonic, mode, confidence) = k?;
    let scale = match mode {
        key::Mode::Major => sequencer::Scale::Major,
        key::Mode::Minor => sequencer::Scale::Minor,
    };
    let key = sequencer::Key::new(tonic, scale);
    let mut chroma = [0.15f32; 12];
    for (pc, c) in chroma.iter_mut().enumerate() {
        if key.contains(pc as u8) {
            *c = 0.55;
        }
    }
    chroma[tonic as usize] = 1.0;
    chroma[((tonic + 7) % 12) as usize] = 0.8;
    let alt = match mode {
        key::Mode::Major if confidence < 0.6 => Some(((tonic + 9) % 12, key::Mode::Minor)),
        _ => None,
    };
    Some(key::KeyEstimate { tonic, mode, confidence, tuning_cents: 7.0, chroma, alt, frames: 1 })
}

/// Notes go to the MIDI port if there is one, and to stderr if asked.
struct SimNotes {
    midi: Option<Arc<MidiOut>>,
    print: bool,
}
impl NoteOut for SimNotes {
    fn note_on(&self, channel: u8, note: u8, vel: u8) -> bool {
        if self.print {
            eprintln!("note on  ch{:2} {:>4} vel {vel}", channel + 1, sequencer::note_name(note));
        }
        self.midi.as_ref().is_none_or(|m| m.note_on(channel, note, vel).is_ok())
    }
    fn note_off(&self, channel: u8, note: u8) -> bool {
        self.midi.as_ref().is_none_or(|m| m.note_off(channel, note).is_ok())
    }
}

#[derive(Parser, Debug)]
#[command(about = "Desktop simulator for the midi-clock touch UI")]
struct Args {
    /// Screen size, WIDTHxHEIGHT (the Joy-it RB-LCD-10-2 is 1280x800).
    #[arg(long, default_value = "1280x800")]
    size: String,
    /// Screen rotation in degrees as in the config (0, 90, 180, 270).
    #[arg(long, default_value_t = 0)]
    rotation: u32,
    /// Start in this mode.
    #[arg(long, default_value = "auto")]
    mode: String,
    /// Virtual drummer tempo.
    #[arg(long, default_value_t = 118.0)]
    drummer_bpm: f64,
    /// Start with the drummer playing.
    #[arg(long)]
    drummer: bool,
    /// Pattern file to load and save (default: patterns.toml in the current directory).
    #[arg(long, default_value = "patterns.toml")]
    patterns: std::path::PathBuf,
    /// Print sequencer notes to stderr as they are sent.
    #[arg(long)]
    print_notes: bool,
    /// Send MIDI clock and sequencer notes for real: `virtual` creates an ALSA
    /// sequencer port "midi-clock:out" that Bitwig etc. can connect to
    /// (`virtual:NAME` picks the client name), `hw:...` uses a rawmidi port.
    #[arg(long)]
    midi: Option<String>,
    /// With `--midi virtual`: connect the port to this ALSA sequencer input at
    /// start, like `aconnect`. A client name or part of it (`virmidi`,
    /// `"Virtual Raw MIDI 1-0"`), or `CLIENT:PORT` numbers.
    #[arg(long)]
    connect: Vec<String>,
}

/// A drummer that hits every beat, feeding hops to the tracker like the audio thread does.
struct Drummer {
    playing: bool,
    bpm: f64,
    jitter: bool,
    next_beat: f64,
    hop_time: f64,
    seed: u64,
}

impl Drummer {
    fn rand(&mut self) -> f64 {
        self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.seed >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Generate hops up to `now`.
    fn advance(&mut self, now: f64, tr: &mut Tracker, hop_dt: f64) {
        while self.hop_time + hop_dt <= now {
            self.hop_time += hop_dt;
            let t = self.hop_time;
            let mut odf = 0.3 + self.rand() as f32 * 0.2;
            let mut onset = None;
            if self.playing {
                if t >= self.next_beat {
                    let strength = 6.0 + self.rand() as f32 * 3.0;
                    odf = strength;
                    onset = Some((self.next_beat, strength));
                    let j = if self.jitter { (self.rand() - 0.5) * 0.03 } else { 0.0 };
                    self.next_beat += 60.0 / self.bpm + j;
                }
            } else {
                self.next_beat = t + 0.1;
            }
            tr.process_hop(HopInput { time: t, odf, kick: odf, onset });
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (w, h) = args
        .size
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .context("--size must be WIDTHxHEIGHT")?;
    let rotation = Rotation::from_degrees(args.rotation).context("rotation must be 0/90/180/270")?;
    let ucfg = UiConfig { rotation: args.rotation, tap_offset_s: 0.0, ..UiConfig::default() };
    // The window shows the physical panel; the canvas handles rotation.
    let canvas = Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, rotation);
    let mut session = Session::new(&ucfg, canvas);
    let mut window = Window::new("midi-clock UI simulator", w, h, WindowOptions::default())?;
    window.set_target_fps(60);

    let mut mode = if args.mode.eq_ignore_ascii_case("manual") { Mode::Manual } else { Mode::Auto };
    let tcfg = TrackerConfig::default();
    let hop_dt = tcfg.hop_dt;
    let mut tracker = Tracker::new(tcfg);
    let mut manual = ManualClock::new(ManualConfig::default(), 120.0);
    let t0 = rtio::now();
    let mut drummer = Drummer { playing: args.drummer, bpm: args.drummer_bpm, jitter: false, next_beat: t0 + 1.0, hop_time: t0, seed: 42 };
    let live_shared = Arc::new(Mutex::new(if mode == Mode::Manual { manual.grid() } else { tracker.grid() }));
    let mut live: Grid = *live_shared.lock().unwrap();
    // Real MIDI output when asked for; the clock thread is the one from the app.
    let midi_out = match &args.midi {
        Some(name) => {
            let m = Arc::new(MidiOut::open(name)?);
            eprintln!("midi: {}", m.name());
            for dest in &args.connect {
                m.connect(dest)?;
                eprintln!("midi: connected to {dest}");
            }
            Some(m)
        }
        None => None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let clock_stats = Arc::new(midiclock::ClockStats::default());
    let clock_thread = midi_out.clone().map(|midi| {
        let live = live_shared.clone();
        let stop = stop.clone();
        let stats = clock_stats.clone();
        let ccfg = midiclock::ClockConfig { lead_s: 0.0, send_start_stop: true };
        std::thread::spawn(move || midiclock::run(|| *live.lock().unwrap(), &*midi, &ccfg, &stop, &stats))
    });
    // Sequencer: real thread, notes to the MIDI port and/or stderr.
    let seq_path = args.patterns.clone();
    let mut seq = Arc::new(if seq_path.exists() { SeqState::load(&seq_path).unwrap_or_default() } else { SeqState::default() });
    let mut seq_dirty: Option<f64> = None;
    let seq_snapshot = Arc::new(ArcSwap::from_pointee(seq.snapshot()));
    let seq_thread = {
        let live = live_shared.clone();
        let snap = seq_snapshot.clone();
        let stop = stop.clone();
        let out = SimNotes { midi: midi_out.clone(), print: args.print_notes };
        std::thread::spawn(move || {
            let stats = SeqStats::default();
            let get_grid = || *live.lock().unwrap();
            sequencer::run(get_grid, || snap.load_full(), &out, 0.0, &stop, &stats);
        })
    };
    let mut ticks = 0u64;
    let mut last_tick_beat = i64::MIN;
    let mut pixels = vec![0u32; w * h];
    let mut mouse_down = false;
    let mut d_held = false;
    let mut j_held = false;
    let mut k_held = false;
    let mut heard_i = 0usize;
    let mut lb_held = false;
    let mut rb_held = false;

    while window.is_open() && !window.is_key_down(Key::Escape) && !window.is_key_down(Key::Q) {
        let now = rtio::now();
        // --- virtual drummer -> real tracker -----------------------------------
        drummer.advance(now, &mut tracker, hop_dt);
        // --- keys -------------------------------------------------------------------
        let key_edge = |win: &Window, k: Key, held: &mut bool| -> bool {
            let down = win.is_key_down(k);
            let edge = down && !*held;
            *held = down;
            edge
        };
        if key_edge(&window, Key::D, &mut d_held) {
            drummer.playing = !drummer.playing;
            eprintln!("drummer {}", if drummer.playing { "playing" } else { "silent" });
        }
        if key_edge(&window, Key::J, &mut j_held) {
            drummer.jitter = !drummer.jitter;
            eprintln!("jitter {}", drummer.jitter);
        }
        if key_edge(&window, Key::K, &mut k_held) {
            heard_i = (heard_i + 1) % HEARD_KEYS.len();
            match heard(HEARD_KEYS[heard_i]) {
                Some(e) => eprintln!("band plays in {} (confidence {:.2})", e.short_name(), e.confidence),
                None => eprintln!("band plays nothing tonal"),
            }
        }
        let heard_key = heard(HEARD_KEYS[heard_i]);
        if key_edge(&window, Key::LeftBracket, &mut lb_held) {
            drummer.bpm -= 2.0;
            eprintln!("drummer {:.0} bpm", drummer.bpm);
        }
        if key_edge(&window, Key::RightBracket, &mut rb_held) {
            drummer.bpm += 2.0;
            eprintln!("drummer {:.0} bpm", drummer.bpm);
        }
        // --- mouse as finger ------------------------------------------------------------
        let down = window.get_mouse_down(MouseButton::Left);
        if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
            let (x, y) = (mx as i32, my as i32);
            let ev = if down && !mouse_down {
                Some(TouchEvent::Down { x, y, t: now })
            } else if down {
                Some(TouchEvent::Move { x, y, t: now })
            } else if mouse_down {
                Some(TouchEvent::Up { t: now })
            } else {
                None
            };
            mouse_down = down;
            if let Some(ev) = ev {
                let cur = mode;
                let mut act = |a: Action| apply(a, &mut mode, &mut tracker, &mut manual, &mut live, &mut seq, &seq_snapshot, &mut seq_dirty, heard_key, now);
                session.touch(ev, cur, &mut act);
            }
        }
        {
            let mut act = |a: Action| apply(a, &mut mode, &mut tracker, &mut manual, &mut live, &mut seq, &seq_snapshot, &mut seq_dirty, heard_key, now);
            session.tick(now, &mut act);
        }
        if mode == Mode::Auto {
            live = tracker.grid();
        }
        *live_shared.lock().unwrap() = live;
        if let Some(t) = seq_dirty {
            if now - t > 1.0 {
                seq_dirty = None;
                if let Err(e) = seq.save(&seq_path) {
                    eprintln!("save failed: {e}");
                }
            }
        }
        // Fake MIDI tick counter (24 per beat while running) when there is no real clock thread.
        if midi_out.is_none() && live.running {
            let n = live.first_beat_at_or_after(now) - 1;
            if n != last_tick_beat {
                last_tick_beat = n;
                ticks += 24;
            }
        }
        // --- frame -------------------------------------------------------------------
        if session.frame_due(now) {
            let level = if drummer.playing { -14.0 + (drummer.rand() as f32) * 6.0 } else { -70.0 };
            let ticks = if midi_out.is_some() { clock_stats.ticks.load(Ordering::Relaxed) } else { ticks };
            let st = Status {
                mode,
                live,
                tracker: tracker.grid(),
                tempo_strength: tracker.last_tempo_strength as f32,
                level_db: level,
                clipping: false,
                midi_in: Grid::default(),
                peak_db: level,
                xruns: 0,
                ticks,
                midi: true,
                taps: manual.taps(),
                now,
                seq: seq.clone(),
                key: heard_key,
                bpm_shown: None,
            };
            let mut act = |a: Action| apply(a, &mut mode, &mut tracker, &mut manual, &mut live, &mut seq, &seq_snapshot, &mut seq_dirty, heard_key, now);
            if session.frame(now, &st, &mut act) {
                for (p, b) in pixels.iter_mut().zip(session.canvas().bytes().chunks_exact(4)) {
                    *p = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
            }
        }
        window.update_with_buffer(&pixels, w, h)?;
        std::thread::sleep(Duration::from_millis(4));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = seq_thread.join();
    if let Some(t) = clock_thread {
        let _ = t.join();
        eprintln!("midi clock: {}", clock_stats.summary());
    }
    Ok(())
}

/// The same mode/ownership rules as the application, without threads.
#[allow(clippy::too_many_arguments)]
fn apply(
    a: Action,
    mode: &mut Mode,
    tracker: &mut Tracker,
    manual: &mut ManualClock,
    live: &mut Grid,
    seq: &mut Arc<SeqState>,
    seq_snapshot: &ArcSwap<sequencer::Snapshot>,
    seq_dirty: &mut Option<f64>,
    heard: Option<key::KeyEstimate>,
    now: f64,
) {
    match a {
        Action::Seq(sa) => {
            if Arc::make_mut(seq).apply(sa) && !sa.view_only() {
                seq_snapshot.store(Arc::new(seq.snapshot()));
                *seq_dirty = Some(now);
            }
        }
        Action::Generate(inst, style, seed) => {
            if let Some(k) = ui::effective_key(&seq.key, heard.as_ref()) {
                if Arc::make_mut(seq).apply(sequencer::SeqAction::Generate(inst, style, k, seed)) {
                    seq_snapshot.store(Arc::new(seq.snapshot()));
                    *seq_dirty = Some(now);
                }
            }
        }
        Action::SetMode(Mode::Manual) => {
            if *mode != Mode::Manual {
                *mode = Mode::Manual;
                manual.adopt(live);
                *live = manual.grid();
            }
        }
        Action::SetMode(Mode::Auto) => {
            if *mode != Mode::Auto {
                *mode = Mode::Auto;
                tracker.adopt(*live, now);
                *live = tracker.grid();
            }
        }
        Action::SetMode(Mode::MidiIn) => {
            // The simulator has no MIDI input: the external clock is absent.
            if *mode != Mode::MidiIn {
                *mode = Mode::MidiIn;
                *live = Grid::default();
            }
        }
        Action::Tap(t) => {
            manual.tap(t);
            *live = manual.grid();
        }
        Action::SetOne(t) => {
            if *mode == Mode::Manual {
                if manual.set_one(t) {
                    *live = manual.grid();
                }
            } else if *mode == Mode::Auto && tracker.set_downbeat(t) {
                *live = tracker.grid();
            }
        }
        Action::Nudge(d) => {
            manual.nudge_bpm(d, now);
            *live = manual.grid();
        }
        Action::ToggleRun => {
            manual.toggle(now);
            *live = manual.grid();
        }
    }
}
