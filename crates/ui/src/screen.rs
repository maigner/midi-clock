//! Layout, rendering and hit testing. Six pages share a top bar: TEMPO
//! (the clock), KEY (the key of the music, heard or set), LEAD (the
//! polyphonic step sequencer), BASS (the single-voice bass line), DRUMS
//! (the pad-and-step-row drum kit) and SETUP (the MIDI channel of every
//! instrument). Everything is placed relative to the display size so
//! 800x480 and 1280x800 both work.

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Arc, Circle, CornerRadii, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle, Triangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};

use sequencer::{cover_map, drum_name, note_name, phrase_steps, BassParam, DrumParam, Instrument, Key, KeySetup, Param, Scale, SeqState, Style, BASS_ROWS, BASS_TOP, DRUMS, PITCH_NAMES, STEPS, TRACKS};
use tracker::{Grid, State};

use crate::canvas::{Canvas, Scaled};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Tempo,
    Key,
    Lead,
    Bass,
    Drums,
    Settings,
}

/// Snapshot of everything the screen shows. Built by the application.
#[derive(Clone, Debug)]
pub struct Status {
    pub mode: Mode,
    /// The grid the click and MIDI clock are using.
    pub live: Grid,
    /// What the tracker hears (also in manual mode).
    pub tracker: Grid,
    pub tempo_strength: f32,
    pub level_db: f32,
    pub xruns: u64,
    pub ticks: u64,
    pub midi: bool,
    pub taps: usize,
    pub now: f64,
    pub seq: std::sync::Arc<SeqState>,
    /// What the key detector hears, if anything.
    pub key: Option<key::KeyEstimate>,
}

/// The key in effect: the detector's, while it is confident enough, when
/// the KEY page is on AUTO; the one set by hand otherwise.
pub fn effective_key(setup: &KeySetup, detected: Option<&key::KeyEstimate>) -> Option<Key> {
    if setup.auto {
        detected.filter(|e| e.confidence >= KEY_MIN_CONFIDENCE).map(|e| Key::new(e.tonic, scale_of(e.mode)))
    } else {
        Some(setup.key)
    }
}

/// Below this confidence a heard key is shown dimmed and not used.
pub const KEY_MIN_CONFIDENCE: f32 = 0.3;

fn scale_of(mode: key::Mode) -> Scale {
    match mode {
        key::Mode::Major => Scale::Major,
        key::Mode::Minor => Scale::Minor,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    PageTempo,
    PageKey,
    PageLead,
    PageBass,
    PageDrums,
    PageSettings,
    ModeAuto,
    ModeManual,
    Minus5,
    Minus1,
    Tap,
    Plus1,
    Plus5,
    StartStop,
    // Key page
    KeyAuto,
    KeyManual,
    /// The tonic set by hand, 0 = C ... 11 = B.
    KeyRoot(u8),
    KeyScale(Scale),
    // Lead and bass pages
    /// Write a new phrase in the key.
    Generate(Instrument, Style),
    // Lead page
    /// Phrase length in bars.
    Bars(u8),
    /// Bar shown for editing.
    Bar(usize),
    /// Keep the shown bar instead of following the clock.
    BarLock,
    ParamMinus(Param),
    ParamPlus(Param),
    // Bass page
    /// Phrase length in bars.
    BassBars(u8),
    /// Bar shown for editing.
    BassBar(usize),
    BassBarLock,
    BassMinus(BassParam),
    BassPlus(BassParam),
    /// Scroll the pitch window by this many semitones (+1 = up).
    BassScroll(i32),
    // Drums page
    DrumBars(u8),
    DrumBar(usize),
    DrumBarLock,
    DrumMinus(DrumParam),
    DrumPlus(DrumParam),
    /// Instrument pad: select and play.
    DrumPad(usize),
    /// One of the 16 steps of the selected instrument in the shown bar.
    DrumStep(usize),
    // Settings page
    /// An instrument's MIDI channel, one down / one up.
    ChannelMinus(Instrument),
    ChannelPlus(Instrument),
}

impl Button {
    /// Whether the button is on screen at all: the tempo controls only in
    /// MANUAL, the key controls only while the key is set by hand.
    pub fn visible(self, mode: Mode, key_auto: bool) -> bool {
        match self {
            Button::Minus5 | Button::Minus1 | Button::Tap | Button::Plus1 | Button::Plus5 | Button::StartStop => mode == Mode::Manual,
            Button::KeyRoot(_) | Button::KeyScale(_) => !key_auto,
            _ => true,
        }
    }

    /// Whether a press does something: visible, and for the generators a
    /// key is known.
    pub fn enabled(self, mode: Mode, key_auto: bool, key_known: bool) -> bool {
        self.visible(mode, key_auto) && !matches!(self, Button::Generate(..) if !key_known)
    }

    pub fn repeats(self) -> bool {
        matches!(
            self,
            Button::Minus5
                | Button::Minus1
                | Button::Plus1
                | Button::Plus5
                | Button::ParamMinus(_)
                | Button::ParamPlus(_)
                | Button::BassMinus(_)
                | Button::BassPlus(_)
                | Button::BassScroll(_)
                | Button::DrumMinus(_)
                | Button::DrumPlus(_)
                | Button::ChannelMinus(_)
                | Button::ChannelPlus(_)
        )
    }

    fn page(self) -> Option<Page> {
        match self {
            Button::PageTempo | Button::PageKey | Button::PageLead | Button::PageBass | Button::PageDrums | Button::PageSettings => None,
            Button::ModeAuto | Button::ModeManual | Button::Minus5 | Button::Minus1 | Button::Tap | Button::Plus1 | Button::Plus5 | Button::StartStop => {
                Some(Page::Tempo)
            }
            Button::KeyAuto | Button::KeyManual | Button::KeyRoot(_) | Button::KeyScale(_) => Some(Page::Key),
            Button::Generate(Instrument::Lead, _) => Some(Page::Lead),
            Button::Generate(Instrument::Bass, _) => Some(Page::Bass),
            Button::Generate(Instrument::Drums, _) => Some(Page::Drums),
            Button::Bars(_) | Button::Bar(_) | Button::BarLock | Button::ParamMinus(_) | Button::ParamPlus(_) => Some(Page::Lead),
            Button::BassBars(_) | Button::BassBar(_) | Button::BassBarLock | Button::BassMinus(_) | Button::BassPlus(_) | Button::BassScroll(_) => {
                Some(Page::Bass)
            }
            Button::DrumBars(_)
            | Button::DrumBar(_)
            | Button::DrumBarLock
            | Button::DrumMinus(_)
            | Button::DrumPlus(_)
            | Button::DrumPad(_)
            | Button::DrumStep(_) => Some(Page::Drums),
            Button::ChannelMinus(_) | Button::ChannelPlus(_) => Some(Page::Settings),
        }
    }
}

/// What a touch landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Button(Button),
    /// Grid cell (track, step).
    Cell(usize, usize),
    Track(usize),
    /// Bass pitch grid cell (row from the bottom, step).
    BassCell(usize, usize),
    /// The note names left of the bass grid: drag to scroll the pitch window.
    BassLabels,
}

const BG: Rgb888 = Rgb888::new(14, 17, 22);
const PANEL: Rgb888 = Rgb888::new(30, 36, 46);
const PANEL_BEAT: Rgb888 = Rgb888::new(40, 48, 60);
const PANEL_PRESSED: Rgb888 = Rgb888::new(70, 80, 96);
const PANEL_DISABLED: Rgb888 = Rgb888::new(22, 26, 32);
const TEXT: Rgb888 = Rgb888::new(225, 228, 232);
const DIM: Rgb888 = Rgb888::new(105, 115, 128);
const AUTO: Rgb888 = Rgb888::new(70, 190, 230);
const MANUAL: Rgb888 = Rgb888::new(240, 150, 50);
const GREEN: Rgb888 = Rgb888::new(80, 200, 110);
const YELLOW: Rgb888 = Rgb888::new(230, 200, 60);
const RED: Rgb888 = Rgb888::new(230, 80, 70);
const BASS: Rgb888 = Rgb888::new(180, 120, 240);
const TRACK_COLORS: [Rgb888; TRACKS] = [
    Rgb888::new(240, 150, 50),
    Rgb888::new(70, 190, 230),
    Rgb888::new(80, 200, 110),
    Rgb888::new(230, 200, 60),
    Rgb888::new(230, 80, 70),
    Rgb888::new(180, 120, 240),
    Rgb888::new(240, 120, 180),
    Rgb888::new(120, 200, 200),
];
/// One colour per pad of the drum kit.
const DRUM_COLORS: [Rgb888; DRUMS] = [
    Rgb888::new(240, 150, 50),
    Rgb888::new(230, 80, 70),
    Rgb888::new(240, 120, 180),
    Rgb888::new(230, 200, 60),
    Rgb888::new(80, 200, 110),
    Rgb888::new(120, 200, 200),
    Rgb888::new(70, 190, 230),
    Rgb888::new(110, 140, 240),
    Rgb888::new(180, 120, 240),
    Rgb888::new(205, 170, 130),
];
/// The -/+ parameter groups along the bottom of the sequence, bass and drums pages.
const SEQ_PARAMS: [Param; 2] = [Param::Note, Param::Velocity];
const BASS_PARAMS: [BassParam; 1] = [BassParam::Velocity];
const DRUM_PARAMS: [DrumParam; 3] = [DrumParam::Note, DrumParam::Velocity, DrumParam::Swing];
/// Groups along the bottom of the lead and bass pages: the parameters and,
/// last, the RANDOM / CNTRPT pair.
const SEQ_GROUPS: usize = SEQ_PARAMS.len() + 1;
const BASS_GROUPS: usize = BASS_PARAMS.len() + 1;
/// Pads per row.
const PAD_COLS: usize = DRUMS / 2;
/// The page tabs of the top bar, with a short label for when the long ones
/// do not fit the width.
const TABS: [(Button, &str, &str); 6] = [
    (Button::PageTempo, "TEMPO", "TMP"),
    (Button::PageKey, "KEY", "KEY"),
    (Button::PageLead, "LEAD", "LD"),
    (Button::PageBass, "BASS", "BS"),
    (Button::PageDrums, "DRUMS", "DR"),
    (Button::PageSettings, "SETUP", "SET"),
];
/// Scales per row of the KEY page: the seven modes, then the rest.
const SCALE_ROWS: [&[Scale]; 2] = [
    &[Scale::Major, Scale::Dorian, Scale::Phrygian, Scale::Lydian, Scale::Mixolydian, Scale::Minor, Scale::Locrian],
    &[Scale::HarmonicMinor, Scale::MelodicMinor, Scale::PentatonicMajor, Scale::PentatonicMinor, Scale::Blues],
];

/// Width of one of `n` parameter groups across `w`, `m` apart and from the edges.
fn param_group_w(w: i32, m: i32, n: usize) -> i32 {
    (w - (n as i32 + 1) * m) / n as i32
}

/// A darker shade of a colour: the steps a note rings on after its head.
fn dim(c: Rgb888) -> Rgb888 {
    let d = |v: u8| (v as u16 * 3 / 5) as u8;
    Rgb888::new(d(c.r()), d(c.g()), d(c.b()))
}

/// The colour of a note at a velocity: full at 127, a third as bright at 1.
fn shade(c: Rgb888, vel: u8) -> Rgb888 {
    let f = 0.35 + 0.65 * (vel.min(127) as f32 / 127.0);
    let d = |v: u8| (v as f32 * f).round().clamp(0.0, 255.0) as u8;
    Rgb888::new(d(c.r()), d(c.g()), d(c.b()))
}

/// A grid cell of a note, joined across the gap to the neighbour cell on
/// the sides where the note goes on; a joined side has no rounding, so a
/// flat edge at the border of the grid shows the note continues in the
/// next (previous) bar.
fn note_cell(cell: Rectangle, gap: i32, corner: Size, join_l: bool, join_r: bool) -> RoundedRectangle {
    let (x, w) = (cell.top_left.x, cell.size.width as i32);
    let x0 = if join_l { x - gap } else { x };
    let x1 = if join_r { x + w + gap } else { x + w };
    let r = Rectangle::new(Point::new(x0, cell.top_left.y), Size::new((x1 - x0).max(1) as u32, cell.size.height));
    let side = |joined: bool| if joined { Size::zero() } else { corner };
    RoundedRectangle::new(r, CornerRadii { top_left: side(join_l), bottom_left: side(join_l), top_right: side(join_r), bottom_right: side(join_r) })
}

/// Lead page content, extracted from the state so views compare cheaply.
#[derive(Clone, Debug, PartialEq)]
pub struct SeqView {
    /// A note starts on this step.
    on: [[bool; STEPS]; TRACKS],
    /// The note sounding on each step: the absolute step of its head, for
    /// the head itself and the steps its length covers.
    cover: [[Option<u16>; STEPS]; TRACKS],
    /// The note on the last step goes on into the next bar.
    cont_right: [bool; TRACKS],
    /// Step note differs from the track's default note.
    alt: [[bool; STEPS]; TRACKS],
    /// Velocity of the note sounding on each step (its head's).
    vel: [[u8; STEPS]; TRACKS],
    labels: [String; TRACKS],
    muted: [bool; TRACKS],
    track: usize,
    bars: u8,
    /// Bar on screen: the one picked when locked, else the one playing.
    bar: usize,
    /// The screen stays on `bar` while the clock plays other bars.
    locked: bool,
    /// Selected step within the shown bar (edit mode).
    selected_step: Option<usize>,
    /// Bar and step the clock is on within the phrase.
    play: Option<(usize, usize)>,
    edit: bool,
    playing: bool,
    pattern: usize,
    params: [String; 2],
}

impl SeqView {
    /// The bar on screen; `play` is where the clock is in the phrase.
    pub fn shown_bar(&self) -> usize {
        self.bar
    }

    fn build(s: &Status) -> SeqView {
        let st = &s.seq;
        let p = st.pattern();
        let bars = p.bars.max(1);
        let play = sequencer::phrase_playhead(&s.live, s.now, bars as usize);
        let bar = shown_bar(st.shown_bar(), st.bar_lock, play);
        let n = phrase_steps(bars);
        let next_bar = (bar + 1) % bars as usize;
        let mut on = [[false; STEPS]; TRACKS];
        let mut cover = [[None; STEPS]; TRACKS];
        let mut cont_right = [false; TRACKS];
        let mut alt = [[false; STEPS]; TRACKS];
        let mut vel = [[0u8; STEPS]; TRACKS];
        let mut labels: [String; TRACKS] = Default::default();
        let mut muted = [false; TRACKS];
        for (t, tr) in p.tracks.iter().enumerate().take(TRACKS) {
            let map = cover_map(|i| tr.steps.get(i).filter(|st| st.on).map(|st| st.len as usize), n);
            for k in 0..STEPS {
                let idx = bar * STEPS + k;
                cover[t][k] = map[idx];
                on[t][k] = map[idx] == Some(idx as u16);
                let step = tr.steps.get(idx).copied().unwrap_or_default();
                alt[t][k] = on[t][k] && step.note != tr.note;
                vel[t][k] = map[idx].and_then(|h| tr.steps.get(h as usize)).map_or(0, |st| st.vel);
            }
            let last = bar * STEPS + STEPS - 1;
            cont_right[t] = map[last].is_some() && map[next_bar * STEPS] == map[last];
            labels[t] = note_name(tr.note);
            muted[t] = tr.muted;
        }
        let tr = &p.tracks[st.track.min(TRACKS - 1)];
        let sel = st.edit.then_some(st.selected_step).flatten().filter(|i| *i < tr.steps.len());
        let (note_p, vel_p) = match sel {
            Some(i) => (format!("STEP {}", note_name(tr.steps[i].note)), format!("VEL {}", tr.steps[i].vel)),
            None => (format!("NOTE {}", note_name(tr.note)), format!("VEL {}", tr.vel)),
        };
        SeqView {
            on,
            cover,
            cont_right,
            alt,
            vel,
            labels,
            muted,
            track: st.track,
            bars,
            bar,
            locked: st.bar_lock,
            selected_step: sel.filter(|i| i / STEPS == bar).map(|i| i % STEPS),
            play,
            edit: st.edit,
            playing: st.playing,
            pattern: st.current,
            params: [note_p, vel_p],
        }
    }
}

/// Bass page content: the bar being shown, one note sounding per step at most.
#[derive(Clone, Debug, PartialEq)]
pub struct BassView {
    /// The note sounding on each step (its head may be on an earlier step).
    notes: [Option<u8>; STEPS],
    /// Absolute step of the head of the note sounding on each step.
    cover: [Option<u16>; STEPS],
    /// Velocity of the note sounding on each step.
    vel: [u8; STEPS],
    /// The note on the last step goes on into the next bar.
    cont_right: bool,
    /// Lowest note of the shown pitch window.
    base: u8,
    labels: [String; BASS_ROWS],
    bars: u8,
    /// Bar on screen: the one picked when locked, else the one playing.
    bar: usize,
    locked: bool,
    /// Bar and step the clock is on within the phrase.
    play: Option<(usize, usize)>,
    params: [String; 1],
}

impl BassView {
    fn build(s: &Status) -> BassView {
        let b = &s.seq.bass;
        let bars = b.bars.max(1);
        let play = sequencer::phrase_playhead(&s.live, s.now, bars as usize);
        let bar = shown_bar(s.seq.bass_bar.min(bars as usize - 1), s.seq.bass_bar_lock, play);
        let n = phrase_steps(bars);
        let map = cover_map(|i| b.steps.get(i).filter(|st| st.on).map(|st| st.len as usize), n);
        let mut notes = [None; STEPS];
        let mut cover = [None; STEPS];
        let mut vel = [0u8; STEPS];
        for k in 0..STEPS {
            cover[k] = map[bar * STEPS + k];
            let head = cover[k].and_then(|h| b.steps.get(h as usize));
            notes[k] = head.map(|st| st.note);
            vel[k] = head.map_or(0, |st| st.vel);
        }
        let last = bar * STEPS + STEPS - 1;
        let cont_right = map[last].is_some() && map[((bar + 1) % bars as usize) * STEPS] == map[last];
        let mut labels: [String; BASS_ROWS] = Default::default();
        for (r, l) in labels.iter_mut().enumerate() {
            *l = note_name(b.base + r as u8);
        }
        BassView {
            notes,
            cover,
            vel,
            cont_right,
            base: b.base,
            labels,
            bars,
            bar,
            locked: s.seq.bass_bar_lock,
            play,
            params: [format!("VEL {}", b.vel)],
        }
    }
}

/// Drums page content: the selected instrument's step row for the shown
/// bar, and every pad's state.
#[derive(Clone, Debug, PartialEq)]
pub struct DrumView {
    /// The selected instrument's steps in the shown bar: 0 off, 1 hit, 2 accent.
    steps: [u8; STEPS],
    /// Every instrument's steps in the shown bar, for the dots on the pads.
    dots: [[u8; STEPS]; DRUMS],
    labels: [String; DRUMS],
    muted: [bool; DRUMS],
    solo: [bool; DRUMS],
    /// The instrument hits on the 16th the clock is on: its pad lights.
    lit: [bool; DRUMS],
    drum: usize,
    bars: u8,
    /// Bar on screen: the one picked when locked, else the one playing.
    bar: usize,
    locked: bool,
    /// Bar and step the clock is on within the phrase.
    play: Option<(usize, usize)>,
    params: [String; 3],
}

impl DrumView {
    fn build(s: &Status) -> DrumView {
        let st = &s.seq;
        let d = &st.drums;
        let bars = d.bars.max(1);
        let play = sequencer::phrase_playhead(&s.live, s.now, bars as usize);
        let bar = shown_bar(st.drum_shown_bar(), st.drum_bar_lock, play);
        let sel = st.drum.min(DRUMS - 1);
        let code = |st: sequencer::DrumStep| if !st.on { 0 } else if st.accent { 2 } else { 1 };
        let mut dots = [[0u8; STEPS]; DRUMS];
        let mut labels: [String; DRUMS] = Default::default();
        let mut muted = [false; DRUMS];
        let mut solo = [false; DRUMS];
        let mut lit = [false; DRUMS];
        for (i, dr) in d.drums.iter().enumerate().take(DRUMS) {
            for (k, dot) in dots[i].iter_mut().enumerate() {
                *dot = dr.steps.get(bar * STEPS + k).map_or(0, |st| code(*st));
            }
            labels[i] = drum_name(dr.note);
            muted[i] = dr.muted;
            solo[i] = dr.solo;
            lit[i] = play.is_some_and(|(b, k)| dr.steps.get(b * STEPS + k).is_some_and(|st| st.on)) && d.audible(i);
        }
        let dr = &d.drums[sel];
        DrumView {
            steps: dots[sel],
            dots,
            labels,
            muted,
            solo,
            lit,
            drum: sel,
            bars,
            bar,
            locked: st.drum_bar_lock,
            play,
            params: [format!("NOTE {}", note_name(dr.note)), format!("VEL {}", dr.vel), format!("SWING {}", d.swing)],
        }
    }
}

/// Settings page content: every instrument's name and MIDI channel, in the
/// order of `Instrument::all()`.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsView {
    rows: Vec<SettingsRow>,
}

#[derive(Clone, Debug, PartialEq)]
struct SettingsRow {
    /// "LEAD", "BASS", "DRUMS".
    name: &'static str,
    /// "CH 10".
    value: String,
    color: Rgb888,
}

impl SettingsView {
    fn build(s: &Status) -> SettingsView {
        let rows = Instrument::all()
            .map(|inst| {
                let (name, color) = match inst {
                    Instrument::Lead => ("LEAD", TEXT),
                    Instrument::Bass => ("BASS", BASS),
                    Instrument::Drums => ("DRUMS", TEXT),
                };
                SettingsRow { name, value: format!("CH {}", s.seq.channel(inst) + 1), color }
            })
            .collect();
        SettingsView { rows }
    }
}

/// Key page content: the key in effect and what the detector hears.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyView {
    auto: bool,
    /// The key in effect, if any (`effective_key`).
    key: Option<Key>,
    /// "A MINOR", or "--" while nothing is known.
    name: String,
    /// The name's colour: dim while the heard key is not trusted.
    color: Rgb888,
    /// Confidence in percent (AUTO only).
    conf_pct: Option<u32>,
    /// "TUNING +12c", "ALSO C MAJOR" and the like, under the name.
    info: String,
    /// The heard chroma, 0..100 per pitch class (AUTO only).
    chroma: [u8; 12],
    /// The heard key's pitch classes (AUTO, for the chroma strip): the
    /// key in effect, or the dimmed one while it is not trusted.
    heard: Option<Key>,
}

impl KeyView {
    fn build(s: &Status) -> KeyView {
        let setup = &s.seq.key;
        let key = effective_key(setup, s.key.as_ref());
        if !setup.auto {
            let k = setup.key;
            let notes: Vec<&str> = (0..12u8).filter(|&pc| k.contains(pc)).map(|pc| PITCH_NAMES[pc as usize]).collect();
            return KeyView { auto: false, key, name: k.name(), color: MANUAL, conf_pct: None, info: format!("NOTES {}", notes.join(" ")), chroma: [0; 12], heard: Some(k) };
        }
        let e = s.key.as_ref();
        let heard = e.map(|e| Key::new(e.tonic, scale_of(e.mode)));
        let name = heard.map_or("--".to_string(), |k| k.name());
        let conf = e.map_or(0.0, |e| e.confidence);
        let color = if e.is_none() {
            DIM
        } else if conf >= 0.6 {
            AUTO
        } else if conf >= KEY_MIN_CONFIDENCE {
            YELLOW
        } else {
            DIM
        };
        let mut parts = Vec::new();
        if let Some(e) = e {
            if e.tuning_cents.abs() >= 5.0 {
                parts.push(format!("TUNING {:+.0}c", e.tuning_cents));
            }
            if let Some((t, m)) = e.alt {
                parts.push(format!("ALSO {}", Key::new(t, scale_of(m)).name()));
            }
        } else {
            parts.push("LISTENING".to_string());
        }
        let mut chroma = [0u8; 12];
        if let Some(e) = e {
            for (c, v) in chroma.iter_mut().zip(e.chroma) {
                *c = (v * 100.0).round().clamp(0.0, 100.0) as u8;
            }
        }
        KeyView { auto: true, key, name, color, conf_pct: e.map(|e| (e.confidence * 100.0).round() as u32), info: parts.join("   "), chroma, heard }
    }
}

/// The bar to put on screen: the picked one while locked (or while the clock
/// is stopped), otherwise the bar being played.
fn shown_bar(picked: usize, locked: bool, play: Option<(usize, usize)>) -> usize {
    match play {
        Some((bar, _)) if !locked => bar,
        _ => picked,
    }
}

/// What actually gets drawn, quantised so equal views mean "no redraw needed".
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    page: Page,
    mode: Mode,
    state: String,
    state_color: Rgb888,
    bpm: String,
    info: String,
    conf_pct: Option<u32>,
    lamp: Option<u32>,
    bar_len: u32,
    level_pct: u32,
    xruns: u64,
    stats: String,
    running: bool,
    /// The KEY page is on AUTO (its hand-set controls are hidden).
    key_auto: bool,
    /// A key is known (the generators work).
    key_known: bool,
    pressed: Option<Button>,
    seq: Option<SeqView>,
    bass: Option<BassView>,
    drums: Option<DrumView>,
    settings: Option<SettingsView>,
    key: Option<KeyView>,
    /// Where the finger is, drawn as a marker (calibration aid).
    pub touch: Option<(i32, i32)>,
}

impl View {
    /// (bar on screen, playing bar) of the sequence page, if built.
    pub fn seq_bars(&self) -> Option<(usize, Option<usize>)> {
        self.seq.as_ref().map(|q| (q.bar, q.play.map(|(b, _)| b)))
    }

    /// (bar on screen, playing bar) of the bass page, if built.
    pub fn bass_bars(&self) -> Option<(usize, Option<usize>)> {
        self.bass.as_ref().map(|q| (q.bar, q.play.map(|(b, _)| b)))
    }

    /// (bar on screen, playing bar) of the drums page, if built.
    pub fn drum_bars(&self) -> Option<(usize, Option<usize>)> {
        self.drums.as_ref().map(|q| (q.bar, q.play.map(|(b, _)| b)))
    }

    pub fn build(s: &Status, pressed: Option<Button>) -> View {
        Self::build_with_touch(s, Page::Tempo, pressed, None)
    }

    pub fn build_with_touch(s: &Status, page: Page, pressed: Option<Button>, touch: Option<(i32, i32)>) -> View {
        let live = &s.live;
        let (state, state_color) = match s.mode {
            Mode::Auto => match s.tracker.state {
                State::Idle => ("LISTENING", DIM),
                State::Locking => ("LOCKING", YELLOW),
                State::Running => ("RUNNING", GREEN),
                State::Coasting => ("COASTING", YELLOW),
                State::Stopped => ("STOPPED", RED),
            },
            Mode::Manual => {
                if !live.running {
                    ("STOPPED", RED)
                } else if s.now < live.beat_time(live.n0) {
                    ("COUNT-IN", YELLOW)
                } else {
                    ("RUNNING", GREEN)
                }
            }
        };
        let bpm = if s.mode == Mode::Auto && !s.tracker.running && s.tracker.state != State::Coasting {
            "---.-".to_string()
        } else {
            format!("{:5.1}", live.bpm())
        };
        let (info, conf_pct) = match s.mode {
            Mode::Auto => (format!("TEMPO STRENGTH {:.2}", s.tempo_strength), Some((s.tracker.confidence * 100.0) as u32)),
            Mode::Manual => {
                let mic = if s.tracker.running {
                    format!("MIC HEARS {:5.1} BPM", s.tracker.bpm())
                } else {
                    "MIC HEARS NO BEAT".to_string()
                };
                let taps = if s.taps > 0 { format!("   TAPS {}", s.taps) } else { String::new() };
                (format!("{mic}{taps}"), None)
            }
        };
        let lamp = if live.running && live.period > 0.05 {
            let n = live.first_beat_at_or_after(s.now) - 1;
            let age = s.now - live.beat_time(n);
            if (0.0..0.12).contains(&age) {
                Some((n - live.downbeat as i64).rem_euclid(live.bar_len.max(1) as i64) as u32)
            } else {
                None
            }
        } else {
            None
        };
        // The level meter is only drawn on the TEMPO page; freeze it elsewhere so
        // a noisy mic does not force a redraw of the LEAD page every frame.
        let level_pct = if page == Page::Tempo { (((s.level_db + 60.0) / 60.0 * 50.0).round().clamp(0.0, 50.0) as u32) * 2 } else { 0 };
        let stats = if s.midi { format!("MIDI {}k TICKS", s.ticks / 1000) } else { "MIDI OFF".to_string() };
        View {
            page,
            mode: s.mode,
            state: state.into(),
            state_color,
            bpm,
            info,
            conf_pct,
            lamp,
            bar_len: live.bar_len.max(1),
            level_pct,
            xruns: s.xruns,
            stats,
            running: live.running,
            key_auto: s.seq.key.auto,
            key_known: effective_key(&s.seq.key, s.key.as_ref()).is_some(),
            pressed,
            seq: (page == Page::Lead).then(|| SeqView::build(s)),
            bass: (page == Page::Bass).then(|| BassView::build(s)),
            drums: (page == Page::Drums).then(|| DrumView::build(s)),
            settings: (page == Page::Settings).then(|| SettingsView::build(s)),
            key: (page == Page::Key).then(|| KeyView::build(s)),
            touch,
        }
    }
}

pub struct Screen {
    w: i32,
    h: i32,
    m: i32,
    small: u32,
    big: u32,
    /// Text scale of the clock state in the top bar (it has to fit between
    /// the tabs and the level meter).
    state_k: u32,
    /// Width of the level meter in the top bar.
    meter_w: i32,
    /// Left edge of the free space in the tempo page's bottom row.
    stats_x: i32,
    /// Horizontal centre of the tempo readout.
    bpm_cx: i32,
    /// Centre of the clock state in the top bar (TEMPO page).
    state_cx: i32,
    /// Key page: the area of the root buttons, which shows the chroma
    /// strip while the key is heard.
    key_strip: Rectangle,
    buttons: Vec<(Button, Rectangle)>,
    tracks: Vec<Rectangle>,
    grid: Rectangle,
    cell_w: i32,
    cell_h: i32,
    /// Bass page: the pitch grid and its cells.
    bass_grid: Rectangle,
    bass_cell_w: i32,
    bass_cell_h: i32,
    /// Bass page: the note names; dragging them (or the grid) up and down
    /// scrolls the pitch window.
    bass_labels: Rectangle,
    /// Drums page: width of one step button.
    drum_step_w: i32,
    /// The LEN / BAR captions of the phrase row (sequence, bass and drums pages).
    captions: [(Point, &'static str); 2],
    /// The tab labels as they fit the width.
    tab_labels: [&'static str; TABS.len()],
    /// Settings page: the name and the channel readout of every instrument,
    /// in the order of `Instrument::all()`.
    setup_rows: Vec<(Rectangle, Rectangle)>,
}

impl Screen {
    pub fn new(w: usize, h: usize) -> Self {
        let (w, h) = (w as i32, h as i32);
        let m = (w / 80).max(4);
        let small = ((w / 400) as u32).clamp(1, 3);
        let mut buttons = Vec::new();
        let rect = |x: i32, y: i32, bw: i32, bh: i32| Rectangle::new(Point::new(x, y), Size::new(bw.max(1) as u32, bh.max(1) as u32));
        // --- top bar: page tabs -------------------------------------------------
        let top_h = (h as f64 * 0.13) as i32;
        // Each tab is as wide as its label. The level meter sits at the
        // right of the bar, its "IN" caption left of it, and the clock state
        // ("LISTENING" is the longest) between it and the tabs: the tabs
        // take the largest text (two lines of the small font at most) that
        // leaves the state its room, and short labels if even one line of
        // the long ones does not.
        let meter_w = (w as f64 * 0.12) as i32;
        let meter_x = w - m - meter_w;
        let state_end = meter_x - m - 20 * small as i32;
        let tabs_w = |k: i32, long: bool| TABS.iter().map(|(_, l, s)| (if long { l } else { s }).len() as i32 * 10 * k + 3 * m).sum::<i32>() + m;
        let fits = |k: i32, long: bool| tabs_w(k, long) + 9 * 10 <= state_end;
        let (tab_k, long) = (1..=small.min(2) as i32).rev().map(|k| (k, true)).find(|&(k, l)| fits(k, l)).unwrap_or((1, false));
        let tab_labels = TABS.map(|(_, l, s)| if long { l } else { s });
        let mut x = m;
        for ((b, _, _), label) in TABS.into_iter().zip(tab_labels) {
            let tab_w = label.len() as i32 * 10 * tab_k + 2 * m;
            buttons.push((b, rect(x, m, tab_w, top_h - m)));
            x += tab_w + m;
        }
        let tabs_end = x;
        let state_cx = (tabs_end + state_end) / 2;
        let state_avail = state_end - tabs_end;
        let state_k = (1..=small).rev().find(|k| 9 * 10 * *k as i32 <= state_avail).unwrap_or(1);
        // --- tempo page ----------------------------------------------------------------
        let mode_w = (w as f64 * 0.17) as i32;
        let y1 = (h as f64 * 0.16) as i32;
        let mode_h = (h as f64 * 0.17) as i32;
        buttons.push((Button::ModeAuto, rect(m, y1, mode_w, mode_h)));
        buttons.push((Button::ModeManual, rect(m, y1 + mode_h + m, mode_w, mode_h)));
        let bpm_left = 2 * m + mode_w;
        let bpm_cx = (bpm_left + w) / 2;
        let big = (((w - bpm_left) as f64 * 0.85 / 50.0).min(h as f64 * 0.30 / 20.0).floor() as u32).max(2);
        let unit = (w - 7 * m) / 6;
        let ya = (h as f64 * 0.66) as i32;
        let ha = (h as f64 * 0.15) as i32;
        let mut x = m;
        for (b, units) in [(Button::Minus5, 1), (Button::Minus1, 1), (Button::Tap, 2), (Button::Plus1, 1), (Button::Plus5, 1)] {
            let bw = unit * units + m * (units - 1);
            buttons.push((b, rect(x, ya, bw, ha)));
            x += bw + m;
        }
        let yb = (h as f64 * 0.83) as i32;
        let hb = h - m - yb;
        buttons.push((Button::StartStop, rect(m, yb, 2 * unit + m, hb)));
        let stats_x = 3 * m + 2 * unit;
        // --- key page ------------------------------------------------------------------
        // AUTO / MANUAL where the tempo page has them, the key's name at the
        // right of them, and underneath the twelve roots in a row, then the
        // scales in two rows (by hand only; the root row is the chroma strip
        // while the key is heard).
        buttons.push((Button::KeyAuto, rect(m, y1, mode_w, mode_h)));
        buttons.push((Button::KeyManual, rect(m, y1 + mode_h + m, mode_w, mode_h)));
        let ky = (h as f64 * 0.55) as i32;
        let kh = (h as f64 * 0.12) as i32;
        let root_w = (w - 13 * m) / 12;
        for pc in 0..12u8 {
            buttons.push((Button::KeyRoot(pc), rect(m + pc as i32 * (root_w + m), ky, root_w, kh)));
        }
        let key_strip = rect(m, ky, 12 * root_w + 11 * m, kh);
        let sy = ky + kh + m;
        let sh = (h - m - sy - m) / 2;
        for (row, scales) in SCALE_ROWS.iter().enumerate() {
            let n = scales.len() as i32;
            let sw = (w - (n + 1) * m) / n;
            let y = sy + row as i32 * (sh + m);
            for (i, sc) in scales.iter().enumerate() {
                buttons.push((Button::KeyScale(*sc), rect(m + i as i32 * (sw + m), y, sw, sh)));
            }
        }
        // --- sequence and bass pages: phrase row "LEN" 1 2 4 8   "BAR" 1 .. 8 ---------
        let by = (h as f64 * 0.15) as i32;
        let bh = (h as f64 * 0.09) as i32;
        let cap_w = (w as f64 * 0.07) as i32;
        let bu = (w - 2 * cap_w - 15 * m) / 13;
        let mut x = m + cap_w;
        let len_cap = Point::new(x - m, by + bh / 2);
        for (i, n) in [1u8, 2, 4, 8].into_iter().enumerate() {
            let r = rect(x + i as i32 * (bu + m), by, bu, bh);
            buttons.push((Button::Bars(n), r));
            buttons.push((Button::BassBars(n), r));
            buttons.push((Button::DrumBars(n), r));
        }
        x += 4 * (bu + m) + m + cap_w;
        let bar_cap = Point::new(x - m, by + bh / 2);
        for b in 0..sequencer::MAX_BARS {
            let r = rect(x + b as i32 * (bu + m), by, bu, bh);
            buttons.push((Button::Bar(b), r));
            buttons.push((Button::BassBar(b), r));
            buttons.push((Button::DrumBar(b), r));
        }
        // and a padlock: hold the shown bar instead of following the clock
        let r = rect(x + sequencer::MAX_BARS as i32 * (bu + m), by, bu, bh);
        buttons.push((Button::BarLock, r));
        buttons.push((Button::BassBarLock, r));
        buttons.push((Button::DrumBarLock, r));
        // Parameters along the bottom of both pages, and the RANDOM /
        // CNTRPT pair in the last group.
        let gap = (m / 2).max(2);
        let yp = (h as f64 * 0.86) as i32;
        let hp = h - m - yp;
        let group_w = param_group_w(w, m, SEQ_GROUPS);
        let pm_w = group_w / 4;
        let gen_pair = |buttons: &mut Vec<(Button, Rectangle)>, inst: Instrument, gx: i32, gw: i32| {
            let half = (gw - gap) / 2;
            buttons.push((Button::Generate(inst, Style::Random), rect(gx, yp, half, hp)));
            buttons.push((Button::Generate(inst, Style::Counterpoint), rect(gx + gw - half, yp, half, hp)));
        };
        // --- sequence page ---------------------------------------------------------
        let gy0 = by + bh + m;
        let gy1 = (h as f64 * 0.84) as i32;
        let track_w = (w as f64 * 0.09) as i32;
        let cell_h = (gy1 - gy0 - gap * (TRACKS as i32 - 1)) / TRACKS as i32;
        let mut tracks = Vec::new();
        for t in 0..TRACKS as i32 {
            tracks.push(rect(m, gy0 + t * (cell_h + gap), track_w, cell_h));
        }
        let gx0 = 2 * m + track_w;
        let cell_w = (w - m - gx0 - gap * (STEPS as i32 - 1)) / STEPS as i32;
        let grid = rect(gx0, gy0, cell_w * STEPS as i32 + gap * (STEPS as i32 - 1), cell_h * TRACKS as i32 + gap * (TRACKS as i32 - 1));
        for (i, p) in SEQ_PARAMS.into_iter().enumerate() {
            let gx = m + i as i32 * (group_w + m);
            buttons.push((Button::ParamMinus(p), rect(gx, yp, pm_w, hp)));
            buttons.push((Button::ParamPlus(p), rect(gx + group_w - pm_w, yp, pm_w, hp)));
        }
        let gen_x = m + SEQ_PARAMS.len() as i32 * (group_w + m);
        gen_pair(&mut buttons, Instrument::Lead, gen_x, w - m - gen_x);
        // --- bass page ---------------------------------------------------------------
        // Pitch grid: `BASS_ROWS` semitones, lowest note at the bottom, note
        // names at the left, and left of those the up/down scroll buttons.
        let py0 = by + bh + m;
        let py1 = (h as f64 * 0.84) as i32;
        let bass_cell_h = (py1 - py0 - gap * (BASS_ROWS as i32 - 1)) / BASS_ROWS as i32;
        let grid_h = bass_cell_h * BASS_ROWS as i32 + gap * (BASS_ROWS as i32 - 1);
        let scroll_w = (w as f64 * 0.05) as i32;
        let half = (grid_h - m) / 2;
        buttons.push((Button::BassScroll(1), rect(m, py0, scroll_w, half)));
        buttons.push((Button::BassScroll(-1), rect(m, py0 + grid_h - half, scroll_w, half)));
        // The note names sit right next to the arrows, in a column just wide
        // enough for the longest name ("C#-1").
        let label_k = small.min((((bass_cell_h + gap) as u32) / 20).max(1));
        let label_w = 4 * 10 * label_k as i32;
        let bass_labels = rect(m + scroll_w + gap, py0, label_w, grid_h);
        let px0 = m + scroll_w + gap + label_w + m;
        let bass_cell_w = (w - m - px0 - gap * (STEPS as i32 - 1)) / STEPS as i32;
        let bass_grid = rect(px0, py0, bass_cell_w * STEPS as i32 + gap * (STEPS as i32 - 1), grid_h);
        let group_w3 = param_group_w(w, m, BASS_GROUPS);
        let pm_w3 = group_w3 / 4;
        for (i, p) in BASS_PARAMS.into_iter().enumerate() {
            let gx = m + i as i32 * (group_w3 + m);
            buttons.push((Button::BassMinus(p), rect(gx, yp, pm_w3, hp)));
            buttons.push((Button::BassPlus(p), rect(gx + group_w3 - pm_w3, yp, pm_w3, hp)));
        }
        let gen_x = m + BASS_PARAMS.len() as i32 * (group_w3 + m);
        gen_pair(&mut buttons, Instrument::Bass, gen_x, w - m - gen_x);
        // --- drums page ---------------------------------------------------------------
        // As on a DrumBrute: the pads in the middle, the 16 step buttons of
        // the selected instrument in one big row underneath.
        let dy0 = by + bh + m;
        let dy1 = (h as f64 * 0.84) as i32;
        let step_h = ((dy1 - dy0) as f64 * 0.30) as i32;
        let step_y = dy1 - step_h;
        let drum_step_w = (w - 2 * m - gap * (STEPS as i32 - 1)) / STEPS as i32;
        for k in 0..STEPS {
            buttons.push((Button::DrumStep(k), rect(m + k as i32 * (drum_step_w + gap), step_y, drum_step_w, step_h)));
        }
        let pads_h = step_y - m - dy0;
        let px = m;
        let pad_w = (w - m - px - gap * (PAD_COLS as i32 - 1)) / PAD_COLS as i32;
        let pad_h = (pads_h - gap) / 2;
        for i in 0..DRUMS {
            let (row, col) = ((i / PAD_COLS) as i32, (i % PAD_COLS) as i32);
            buttons.push((Button::DrumPad(i), rect(px + col * (pad_w + gap), dy0 + row * (pad_h + gap), pad_w, pad_h)));
        }
        let group_w4 = param_group_w(w, m, DRUM_PARAMS.len());
        let pm_w4 = group_w4 / 4;
        for (i, p) in DRUM_PARAMS.into_iter().enumerate() {
            let gx = m + i as i32 * (group_w4 + m);
            buttons.push((Button::DrumMinus(p), rect(gx, yp, pm_w4, hp)));
            buttons.push((Button::DrumPlus(p), rect(gx + group_w4 - pm_w4, yp, pm_w4, hp)));
        }
        // --- settings page -------------------------------------------------------------
        // One row per instrument across the width: its name, then
        // "[-] CH n [+]" with the readout taking what the buttons leave.
        let setup_n = Instrument::all().count() as i32;
        let row_h = ((h as f64 * 0.16) as i32).min((h - m - by - (setup_n - 1) * m) / setup_n);
        let name_w = (w as f64 * 0.25) as i32;
        let pm_w5 = (w as f64 * 0.15) as i32;
        let value_w = w - 2 * m - name_w - m - 2 * pm_w5 - 2 * gap;
        let mut setup_rows = Vec::new();
        for (i, inst) in Instrument::all().enumerate() {
            let y = by + i as i32 * (row_h + m);
            let gx = m + name_w + m;
            buttons.push((Button::ChannelMinus(inst), rect(gx, y, pm_w5, row_h)));
            buttons.push((Button::ChannelPlus(inst), rect(gx + pm_w5 + gap + value_w + gap, y, pm_w5, row_h)));
            setup_rows.push((rect(m, y, name_w, row_h), rect(gx + pm_w5 + gap, y, value_w, row_h)));
        }
        Self {
            w,
            h,
            m,
            small,
            big,
            state_k,
            meter_w,
            stats_x,
            bpm_cx,
            state_cx,
            key_strip,
            buttons,
            tracks,
            grid,
            cell_w,
            cell_h,
            bass_grid,
            bass_cell_w,
            bass_cell_h,
            bass_labels,
            drum_step_w,
            captions: [(len_cap, "LEN"), (bar_cap, "BAR")],
            tab_labels,
            setup_rows,
        }
    }

    /// Vertical distance between two bass rows: one semitone of drag scrolling.
    pub fn bass_row_pitch(&self) -> i32 {
        self.bass_cell_h + (self.m / 2).max(2)
    }

    /// Horizontal distance between two bass columns.
    pub fn bass_col_pitch(&self) -> i32 {
        self.bass_cell_w + (self.m / 2).max(2)
    }

    /// Pixels of vertical drag per velocity step (the full range takes
    /// about half the screen height).
    pub fn vel_pixels(&self) -> i32 {
        (self.h / 240).max(2)
    }

    /// Column of the sequence (or bass) grid under `x`, clamped to the grid,
    /// for a finger dragging a note's length.
    pub fn column_at(&self, x: i32, bass: bool) -> usize {
        let (g, cw) = if bass { (self.bass_grid, self.bass_cell_w) } else { (self.grid, self.cell_w) };
        let gap = (self.m / 2).max(2);
        ((x - g.top_left.x) / (cw + gap)).clamp(0, STEPS as i32 - 1) as usize
    }

    /// +1 when `x` is at the right edge of the grid (the outer quarter of the
    /// last column, or beyond), -1 at the left edge, 0 elsewhere.
    pub fn grid_edge(&self, x: i32, bass: bool) -> i32 {
        let (g, cw) = if bass { (self.bass_grid, self.bass_cell_w) } else { (self.grid, self.cell_w) };
        let l = g.top_left.x;
        let r = l + g.size.width as i32;
        if x >= r - cw / 4 {
            1
        } else if x < l + cw / 4 {
            -1
        } else {
            0
        }
    }

    pub fn hit(&self, x: i32, y: i32, page: Page) -> Option<Hit> {
        let p = Point::new(x, y);
        if let Some((b, _)) = self.buttons.iter().find(|(b, r)| b.page().map_or(true, |bp| bp == page) && r.contains(p)) {
            return Some(Hit::Button(*b));
        }
        if page == Page::Lead {
            if let Some(t) = self.tracks.iter().position(|r| r.contains(p)) {
                return Some(Hit::Track(t));
            }
            if self.grid.contains(p) {
                let gap = (self.m / 2).max(2);
                let cx = (x - self.grid.top_left.x) / (self.cell_w + gap);
                let cy = (y - self.grid.top_left.y) / (self.cell_h + gap);
                if cx >= 0 && cy >= 0 && (cx as usize) < STEPS && (cy as usize) < TRACKS {
                    return Some(Hit::Cell(cy as usize, cx as usize));
                }
            }
        }
        if page == Page::Bass && self.bass_labels.contains(p) {
            return Some(Hit::BassLabels);
        }
        if page == Page::Bass && self.bass_grid.contains(p) {
            let gap = (self.m / 2).max(2);
            let cx = (x - self.bass_grid.top_left.x) / (self.bass_cell_w + gap);
            let cy = (y - self.bass_grid.top_left.y) / (self.bass_cell_h + gap);
            if cx >= 0 && cy >= 0 && (cx as usize) < STEPS && (cy as usize) < BASS_ROWS {
                return Some(Hit::BassCell(BASS_ROWS - 1 - cy as usize, cx as usize));
            }
        }
        None
    }

    fn text(&self, c: &mut Canvas, s: &str, at: Point, k: u32, color: Rgb888, align: Alignment) {
        let style = MonoTextStyle::new(&FONT_10X20, color);
        let ts = TextStyleBuilder::new().alignment(align).baseline(Baseline::Middle).build();
        let mut sc = Scaled { target: c, k, origin: at };
        let _ = Text::with_text_style(s, Point::zero(), style, ts).draw(&mut sc);
    }

    /// A padlock in the middle of `r`: shackle closed when `locked`, lifted
    /// open otherwise.
    fn padlock(&self, c: &mut Canvas, r: &Rectangle, locked: bool, color: Rgb888) {
        let side = (r.size.width.min(r.size.height) as i32 * 6 / 10).max(8);
        let stroke = (side / 8).max(2) as u32;
        let ctr = r.center();
        // body: a rounded box in the lower half
        let body_w = side;
        let body_h = side / 2;
        let body_y = ctr.y + side / 2 - body_h;
        let body = Rectangle::new(Point::new(ctr.x - body_w / 2, body_y), Size::new(body_w as u32, body_h as u32));
        let corner = Size::new(stroke, stroke);
        let _ = RoundedRectangle::with_equal_corners(body, corner).into_styled(PrimitiveStyle::with_fill(color)).draw(c);
        // shackle: a half circle standing on the body, open on the right when unlocked
        let d = (side * 55 / 100).max(4);
        let lift = if locked { 0 } else { (stroke as i32).max(side / 6) };
        let arc_c = Point::new(ctr.x, body_y - d / 2 + stroke as i32 / 2 - lift);
        let style = PrimitiveStyle::with_stroke(color, stroke);
        let _ = Arc::with_center(arc_c, d as u32, Angle::from_degrees(180.0), Angle::from_degrees(180.0)).into_styled(style).draw(c);
        let leg = |x: i32, to_y: i32| Line::new(Point::new(x, arc_c.y), Point::new(x, to_y)).into_styled(style);
        let hx = d / 2;
        let _ = leg(ctr.x - hx, body_y).draw(c);
        let _ = leg(ctr.x + hx, if locked { body_y } else { arc_c.y + (d / 4).max(1) }).draw(c);
    }

    fn button(&self, c: &mut Canvas, r: &Rectangle, label: &str, fill: Rgb888, fg: Rgb888, accent: Option<Rgb888>) {
        let rr = RoundedRectangle::with_equal_corners(*r, Size::new(self.m as u32, self.m as u32));
        let style = match accent {
            Some(a) => PrimitiveStyleBuilder::new().fill_color(fill).stroke_color(a).stroke_width(3).build(),
            None => PrimitiveStyle::with_fill(fill),
        };
        let _ = rr.into_styled(style).draw(c);
        // Shrink the text if the label does not fit.
        let mut k = self.small;
        while k > 1 && (label.len() as i32) * 10 * k as i32 > r.size.width as i32 - self.m {
            k -= 1;
        }
        self.text(c, label, r.center(), k, fg, Alignment::Center);
    }

    pub fn render(&self, c: &mut Canvas, v: &View) {
        let _ = c.clear(BG);
        let (w, h, m) = (self.w, self.h, self.m);
        let s = self.small;
        // --- buttons of the top bar and the current page ---------------------
        for (b, r) in &self.buttons {
            if b.page().map_or(false, |p| p != v.page) {
                continue;
            }
            // pads and steps are drawn by `render_drums`
            if matches!(b, Button::DrumPad(_) | Button::DrumStep(_)) {
                continue;
            }
            if !b.visible(v.mode, v.key_auto) {
                continue;
            }
            let bass = v.bass.as_ref();
            let seq = v.seq.as_ref();
            let drums = v.drums.as_ref();
            let drum_col = drums.map(|q| DRUM_COLORS[q.drum]);
            let key = v.key.as_ref();
            // bars beyond the phrase length cannot be selected
            let enabled = b.enabled(v.mode, v.key_auto, v.key_known)
                && !matches!(b, Button::BassBar(i) if bass.map_or(false, |q| *i >= q.bars as usize))
                && !matches!(b, Button::Bar(i) if seq.map_or(false, |q| *i >= q.bars as usize))
                && !matches!(b, Button::DrumBar(i) if drums.map_or(false, |q| *i >= q.bars as usize))
                && !matches!(b, Button::BassScroll(d) if bass.map_or(false, |q| (*d > 0 && q.base >= BASS_TOP) || (*d < 0 && q.base == 0)));
            let pressed = v.pressed == Some(*b);
            let (label, accent): (String, Option<Rgb888>) = match b {
                Button::PageTempo => (self.tab_labels[0].into(), (v.page == Page::Tempo).then_some(TEXT)),
                Button::PageKey => (self.tab_labels[1].into(), (v.page == Page::Key).then_some(TEXT)),
                Button::PageLead => (self.tab_labels[2].into(), (v.page == Page::Lead).then_some(TEXT)),
                Button::PageBass => (self.tab_labels[3].into(), (v.page == Page::Bass).then_some(TEXT)),
                Button::PageDrums => (self.tab_labels[4].into(), (v.page == Page::Drums).then_some(TEXT)),
                Button::PageSettings => (self.tab_labels[5].into(), (v.page == Page::Settings).then_some(TEXT)),
                Button::ModeAuto => ("AUTO".into(), (v.mode == Mode::Auto).then_some(AUTO)),
                Button::ModeManual => ("MANUAL".into(), (v.mode == Mode::Manual).then_some(MANUAL)),
                Button::Minus5 => ("-5".into(), None),
                Button::Minus1 => ("-1".into(), None),
                Button::Tap => ("TAP".into(), (v.mode == Mode::Manual).then_some(MANUAL)),
                Button::Plus1 => ("+1".into(), None),
                Button::Plus5 => ("+5".into(), None),
                Button::StartStop => (if v.running { "STOP" } else { "START" }.into(), None),
                Button::KeyAuto => ("AUTO".into(), (v.key_auto).then_some(AUTO)),
                Button::KeyManual => ("MANUAL".into(), (!v.key_auto).then_some(MANUAL)),
                Button::KeyRoot(pc) => (PITCH_NAMES[*pc as usize].into(), key.and_then(|q| q.key).filter(|k| k.root == *pc).map(|_| MANUAL)),
                Button::KeyScale(sc) => {
                    let (_, r) = self.buttons.iter().find(|(x, _)| x == b).expect("own button");
                    let label = if (sc.name().len() as i32) * 10 * s as i32 <= r.size.width as i32 - m { sc.name() } else { sc.short() };
                    (label.into(), key.and_then(|q| q.key).filter(|k| k.scale == *sc).map(|_| MANUAL))
                }
                Button::Generate(_, style) => {
                    let (long, short) = match style {
                        Style::Random => ("RANDOM", "RND"),
                        Style::Counterpoint => ("CNTRPT", "CPT"),
                    };
                    (if 6 * 10 <= r.size.width as i32 - m { long } else { short }.into(), None)
                }
                Button::ParamMinus(_) | Button::BassMinus(_) | Button::DrumMinus(_) | Button::ChannelMinus(_) => ("-".into(), None),
                Button::ParamPlus(_) | Button::BassPlus(_) | Button::DrumPlus(_) | Button::ChannelPlus(_) => ("+".into(), None),
                Button::BassScroll(_) => (String::new(), None),
                Button::BarLock => (String::new(), seq.filter(|q| q.locked).map(|q| TRACK_COLORS[q.track])),
                Button::BassBarLock => (String::new(), bass.map_or(false, |q| q.locked).then_some(BASS)),
                Button::BassBars(n) => (n.to_string(), bass.map_or(false, |q| q.bars == *n).then_some(BASS)),
                Button::BassBar(i) => {
                    let shown = bass.map_or(false, |q| q.bar == *i);
                    let playing = bass.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i));
                    ((i + 1).to_string(), if shown { Some(BASS) } else if playing { Some(GREEN) } else { None })
                }
                Button::Bars(n) => (n.to_string(), seq.filter(|q| q.bars == *n).map(|q| TRACK_COLORS[q.track])),
                Button::Bar(i) => {
                    let shown = seq.filter(|q| q.bar == *i).map(|q| TRACK_COLORS[q.track]);
                    let playing = seq.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i));
                    ((i + 1).to_string(), shown.or(playing.then_some(GREEN)))
                }
                Button::DrumBarLock => (String::new(), drums.filter(|q| q.locked).and(drum_col)),
                Button::DrumBars(n) => (n.to_string(), drums.filter(|q| q.bars == *n).and(drum_col)),
                Button::DrumBar(i) => {
                    let shown = drums.filter(|q| q.bar == *i).and(drum_col);
                    let playing = drums.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i));
                    ((i + 1).to_string(), shown.or(playing.then_some(GREEN)))
                }
                Button::DrumPad(_) | Button::DrumStep(_) => unreachable!(),
            };
            let fill = if !enabled {
                PANEL_DISABLED
            } else if pressed {
                PANEL_PRESSED
            } else {
                PANEL
            };
            let playing_bar = matches!(b, Button::BassBar(i) if bass.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i)))
                || matches!(b, Button::Bar(i) if seq.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i)))
                || matches!(b, Button::DrumBar(i) if drums.map_or(false, |q| q.play.map_or(false, |(bar, _)| bar == *i)));
            let in_key = matches!(b, Button::KeyRoot(pc) if key.and_then(|q| q.key).is_some_and(|k| k.contains(*pc)));
            let fg = if !enabled {
                DIM
            } else if playing_bar {
                GREEN
            } else if matches!(b, Button::KeyRoot(_)) && !in_key {
                DIM
            } else {
                TEXT
            };
            self.button(c, r, &label, fill, fg, accent);
            match b {
                Button::BarLock => self.padlock(c, r, seq.map_or(false, |q| q.locked), accent.unwrap_or(fg)),
                Button::BassBarLock => self.padlock(c, r, bass.map_or(false, |q| q.locked), accent.unwrap_or(fg)),
                Button::DrumBarLock => self.padlock(c, r, drums.map_or(false, |q| q.locked), accent.unwrap_or(fg)),
                _ => {}
            }
        }
        // --- top bar ----------------------------------------------------------
        // Tempo page: clock state in the middle, input level meter at the right.
        // Other pages: only the tempo (the state and the meter belong to the
        // TEMPO tab); XRUNS stay visible everywhere because they are an error.
        let top_h = (h as f64 * 0.13) as i32;
        let cy = top_h / 2;
        match v.page {
            Page::Tempo => {
                self.text(c, &v.state, Point::new(self.state_cx, cy), self.state_k, v.state_color, Alignment::Center);
                let meter_w = self.meter_w;
                let meter_h = (10 * s) as i32;
                let meter_x = w - m - meter_w;
                let _ = c.fill_solid(&Rectangle::new(Point::new(meter_x, cy - meter_h / 2), Size::new(meter_w as u32, meter_h as u32)), PANEL);
                let filled = meter_w * v.level_pct as i32 / 100;
                let lc = if v.level_pct > 95 {
                    RED
                } else if v.level_pct > 80 {
                    YELLOW
                } else {
                    GREEN
                };
                if filled > 0 {
                    let _ = c.fill_solid(&Rectangle::new(Point::new(meter_x, cy - meter_h / 2), Size::new(filled as u32, meter_h as u32)), lc);
                }
                self.text(c, "IN", Point::new(meter_x - m, cy), s, DIM, Alignment::Right);
                if v.xruns > 0 {
                    self.text(c, &format!("XRUNS {}", v.xruns), Point::new(meter_x - m - (30 * s) as i32, cy), s, RED, Alignment::Right);
                }
            }
            Page::Key | Page::Lead | Page::Bass | Page::Drums | Page::Settings => {
                if v.xruns > 0 {
                    self.text(c, &format!("XRUNS {}", v.xruns), Point::new(w - m, cy), s, RED, Alignment::Right);
                }
            }
        }
        match v.page {
            Page::Tempo => self.render_tempo(c, v),
            Page::Key => {
                if let Some(q) = &v.key {
                    self.render_key(c, v, q);
                }
            }
            Page::Lead => {
                if let Some(q) = &v.seq {
                    self.render_sequence(c, v, q);
                }
            }
            Page::Bass => {
                if let Some(q) = &v.bass {
                    self.render_bass(c, q);
                }
            }
            Page::Drums => {
                if let Some(q) = &v.drums {
                    self.render_drums(c, v, q);
                }
            }
            Page::Settings => {
                if let Some(q) = &v.settings {
                    self.render_settings(c, q);
                }
            }
        }
        // --- touch marker -----------------------------------------------------
        if let Some((x, y)) = v.touch {
            let r = m * 2;
            let ring = PrimitiveStyleBuilder::new().stroke_color(YELLOW).stroke_width(2).build();
            let _ = Circle::with_center(Point::new(x, y), (2 * r) as u32).into_styled(ring).draw(c);
            let _ = c.fill_solid(&Rectangle::new(Point::new(x - r - m, y), Size::new((2 * (r + m) + 1) as u32, 1)), YELLOW);
            let _ = c.fill_solid(&Rectangle::new(Point::new(x, y - r - m), Size::new(1, (2 * (r + m) + 1) as u32)), YELLOW);
        }
    }

    fn render_tempo(&self, c: &mut Canvas, v: &View) {
        let (w, h, m) = (self.w, self.h, self.m);
        let s = self.small;
        let cx = self.bpm_cx;
        // --- big BPM ----------------------------------------------------------
        let bpm_top = (h as f64 * 0.16) as i32;
        let bpm_cy = bpm_top + (10 * self.big) as i32;
        let bpm_color = match v.mode {
            Mode::Auto => AUTO,
            Mode::Manual => MANUAL,
        };
        self.text(c, &v.bpm, Point::new(cx, bpm_cy), self.big, bpm_color, Alignment::Center);
        self.text(c, "BPM", Point::new(cx + (25 * self.big) as i32 + m, bpm_cy + (6 * self.big) as i32), s, DIM, Alignment::Left);
        // --- info line + confidence bar -------------------------------------
        let info_y = bpm_top + (20 * self.big) as i32 + m + (10 * s) as i32;
        let left = cx - (25 * self.big) as i32;
        match v.conf_pct {
            Some(p) => {
                let bar_w = (w as f64 * 0.25) as i32;
                let bar_x = w - m - bar_w;
                let bar_h = (10 * s) as i32;
                let avail = bar_x - m - (5 * 10 * s as i32) - 2 * m - left;
                let info = if (v.info.len() as i32) * 10 * s as i32 <= avail { v.info.clone() } else { v.info.replace("TEMPO STRENGTH", "STR") };
                self.text(c, &info, Point::new(left, info_y), s, DIM, Alignment::Left);
                self.text(c, "CONF", Point::new(bar_x - m, info_y), s, DIM, Alignment::Right);
                let _ = c.fill_solid(&Rectangle::new(Point::new(bar_x, info_y - bar_h / 2), Size::new(bar_w as u32, bar_h as u32)), PANEL);
                let f = bar_w * p.min(100) as i32 / 100;
                if f > 0 {
                    let col = if p >= 60 { GREEN } else if p >= 30 { YELLOW } else { RED };
                    let _ = c.fill_solid(&Rectangle::new(Point::new(bar_x, info_y - bar_h / 2), Size::new(f as u32, bar_h as u32)), col);
                }
            }
            None => self.text(c, &v.info, Point::new(cx, info_y), s, DIM, Alignment::Center),
        }
        // --- beat lamps -------------------------------------------------------
        let lamp_y = (h as f64 * 0.60) as i32;
        let d = (h as f64 * 0.045) as i32;
        let gap = d * 2;
        let total = v.bar_len as i32 * d + (v.bar_len as i32 - 1) * gap;
        let mut x = cx - total / 2;
        for i in 0..v.bar_len {
            let on = v.lamp == Some(i);
            let col = if !on {
                PANEL
            } else if i == 0 {
                bpm_color
            } else {
                TEXT
            };
            let _ = Circle::new(Point::new(x, lamp_y - d / 2), d as u32).into_styled(PrimitiveStyle::with_fill(col)).draw(c);
            x += d + gap;
        }
        // --- stats (bottom row, right) ----------------------------------------
        let yb = (h as f64 * 0.83) as i32;
        let hb = h - m - yb;
        let k = s.max(2) - 1;
        let avail = w - m - if v.mode == Mode::Manual { self.stats_x } else { m };
        if (v.stats.len() as i32) * 10 * k as i32 <= avail {
            self.text(c, &v.stats, Point::new(w - m, yb + hb / 2), k, DIM, Alignment::Right);
        }
    }

    fn render_key(&self, c: &mut Canvas, v: &View, q: &KeyView) {
        let (w, h, m) = (self.w, self.h, self.m);
        let s = self.small;
        let cx = self.bpm_cx;
        // --- the key's name, as big as fits ------------------------------------------
        let top = (h as f64 * 0.16) as i32;
        let cy = top + (10 * self.big) as i32;
        let avail = (w - (2 * m + (w as f64 * 0.17) as i32)) - 2 * m;
        let mut k = self.big;
        while k > 1 && (q.name.len() as i32) * 10 * k as i32 > avail {
            k -= 1;
        }
        self.text(c, &q.name, Point::new(cx, cy), k, q.color, Alignment::Center);
        // --- under it: confidence bar (heard) or the notes (by hand) ------------------
        let info_y = top + (20 * self.big) as i32 + m + (10 * s) as i32;
        let left = cx - avail / 2;
        match q.conf_pct {
            Some(p) => {
                let bar_w = (w as f64 * 0.25) as i32;
                let bar_x = w - m - bar_w;
                let bar_h = (10 * s) as i32;
                let room = bar_x - m - (5 * 10 * s as i32) - 2 * m - left;
                let info = if (q.info.len() as i32) * 10 * s as i32 <= room { q.info.clone() } else { q.info.replace("TUNING ", "").replace("ALSO ", "/ ") };
                self.text(c, &info, Point::new(left, info_y), s, DIM, Alignment::Left);
                self.text(c, "CONF", Point::new(bar_x - m, info_y), s, DIM, Alignment::Right);
                let _ = c.fill_solid(&Rectangle::new(Point::new(bar_x, info_y - bar_h / 2), Size::new(bar_w as u32, bar_h as u32)), PANEL);
                let f = bar_w * p.min(100) as i32 / 100;
                if f > 0 {
                    let col = if p >= 60 { GREEN } else if p >= 30 { YELLOW } else { RED };
                    let _ = c.fill_solid(&Rectangle::new(Point::new(bar_x, info_y - bar_h / 2), Size::new(f as u32, bar_h as u32)), col);
                }
            }
            None => {
                let mut k = s;
                while k > 1 && (q.info.len() as i32) * 10 * k as i32 > avail {
                    k -= 1;
                }
                self.text(c, &q.info, Point::new(cx, info_y), k, DIM, Alignment::Center);
            }
        }
        // --- heard: the chroma strip where the root and scale buttons would be ----------
        if q.auto {
            let r = Rectangle::new(self.key_strip.top_left, Size::new(self.key_strip.size.width, (h - m - self.key_strip.top_left.y).max(1) as u32));
            let n = 12;
            let cell_w = (r.size.width as i32 - (n - 1) * m) / n;
            let corner = Size::new(m as u32, m as u32);
            for pc in 0..12usize {
                let x = r.top_left.x + pc as i32 * (cell_w + m);
                let cell = Rectangle::new(Point::new(x, r.top_left.y), Size::new(cell_w as u32, r.size.height));
                let _ = RoundedRectangle::with_equal_corners(cell, corner).into_styled(PrimitiveStyle::with_fill(PANEL)).draw(c);
                let in_key = q.heard.is_some_and(|k| k.contains(pc as u8));
                let tonic = q.heard.is_some_and(|k| k.root as usize == pc);
                let bar_h = (r.size.height as i32 - 2 * m) * q.chroma[pc] as i32 / 100;
                if bar_h > 0 {
                    let col = if tonic { q.color } else if in_key { dim(q.color) } else { PANEL_PRESSED };
                    let bar = Rectangle::new(Point::new(x + m, r.top_left.y + r.size.height as i32 - m - bar_h), Size::new((cell_w - 2 * m).max(1) as u32, bar_h as u32));
                    let _ = c.fill_solid(&bar, col);
                }
                let fg = if tonic { TEXT } else if in_key { TEXT } else { DIM };
                self.text(c, PITCH_NAMES[pc], Point::new(x + cell_w / 2, r.top_left.y + m + (5 * s) as i32), s, fg, Alignment::Center);
            }
        }
        let _ = v;
    }

    fn captions(&self, c: &mut Canvas) {
        let cap_w = (self.w as f64 * 0.07) as i32;
        let k = self.small.min((((cap_w - self.m) / 30) as u32).max(1));
        for (at, caption) in self.captions {
            self.text(c, caption, at, k, DIM, Alignment::Right);
        }
    }

    fn render_sequence(&self, c: &mut Canvas, v: &View, q: &SeqView) {
        let m = self.m;
        let gap = (m / 2).max(2);
        self.captions(c);
        let playhead = q.play.filter(|(bar, _)| *bar == q.bar).map(|(_, k)| k);
        // --- track buttons ------------------------------------------------------
        for (t, r) in self.tracks.iter().enumerate() {
            let selected = q.track == t;
            let fill = if q.muted[t] { PANEL_DISABLED } else { PANEL };
            let fg = if q.muted[t] { DIM } else { TRACK_COLORS[t] };
            self.button(c, r, &q.labels[t], fill, fg, selected.then_some(TRACK_COLORS[t]));
        }
        // --- grid -------------------------------------------------------------------
        let (gx, gy) = (self.grid.top_left.x, self.grid.top_left.y);
        let corner = Size::new((gap * 2) as u32, (gap * 2) as u32);
        for t in 0..TRACKS {
            for k in 0..STEPS {
                let x = gx + k as i32 * (self.cell_w + gap);
                let y = gy + t as i32 * (self.cell_h + gap);
                let on_playhead = playhead == Some(k);
                let cov = q.cover[t][k];
                let head = q.on[t][k];
                let join_l = cov.is_some() && if k == 0 { !head } else { q.cover[t][k - 1] == cov };
                let join_r = cov.is_some() && if k == STEPS - 1 { q.cont_right[t] } else { q.cover[t][k + 1] == cov };
                let fill = if cov.is_some() {
                    let col = shade(TRACK_COLORS[t], q.vel[t][k]);
                    if q.muted[t] {
                        DIM
                    } else if on_playhead {
                        TEXT
                    } else if head {
                        col
                    } else {
                        dim(col)
                    }
                } else if on_playhead {
                    PANEL_PRESSED
                } else if k % 4 == 0 {
                    PANEL_BEAT
                } else {
                    PANEL
                };
                let rr = note_cell(Rectangle::new(Point::new(x, y), Size::new(self.cell_w as u32, self.cell_h as u32)), gap, corner, join_l, join_r);
                let selected = q.track == t && q.selected_step == Some(k);
                let style = if selected {
                    PrimitiveStyleBuilder::new().fill_color(fill).stroke_color(YELLOW).stroke_width(3).build()
                } else {
                    PrimitiveStyle::with_fill(fill)
                };
                let _ = rr.into_styled(style).draw(c);
                if q.alt[t][k] {
                    // a dot: this step has its own note
                    let d = (gap * 2).max(4) as u32;
                    let _ = Circle::new(Point::new(x + self.cell_w - gap - d as i32, y + gap), d)
                        .into_styled(PrimitiveStyle::with_fill(BG))
                        .draw(c);
                }
            }
        }
        // --- parameter labels between their -/+ buttons ---------------------
        let yp = (self.h as f64 * 0.86) as i32;
        let hp = self.h - m - yp;
        let group_w = param_group_w(self.w, m, SEQ_GROUPS);
        for (i, label) in q.params.iter().enumerate() {
            let gx = m + i as i32 * (group_w + m);
            let r = Rectangle::new(Point::new(gx + group_w / 4 + gap, yp), Size::new((group_w / 2 - 2 * gap) as u32, hp as u32));
            let accent = if q.edit && q.selected_step.is_some() { YELLOW } else { TRACK_COLORS[q.track] };
            self.button(c, &r, label, PANEL_DISABLED, accent, None);
        }
        let _ = v;
    }

    fn render_bass(&self, c: &mut Canvas, q: &BassView) {
        let m = self.m;
        let gap = (m / 2).max(2);
        let s = self.small;
        self.captions(c);
        // --- pitch grid ------------------------------------------------------------
        let (gx, gy) = (self.bass_grid.top_left.x, self.bass_grid.top_left.y);
        let (cw, ch) = (self.bass_cell_w, self.bass_cell_h);
        let corner = Size::new((gap * 2) as u32, (gap * 2) as u32);
        let playhead = q.play.filter(|(bar, _)| *bar == q.bar).map(|(_, k)| k);
        let black = |r: usize| matches!((q.base as usize + r) % 12, 1 | 3 | 6 | 8 | 10);
        // --- scroll arrows, greyed out at the ends of the MIDI range -----------
        for (b, r) in &self.buttons {
            if let Button::BassScroll(d) = b {
                let up = *d > 0;
                let at_end = if up { q.base >= BASS_TOP } else { q.base == 0 };
                let c0 = r.center();
                let ah = ((r.size.height as i32 - 2 * m) / 3).clamp(m, 3 * m);
                let aw = ((r.size.width as i32 - 2 * m) / 2).max(m / 2);
                let (top, bottom) = if up { (c0.y - ah / 2, c0.y + ah / 2) } else { (c0.y + ah / 2, c0.y - ah / 2) };
                let tri = Triangle::new(Point::new(c0.x, top), Point::new(c0.x - aw, bottom), Point::new(c0.x + aw, bottom));
                let _ = tri.into_styled(PrimitiveStyle::with_fill(if at_end { DIM } else { TEXT })).draw(c);
            }
        }
        for r in 0..BASS_ROWS {
            let y = gy + (BASS_ROWS - 1 - r) as i32 * (ch + gap);
            // note name, as large as the row pitch allows, tight against the arrows
            let k = s.min((((ch + gap) as u32) / 20).max(1));
            self.text(c, &q.labels[r], Point::new(self.bass_labels.top_left.x, y + ch / 2), k, if black(r) { DIM } else { TEXT }, Alignment::Left);
            for k in 0..STEPS {
                let x = gx + k as i32 * (cw + gap);
                let note = q.base + r as u8;
                let on = q.notes[k] == Some(note);
                let cov = q.cover[k].filter(|_| on);
                let head = cov == Some((q.bar * STEPS + k) as u16);
                let join_l = cov.is_some() && if k == 0 { !head } else { q.cover[k - 1] == cov };
                let join_r = cov.is_some() && if k == STEPS - 1 { q.cont_right } else { q.cover[k + 1] == cov };
                let on_playhead = playhead == Some(k);
                let fill = if on {
                    let col = shade(BASS, q.vel[k]);
                    if on_playhead {
                        TEXT
                    } else if head {
                        col
                    } else {
                        dim(col)
                    }
                } else if on_playhead {
                    PANEL_PRESSED
                } else if black(r) {
                    PANEL_DISABLED
                } else if k % 4 == 0 {
                    PANEL_BEAT
                } else {
                    PANEL
                };
                let rr = note_cell(Rectangle::new(Point::new(x, y), Size::new(cw as u32, ch as u32)), gap, corner, join_l, join_r);
                let _ = rr.into_styled(PrimitiveStyle::with_fill(fill)).draw(c);
                // a note outside the shown octave: a bar at the top or bottom edge
                if let Some(n) = q.notes[k] {
                    let below = r == 0 && n < q.base;
                    let above = r == BASS_ROWS - 1 && n >= q.base + BASS_ROWS as u8;
                    if below || above {
                        let ey = if below { y + ch - gap } else { y };
                        let _ = c.fill_solid(&Rectangle::new(Point::new(x + gap, ey), Size::new((cw - 2 * gap).max(1) as u32, gap as u32)), BASS);
                    }
                }
            }
        }
        // --- parameter labels between their -/+ buttons ---------------------
        let yp = (self.h as f64 * 0.86) as i32;
        let hp = self.h - m - yp;
        let group_w = param_group_w(self.w, m, BASS_GROUPS);
        for (i, label) in q.params.iter().enumerate() {
            let gx = m + i as i32 * (group_w + m);
            let r = Rectangle::new(Point::new(gx + group_w / 4 + gap, yp), Size::new((group_w / 2 - 2 * gap) as u32, hp as u32));
            self.button(c, &r, label, PANEL_DISABLED, BASS, None);
        }
    }

    fn render_drums(&self, c: &mut Canvas, v: &View, q: &DrumView) {
        let m = self.m;
        let gap = (m / 2).max(2);
        self.captions(c);
        let playhead = q.play.filter(|(bar, _)| *bar == q.bar).map(|(_, k)| k);
        let corner = Size::new(m as u32, m as u32);
        for (b, r) in &self.buttons {
            let pressed = v.pressed == Some(*b);
            match b {
                // --- step row: the selected instrument, as the LEDs of a
                // DrumBrute: dim for a hit, bright for an accent -----------------
                Button::DrumStep(k) => {
                    let k = *k;
                    let st = q.steps[k];
                    let col = DRUM_COLORS[q.drum];
                    let on_playhead = playhead == Some(k);
                    let fill = if st > 0 {
                        if q.muted[q.drum] {
                            DIM
                        } else if on_playhead {
                            TEXT
                        } else if st == 2 {
                            col
                        } else {
                            dim(col)
                        }
                    } else if on_playhead || pressed {
                        PANEL_PRESSED
                    } else if k % 4 == 0 {
                        PANEL_BEAT
                    } else {
                        PANEL
                    };
                    let _ = RoundedRectangle::with_equal_corners(*r, corner).into_styled(PrimitiveStyle::with_fill(fill)).draw(c);
                    if self.drum_step_w >= 30 {
                        let at = Point::new(r.top_left.x + gap + 1, r.top_left.y + gap + 5);
                        self.text(c, &(k + 1).to_string(), at, 1, if st > 0 { BG } else { DIM }, Alignment::Left);
                    }
                }
                // --- pads: name, and a dot per step of the shown bar ---------------
                Button::DrumPad(i) => {
                    let i = *i;
                    let col = DRUM_COLORS[i];
                    let fill = if q.muted[i] {
                        PANEL_DISABLED
                    } else if pressed || q.lit[i] {
                        PANEL_PRESSED
                    } else {
                        PANEL
                    };
                    let fg = if q.muted[i] {
                        DIM
                    } else if q.solo[i] {
                        GREEN
                    } else {
                        col
                    };
                    let rr = RoundedRectangle::with_equal_corners(*r, corner);
                    let style = if q.drum == i {
                        PrimitiveStyleBuilder::new().fill_color(fill).stroke_color(col).stroke_width(3).build()
                    } else {
                        PrimitiveStyle::with_fill(fill)
                    };
                    let _ = rr.into_styled(style).draw(c);
                    let (rw, rh) = (r.size.width as i32, r.size.height as i32);
                    let label = &q.labels[i];
                    let mut k = self.small;
                    while k > 1 && (label.len() as i32) * 10 * k as i32 > rw - 2 * m {
                        k -= 1;
                    }
                    // dots along the bottom edge, if the pad is wide enough
                    let pitch = (rw - 2 * m) / STEPS as i32;
                    let dot_h = if pitch >= 4 { (pitch - 2).min(rh / 6).max(2) } else { 0 };
                    let text_cy = r.top_left.y + (rh - dot_h - m / 2) / 2;
                    self.text(c, label, Point::new(r.center().x, text_cy), k, fg, Alignment::Center);
                    if dot_h > 0 {
                        let dot_y = r.top_left.y + rh - m - dot_h;
                        let x0 = r.top_left.x + (rw - pitch * STEPS as i32) / 2;
                        for s in 0..STEPS {
                            let d = q.dots[i][s];
                            let on_playhead = playhead == Some(s);
                            let dc = if d > 0 {
                                if q.muted[i] {
                                    DIM
                                } else if on_playhead {
                                    TEXT
                                } else if d == 2 {
                                    col
                                } else {
                                    dim(col)
                                }
                            } else if on_playhead {
                                PANEL_PRESSED
                            } else {
                                BG
                            };
                            let dr = Rectangle::new(Point::new(x0 + s as i32 * pitch + 1, dot_y), Size::new((pitch - 2).max(1) as u32, dot_h as u32));
                            let _ = c.fill_solid(&dr, dc);
                        }
                    }
                }
                _ => {}
            }
        }
        // --- parameter labels between their -/+ buttons ---------------------
        let yp = (self.h as f64 * 0.86) as i32;
        let hp = self.h - m - yp;
        let group_w = param_group_w(self.w, m, DRUM_PARAMS.len());
        for (i, label) in q.params.iter().enumerate() {
            let gx = m + i as i32 * (group_w + m);
            let r = Rectangle::new(Point::new(gx + group_w / 4 + gap, yp), Size::new((group_w / 2 - 2 * gap) as u32, hp as u32));
            self.button(c, &r, label, PANEL_DISABLED, DRUM_COLORS[q.drum], None);
        }
    }

    fn render_settings(&self, c: &mut Canvas, q: &SettingsView) {
        // The -/+ buttons are drawn with the other buttons; here the name of
        // each instrument and its channel between them, in the instrument's
        // colour as the parameter readouts of the other pages.
        let k = self.small;
        for ((name_r, value_r), row) in self.setup_rows.iter().zip(&q.rows) {
            self.text(c, row.name, Point::new(name_r.top_left.x + self.m / 2, name_r.center().y), k, row.color, Alignment::Left);
            let rr = RoundedRectangle::with_equal_corners(*value_r, Size::new(self.m as u32, self.m as u32));
            let _ = rr.into_styled(PrimitiveStyle::with_fill(PANEL_DISABLED)).draw(c);
            self.text(c, &row.value, value_r.center(), k, row.color, Alignment::Center);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{PixelFormat, Rotation};

    fn status() -> Status {
        let mut seq = SeqState::default();
        seq.apply(sequencer::SeqAction::ToggleStep(0, 0));
        seq.apply(sequencer::SeqAction::ToggleStep(1, 4));
        seq.apply(sequencer::SeqAction::CellDown(2, 6)); // open hat ringing 4 steps
        seq.apply(sequencer::SeqAction::CellDrag(9));
        seq.apply(sequencer::SeqAction::CellUp);
        seq.apply(sequencer::SeqAction::BassSet(0, 0));
        seq.apply(sequencer::SeqAction::BassSet(7, 8));
        seq.apply(sequencer::SeqAction::BassScroll(-2)); // window A#0..A1, C1 still on step 0
        Status {
            mode: Mode::Auto,
            live: Grid { running: true, t0: 100.0, n0: 0, period: 0.5, state: State::Running, confidence: 0.8, ..Grid::default() },
            tracker: Grid { running: true, t0: 100.0, n0: 0, period: 0.5, state: State::Running, confidence: 0.8, ..Grid::default() },
            tempo_strength: 0.4,
            level_db: -18.0,
            xruns: 0,
            ticks: 12345,
            midi: true,
            taps: 0,
            now: 103.02,
            seq: std::sync::Arc::new(seq),
            key: None,
        }
    }

    fn heard(tonic: u8, mode: key::Mode, confidence: f32) -> key::KeyEstimate {
        let mut chroma = [0.2f32; 12];
        chroma[tonic as usize] = 1.0;
        key::KeyEstimate { tonic, mode, confidence, tuning_cents: 6.0, chroma, alt: None, frames: 10 }
    }

    #[test]
    fn buttons_do_not_overlap_and_fit() {
        for (w, h) in [(800usize, 480usize), (480, 320), (1024, 600), (1280, 800), (320, 240)] {
            let s = Screen::new(w, h);
            let bb = Rectangle::new(Point::zero(), Size::new(w as u32, h as u32));
            for page in [Page::Tempo, Page::Key, Page::Lead, Page::Bass, Page::Drums, Page::Settings] {
                let on_page: Vec<_> = s.buttons.iter().filter(|(b, _)| b.page().map_or(true, |p| p == page)).collect();
                for (i, (b, r)) in on_page.iter().enumerate() {
                    assert_eq!(r.intersection(&bb), *r, "{b:?} outside {w}x{h}");
                    for (b2, r2) in &on_page[i + 1..] {
                        assert!(r.intersection(r2).is_zero_sized(), "{b:?} overlaps {b2:?} at {w}x{h}");
                    }
                }
            }
            assert_eq!(s.hit(w as i32 / 2, (h as f64 * 0.72) as i32, Page::Tempo), Some(Hit::Button(Button::Tap)));
            // the six tabs in a row across the top, all inside the screen
            let tabs: Vec<_> = TABS.iter().map(|(b, _, _)| s.buttons.iter().find(|(x, _)| x == b).unwrap().1).collect();
            for (t1, t2) in tabs.iter().zip(&tabs[1..]) {
                assert!(t2.top_left.x >= t1.top_left.x + t1.size.width as i32 && t2.top_left.y == t1.top_left.y);
            }
            let last = tabs[TABS.len() - 1];
            assert!(last.top_left.x + (last.size.width as i32) <= w as i32 - s.m, "tabs run off the screen at {w}x{h}");
            let tc = last.center();
            assert_eq!(s.hit(tc.x, tc.y, Page::Drums), Some(Hit::Button(Button::PageSettings)));
            let kc = tabs[1].center();
            assert_eq!(s.hit(kc.x, kc.y, Page::Tempo), Some(Hit::Button(Button::PageKey)));
            // key page: the twelve roots in a row across the width, the scales
            // in two rows underneath, all inside the screen
            let at = |b: Button| s.buttons.iter().find(|(x, _)| *x == b).unwrap().1;
            let (r0, r11) = (at(Button::KeyRoot(0)), at(Button::KeyRoot(11)));
            assert_eq!(r0.top_left.x, s.m);
            assert!(r11.top_left.x + (r11.size.width as i32) <= w as i32 - s.m);
            assert_eq!(r0.top_left.y, r11.top_left.y);
            assert_eq!(s.key_strip.top_left, r0.top_left);
            let (s0, s6, s7) = (at(Button::KeyScale(Scale::Major)), at(Button::KeyScale(Scale::Locrian)), at(Button::KeyScale(Scale::HarmonicMinor)));
            assert!(s0.top_left.y > r0.top_left.y + r0.size.height as i32);
            assert_eq!(s0.top_left.y, s6.top_left.y);
            assert!(s7.top_left.y > s0.top_left.y);
            assert!(s0.size.height as i32 >= (h as i32) / 12, "scale buttons too flat at {w}x{h}");
            let c = s7.center();
            assert_eq!(s.hit(c.x, c.y, Page::Key), Some(Hit::Button(Button::KeyScale(Scale::HarmonicMinor))));
            assert!(!matches!(s.hit(c.x, c.y, Page::Tempo), Some(Hit::Button(Button::KeyScale(_)))));
            let a = at(Button::KeyAuto);
            assert_eq!(a, at(Button::ModeAuto), "the key page's mode buttons sit where the tempo page's do");
            // the generator pair at the right of the bottom row of the lead and bass pages
            for inst in [Instrument::Lead, Instrument::Bass] {
                let (rnd, cpt) = (at(Button::Generate(inst, Style::Random)), at(Button::Generate(inst, Style::Counterpoint)));
                assert!(rnd.top_left.x + (rnd.size.width as i32) <= cpt.top_left.x && rnd.top_left.y == cpt.top_left.y);
                assert_eq!(cpt.top_left.x + (cpt.size.width as i32), w as i32 - s.m, "{inst:?} pair does not reach the right edge at {w}x{h}");
                assert!(3 * 10 <= rnd.size.width as i32 - s.m, "RND does not fit at {w}x{h}");
            }
            let vp = at(Button::BassPlus(BassParam::Velocity));
            let rnd = at(Button::Generate(Instrument::Bass, Style::Random));
            assert!(vp.top_left.x + (vp.size.width as i32) <= rnd.top_left.x);
            // settings: one row per instrument, name, -, readout, + across the width
            for (i, inst) in Instrument::all().enumerate() {
                let (minus, plus) = (at(Button::ChannelMinus(inst)), at(Button::ChannelPlus(inst)));
                let (name, value) = s.setup_rows[i];
                assert!(name.top_left.x + (name.size.width as i32) <= minus.top_left.x && name.top_left.y == minus.top_left.y);
                assert!(minus.top_left.x + (minus.size.width as i32) <= value.top_left.x);
                assert!(value.top_left.x + (value.size.width as i32) <= plus.top_left.x);
                assert_eq!(plus.top_left.x + (plus.size.width as i32), w as i32 - s.m, "{inst:?} row does not reach the right edge at {w}x{h}");
                assert!(minus.size.height as i32 >= (h as i32) / 10, "{inst:?} channel buttons too flat at {w}x{h}");
                assert!(5 * 10 * s.small as i32 <= name.size.width as i32 - s.m, "name does not fit at {w}x{h}");
                assert!(5 * 10 * s.small as i32 <= value.size.width as i32, "readout does not fit at {w}x{h}");
                if i > 0 {
                    assert!(name.top_left.y >= s.setup_rows[i - 1].0.top_left.y + s.setup_rows[i - 1].0.size.height as i32);
                }
                let (mc, pc) = (minus.center(), plus.center());
                assert_eq!(s.hit(mc.x, mc.y, Page::Settings), Some(Hit::Button(Button::ChannelMinus(inst))));
                assert_eq!(s.hit(pc.x, pc.y, Page::Settings), Some(Hit::Button(Button::ChannelPlus(inst))));
                assert!(!matches!(s.hit(pc.x, pc.y, Page::Lead), Some(Hit::Button(Button::ChannelPlus(_)))));
            }
            // grid: the top-left cell and the bottom-right cell
            let g = s.grid;
            assert_eq!(s.hit(g.top_left.x + 1, g.top_left.y + 1, Page::Lead), Some(Hit::Cell(0, 0)));
            let br = g.bottom_right().unwrap();
            assert_eq!(s.hit(br.x - 1, br.y - 1, Page::Lead), Some(Hit::Cell(TRACKS - 1, STEPS - 1)));
            assert!(!matches!(s.hit(g.top_left.x + 1, g.top_left.y + 1, Page::Tempo), Some(Hit::Cell(..))));
            assert!(matches!(s.hit(s.tracks[3].center().x, s.tracks[3].center().y, Page::Lead), Some(Hit::Track(3))));
            // bass grid: the bottom row is the base note, the top row the 12th
            let g = s.bass_grid;
            assert_eq!(s.hit(g.top_left.x + 1, g.top_left.y + 1, Page::Bass), Some(Hit::BassCell(BASS_ROWS - 1, 0)));
            let br = g.bottom_right().unwrap();
            assert_eq!(s.hit(br.x - 1, br.y - 1, Page::Bass), Some(Hit::BassCell(0, STEPS - 1)));
            let l = s.bass_labels.center();
            assert_eq!(s.hit(l.x, l.y, Page::Bass), Some(Hit::BassLabels));
            assert_eq!(s.hit(s.m + 1, g.top_left.y + 1, Page::Bass), Some(Hit::Button(Button::BassScroll(1))));
            assert_eq!(s.hit(s.m + 1, br.y - 1, Page::Bass), Some(Hit::Button(Button::BassScroll(-1))));
            assert!(s.bass_row_pitch() > 0);
            // the padlock sits right of BAR 8 on both pages
            let (_, r8) = s.buttons.iter().find(|(b, _)| *b == Button::Bar(7)).unwrap();
            let (_, rl) = s.buttons.iter().find(|(b, _)| *b == Button::BarLock).unwrap();
            assert!(rl.top_left.x > r8.top_left.x && rl.top_left.y == r8.top_left.y, "padlock next to BAR 8 at {w}x{h}");
            let lc = rl.center();
            assert_eq!(s.hit(lc.x, lc.y, Page::Lead), Some(Hit::Button(Button::BarLock)));
            assert_eq!(s.hit(lc.x, lc.y, Page::Bass), Some(Hit::Button(Button::BassBarLock)));
            assert_eq!(s.hit(lc.x, lc.y, Page::Drums), Some(Hit::Button(Button::DrumBarLock)));
            // drums: pads in two rows of five, the step row underneath them
            let (p0, p5, p9) = (at(Button::DrumPad(0)), at(Button::DrumPad(5)), at(Button::DrumPad(9)));
            assert_eq!(p0.top_left.x, p5.top_left.x);
            assert!(p5.top_left.y > p0.top_left.y && p9.top_left.x > p5.top_left.x);
            let st0 = at(Button::DrumStep(0));
            assert!(st0.top_left.y > p9.top_left.y + p9.size.height as i32);
            assert_eq!(s.hit(p0.center().x, p0.center().y, Page::Drums), Some(Hit::Button(Button::DrumPad(0))));
            assert!(!matches!(s.hit(p0.center().x, p0.center().y, Page::Lead), Some(Hit::Button(Button::DrumPad(_)))));
            let s15 = at(Button::DrumStep(15));
            assert_eq!(s.hit(s15.center().x, s15.center().y, Page::Drums), Some(Hit::Button(Button::DrumStep(15))));
            assert!(s15.top_left.x + (s15.size.width as i32) <= w as i32 - s.m);
            assert_eq!(p0.top_left.x, s.m);
            assert!(!g.intersection(&bb).is_zero_sized() && g.intersection(&bb) == g, "bass grid outside {w}x{h}");
            for (b, r) in &s.buttons {
                if b.page() == Some(Page::Bass) {
                    assert!(r.intersection(&g).is_zero_sized(), "{b:?} overlaps the bass grid at {w}x{h}");
                }
                if b.page() == Some(Page::Lead) {
                    assert!(r.intersection(&s.grid).is_zero_sized(), "{b:?} overlaps the grid at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn view_changes_only_when_something_visible_changes() {
        let s = status();
        let a = View::build(&s, None);
        let mut s2 = s.clone();
        s2.ticks += 7;
        s2.level_db += 0.3;
        assert_eq!(a, View::build(&s2, None));
        assert!(!a.key_known && a.key_auto);
        // a heard key changes nothing on the tempo page, but the generators come alive
        s2.key = Some(heard(9, key::Mode::Minor, 0.8));
        let a2 = View::build(&s2, None);
        assert!(a2.key_known);
        assert_ne!(a, a2);
        s2.now += 0.2; // lamp goes off
        assert_ne!(a, View::build(&s2, None));
        assert_eq!(a.lamp, Some(2));
        // sequence page: the playhead moves every 16th
        let b = View::build_with_touch(&s, Page::Lead, None, None);
        assert_eq!(b.seq.as_ref().unwrap().play, Some((0, 8)));
        let mut s3 = s.clone();
        s3.now += 0.13;
        assert_ne!(b, View::build_with_touch(&s3, Page::Lead, None, None));
        // bass page: the phrase position is (bar, step); the screen follows
        // the playing bar (bar 2 here) until a bar is picked
        let d = View::build_with_touch(&s, Page::Bass, None, None);
        assert_eq!(d.bass.as_ref().unwrap().play, Some((1, 8)));
        assert_eq!(d.bass_bars(), Some((1, Some(1))));
        assert_eq!(d.bass.as_ref().unwrap().notes[8], None, "bar 2 (empty) is on screen");
        assert_ne!(d, View::build_with_touch(&s3, Page::Bass, None, None));
        let mut s4 = s.clone();
        let mut seq = (*s4.seq).clone();
        seq.apply(sequencer::SeqAction::BassBar(0));
        s4.seq = std::sync::Arc::new(seq);
        let e = View::build_with_touch(&s4, Page::Bass, None, None);
        assert_eq!(e.bass_bars(), Some((0, Some(1))));
        assert!(e.bass.as_ref().unwrap().locked);
        assert_eq!(e.bass.as_ref().unwrap().notes[8], Some(43), "bar 1 is on screen");
        // stopped clock: the picked bar shows
        let mut s5 = s.clone();
        s5.live.running = false;
        assert_eq!(View::build_with_touch(&s5, Page::Bass, None, None).bass_bars(), Some((0, None)));
    }

    #[test]
    fn key_view_shows_what_is_heard_or_set() {
        let mut st = status();
        let v = View::build_with_touch(&st, Page::Key, None, None);
        let q = v.key.as_ref().unwrap();
        assert!(q.auto && q.key.is_none());
        assert_eq!((q.name.as_str(), q.color, q.conf_pct), ("--", DIM, None));
        assert_eq!(q.info, "LISTENING");
        // a confident key: named, in colour, with the tuning and the chroma
        st.key = Some(heard(9, key::Mode::Minor, 0.8));
        let v2 = View::build_with_touch(&st, Page::Key, None, None);
        let q = v2.key.as_ref().unwrap();
        assert_eq!(q.key, Some(Key::new(9, Scale::Minor)));
        assert_eq!((q.name.as_str(), q.color, q.conf_pct), ("A MINOR", AUTO, Some(80)));
        assert_eq!(q.info, "TUNING +6c");
        assert_eq!(q.chroma[9], 100);
        assert_eq!(q.heard, q.key);
        assert!(v2.key_known);
        // a doubtful one is shown dim and not used
        st.key = Some(key::KeyEstimate { confidence: 0.1, alt: Some((0, key::Mode::Major)), ..heard(9, key::Mode::Minor, 0.1) });
        let v3 = View::build_with_touch(&st, Page::Key, None, None);
        let q = v3.key.as_ref().unwrap();
        assert!(q.key.is_none() && !v3.key_known);
        assert_eq!((q.name.as_str(), q.color), ("A MINOR", DIM));
        assert_eq!(q.info, "TUNING +6c   ALSO C MAJOR");
        assert!(q.heard.is_some());
        // by hand: the set key, its notes, no confidence
        let mut seq = (*st.seq).clone();
        seq.apply(sequencer::SeqAction::KeyRoot(2));
        seq.apply(sequencer::SeqAction::KeyScale(Scale::Dorian));
        st.seq = std::sync::Arc::new(seq);
        let v4 = View::build_with_touch(&st, Page::Key, None, None);
        let q = v4.key.as_ref().unwrap();
        assert!(!q.auto && !v4.key_auto && v4.key_known);
        assert_eq!(q.key, Some(Key::new(2, Scale::Dorian)));
        assert_eq!((q.name.as_str(), q.color, q.conf_pct), ("D DORIAN", MANUAL, None));
        assert_eq!(q.info, "NOTES C D E F G A B");
        assert_eq!(effective_key(&st.seq.key, st.key.as_ref()), Some(Key::new(2, Scale::Dorian)));
        // the detector's changes no longer change the view
        st.key = None;
        assert_eq!(View::build_with_touch(&st, Page::Key, None, None), v4);
        // renders at every size, heard and by hand
        for (w, h) in [(800usize, 480usize), (320, 240), (1280, 800), (480, 320)] {
            let s = Screen::new(w, h);
            let mut c = Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0);
            s.render(&mut c, &v2);
            s.render(&mut c, &v4);
        }
    }

    #[test]
    fn velocity_shades_the_cells() {
        let s = status();
        let mut seq = (*s.seq).clone();
        seq.apply(sequencer::SeqAction::CellDown(1, 4));
        seq.apply(sequencer::SeqAction::CellVel(-60));
        seq.apply(sequencer::SeqAction::CellUp);
        seq.apply(sequencer::SeqAction::BassBar(0));
        seq.apply(sequencer::SeqAction::BassCellDown(2, 0)); // C1 in the scrolled window (row 2)
        seq.apply(sequencer::SeqAction::BassCellVel(27));
        seq.apply(sequencer::SeqAction::BassCellUp);
        let st = Status { seq: std::sync::Arc::new(seq), ..s };
        let q = View::build_with_touch(&st, Page::Lead, None, None).seq.unwrap();
        assert_eq!(q.vel[1][4], 40);
        assert_eq!(q.vel[0][0], 100);
        assert_eq!(q.vel[0][1], 0, "no note");
        assert_eq!(q.vel[2][6..10], [100; 4], "the tail carries the head's velocity");
        let b = View::build_with_touch(&st, Page::Bass, None, None).bass.unwrap();
        assert_eq!(b.vel[0], 127);
        assert!(shade(BASS, 127) == BASS && shade(BASS, 1).r() < BASS.r() / 2 && shade(BASS, 64).g() < BASS.g());
    }

    #[test]
    fn long_notes_show_joined_and_continue_over_the_bar() {
        let s = status();
        let v = View::build_with_touch(&s, Page::Lead, None, None);
        let q = v.seq.as_ref().unwrap();
        assert!(q.on[2][6] && !q.on[2][7]);
        assert_eq!(q.cover[2][6..10], [Some(6); 4]);
        assert_eq!(q.cover[2][10], None);
        // a bass note from step 14 into bar 2 (the phrase is 2 bars)
        let mut seq = (*s.seq).clone();
        seq.apply(sequencer::SeqAction::BassBar(0));
        seq.apply(sequencer::SeqAction::BassCellDown(0, 14));
        seq.apply(sequencer::SeqAction::BassCellDrag(15));
        seq.apply(sequencer::SeqAction::BassCellDragBar(1));
        seq.apply(sequencer::SeqAction::BassCellDrag(2));
        seq.apply(sequencer::SeqAction::BassCellUp);
        assert_eq!(seq.bass_bar, 1);
        let s2 = Status { seq: std::sync::Arc::new(seq.clone()), ..s.clone() };
        let b = View::build_with_touch(&s2, Page::Bass, None, None).bass.unwrap();
        assert_eq!(b.cover[0..3], [Some(14); 3]);
        assert_eq!(b.notes[0], Some(34), "row 0 of the scrolled window");
        assert!(!b.cont_right);
        seq.apply(sequencer::SeqAction::BassBar(0));
        let s3 = Status { seq: std::sync::Arc::new(seq), ..s };
        let b = View::build_with_touch(&s3, Page::Bass, None, None).bass.unwrap();
        assert_eq!(b.cover[14], Some(14));
        assert!(b.cont_right);
    }

    #[test]
    fn a_drag_on_the_grid_sets_a_length() {
        use crate::{Action, Session, TouchEvent, UiConfig};
        use sequencer::SeqAction::*;
        let (w, h) = (800usize, 480usize);
        let screen = Screen::new(w, h);
        let mut ses = Session::new(&UiConfig::default(), Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0));
        ses.set_page(Page::Lead);
        let gap = (screen.m / 2).max(2);
        let cell = |t: usize, k: usize| {
            (screen.grid.top_left.x + k as i32 * (screen.cell_w + gap) + screen.cell_w / 2, screen.grid.top_left.y + t as i32 * (screen.cell_h + gap) + screen.cell_h / 2)
        };
        let mut got = Vec::new();
        let mut act = |a: Action| got.push(a);
        let (x, y) = cell(2, 3);
        ses.touch(TouchEvent::Down { x, y, t: 1.0 }, Mode::Auto, &mut act);
        let (x, y) = cell(2, 6);
        ses.touch(TouchEvent::Move { x, y, t: 1.1 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x: x + 1, y, t: 1.2 }, Mode::Auto, &mut act);
        // to the right edge and hold: the next bar
        let (x, y) = cell(2, 15);
        let x = x + screen.cell_w / 2 - 1;
        ses.touch(TouchEvent::Move { x, y, t: 1.3 }, Mode::Auto, &mut act);
        ses.tick(1.5, &mut act);
        ses.tick(2.0, &mut act);
        ses.touch(TouchEvent::Up { t: 2.1 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(CellDown(2, 3)), Action::Seq(CellDrag(6)), Action::Seq(CellDrag(15)), Action::Seq(CellDragBar(1)), Action::Seq(CellUp)]);
        // up and down on a cell sets the velocity: one step per `vel_pixels`
        got.clear();
        let mut act = |a: Action| got.push(a);
        let vp = screen.vel_pixels();
        let (x, y) = cell(1, 2);
        ses.touch(TouchEvent::Down { x, y, t: 2.5 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y - 5 * vp, t: 2.6 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y - 5 * vp + 1, t: 2.65 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y + 3 * vp, t: 2.7 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 2.8 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(CellDown(1, 2)), Action::Seq(CellVel(5)), Action::Seq(CellVel(-8)), Action::Seq(CellUp)]);
        // the bass grid likewise
        got.clear();
        let mut act = |a: Action| got.push(a);
        ses.set_page(Page::Bass);
        let bcell = |r: usize, k: usize| {
            (
                screen.bass_grid.top_left.x + k as i32 * (screen.bass_cell_w + gap) + screen.bass_cell_w / 2,
                screen.bass_grid.top_left.y + (BASS_ROWS - 1 - r) as i32 * (screen.bass_cell_h + gap) + screen.bass_cell_h / 2,
            )
        };
        let (x, y) = bcell(4, 2);
        ses.touch(TouchEvent::Down { x, y, t: 3.0 }, Mode::Auto, &mut act);
        let (x, y) = bcell(4, 5);
        ses.touch(TouchEvent::Move { x, y, t: 3.1 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 3.2 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(BassCellDown(4, 2)), Action::Seq(BassCellDrag(5)), Action::Seq(BassCellUp)]);
        // a tap (finger lifted where it landed) sets the note
        got.clear();
        let mut act = |a: Action| got.push(a);
        let (x, y) = bcell(3, 7);
        ses.touch(TouchEvent::Down { x, y, t: 4.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x: x + 2, y: y + 1, t: 4.05 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 4.1 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(BassSet(3, 7))]);
        // dragging up and down on the grid scrolls the pitch window, one
        // semitone per row of travel, and leaves the cell alone
        got.clear();
        let mut act = |a: Action| got.push(a);
        let (x, y) = bcell(3, 7);
        let pitch = screen.bass_row_pitch();
        ses.touch(TouchEvent::Down { x, y, t: 5.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x: x + 3, y: y + pitch / 2 + 1, t: 5.1 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x: x + 3, y: y + 2 * pitch + 1, t: 5.2 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y - pitch, t: 5.3 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 5.4 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(BassScroll(2)), Action::Seq(BassScroll(-3))]);
        // ... unless the cell holds a note: then it is the note's velocity.
        // The session learns the notes from the frame.
        got.clear();
        let mut act = |a: Action| got.push(a);
        let mut seq = SeqState::default();
        seq.apply(BassSet(3, 7));
        let st = Status { seq: std::sync::Arc::new(seq), ..status() };
        ses.frame(st.now, &st, &mut act);
        got.clear();
        let mut act = |a: Action| got.push(a);
        let (x, y) = bcell(3, 7);
        ses.touch(TouchEvent::Down { x, y, t: 6.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y - 2 * pitch, t: 6.1 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 6.2 }, Mode::Auto, &mut act);
        assert_eq!(got[0], Action::Seq(BassCellDown(3, 7)));
        assert!(matches!(got[1], Action::Seq(BassCellVel(d)) if d > 0), "{got:?}");
        assert_eq!(got.last(), Some(&Action::Seq(BassCellUp)));
        // the empty row above still scrolls
        got.clear();
        let mut act = |a: Action| got.push(a);
        let (x, y) = bcell(4, 7);
        ses.touch(TouchEvent::Down { x, y, t: 7.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Move { x, y: y + pitch, t: 7.1 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 7.2 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(BassScroll(1))]);
        // the generator buttons are dead without a key and alive with one,
        // seeded from the press time; the tempo buttons are gone in AUTO
        got.clear();
        let mut act = |a: Action| got.push(a);
        let rnd = screen.buttons.iter().find(|(b, _)| *b == Button::Generate(Instrument::Bass, Style::Random)).unwrap().1.center();
        ses.touch(TouchEvent::Down { x: rnd.x, y: rnd.y, t: 8.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 8.1 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![]);
        let mut act = |a: Action| got.push(a);
        let st = Status { key: Some(heard(0, key::Mode::Major, 0.9)), ..st };
        ses.frame(st.now, &st, &mut act);
        got.clear();
        let mut act = |a: Action| got.push(a);
        ses.touch(TouchEvent::Down { x: rnd.x, y: rnd.y, t: 8.5 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 8.6 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Down { x: rnd.x, y: rnd.y, t: 8.9 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 9.0 }, Mode::Auto, &mut act);
        assert!(matches!(got[..], [Action::Generate(Instrument::Bass, Style::Random, a), Action::Generate(Instrument::Bass, Style::Random, b)] if a != b), "{got:?}");
        got.clear();
        let mut act = |a: Action| got.push(a);
        ses.set_page(Page::Tempo);
        let tap = screen.buttons.iter().find(|(b, _)| *b == Button::Tap).unwrap().1.center();
        ses.touch(TouchEvent::Down { x: tap.x, y: tap.y, t: 9.5 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 9.6 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![]);
        let mut act = |a: Action| got.push(a);
        ses.touch(TouchEvent::Down { x: tap.x, y: tap.y, t: 9.7 }, Mode::Manual, &mut act);
        ses.touch(TouchEvent::Up { t: 9.8 }, Mode::Manual, &mut act);
        assert_eq!(got, vec![Action::Tap(9.7 - UiConfig::default().tap_offset_s)]);
        // the key page: roots and scales only by hand
        got.clear();
        let mut act = |a: Action| got.push(a);
        ses.set_page(Page::Key);
        let root = screen.buttons.iter().find(|(b, _)| *b == Button::KeyRoot(7)).unwrap().1.center();
        ses.touch(TouchEvent::Down { x: root.x, y: root.y, t: 10.0 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 10.1 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![], "roots are hidden while the key is heard");
        let mut act = |a: Action| got.push(a);
        let man = screen.buttons.iter().find(|(b, _)| *b == Button::KeyManual).unwrap().1.center();
        ses.touch(TouchEvent::Down { x: man.x, y: man.y, t: 10.2 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 10.3 }, Mode::Auto, &mut act);
        let mut seq = (*st.seq).clone();
        seq.apply(KeyAuto(false));
        let st = Status { seq: std::sync::Arc::new(seq), ..st };
        ses.frame(st.now, &st, &mut act);
        ses.touch(TouchEvent::Down { x: root.x, y: root.y, t: 10.5 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 10.6 }, Mode::Auto, &mut act);
        let sc = screen.buttons.iter().find(|(b, _)| *b == Button::KeyScale(Scale::Blues)).unwrap().1.center();
        ses.touch(TouchEvent::Down { x: sc.x, y: sc.y, t: 10.7 }, Mode::Auto, &mut act);
        ses.touch(TouchEvent::Up { t: 10.8 }, Mode::Auto, &mut act);
        assert_eq!(got, vec![Action::Seq(KeyAuto(false)), Action::Seq(KeyRoot(7)), Action::Seq(KeyScale(Scale::Blues))]);
    }

    #[test]
    fn renders_without_panicking_at_all_sizes() {
        assert!(!Button::Tap.visible(Mode::Auto, true) && Button::Tap.visible(Mode::Manual, true));
        assert!(!Button::StartStop.visible(Mode::Auto, false) && Button::ModeManual.visible(Mode::Auto, true));
        assert!(!Button::KeyRoot(3).visible(Mode::Manual, true) && Button::KeyRoot(3).visible(Mode::Auto, false));
        assert!(Button::KeyAuto.visible(Mode::Auto, true) && Button::KeyManual.visible(Mode::Auto, false));
        assert!(!Button::Generate(Instrument::Lead, Style::Random).enabled(Mode::Auto, true, false));
        assert!(Button::Generate(Instrument::Bass, Style::Counterpoint).enabled(Mode::Manual, false, true));
        for (w, h) in [(1280usize, 800usize), (800, 480), (480, 320), (320, 240)] {
            let s = Screen::new(w, h);
            let mut c = Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0);
            let mut st = status();
            s.render(&mut c, &View::build(&st, Some(Button::Tap)));
            // the tempo controls are not drawn in AUTO: their area stays background
            let tap = s.buttons.iter().find(|(b, _)| *b == Button::Tap).unwrap().1.center();
            let px = |c: &Canvas, p: Point| {
                let i = (p.y as usize * w + p.x as usize) * 4;
                let b = c.bytes();
                Rgb888::new(b[i + 2], b[i + 1], b[i])
            };
            assert_eq!(px(&c, tap), BG, "TAP drawn in AUTO at {w}x{h}");
            st.mode = Mode::Manual;
            s.render(&mut c, &View::build(&st, None));
            assert_ne!(px(&c, tap), BG, "TAP missing in MANUAL at {w}x{h}");
            st.taps = 3;
            s.render(&mut c, &View::build_with_touch(&st, Page::Lead, Some(Button::ParamPlus(Param::Note)), Some((10, 10))));
            assert!(c.bytes().iter().any(|&b| b > 100));
            s.render(&mut c, &View::build_with_touch(&st, Page::Bass, Some(Button::BassBar(1)), None));
            s.render(&mut c, &View::build_with_touch(&st, Page::Drums, Some(Button::DrumPad(3)), None));
        }
    }

    #[test]
    fn drums_view_shows_the_selected_pad_and_lights_playing_pads() {
        use sequencer::SeqAction::*;
        let s = status();
        let mut seq = (*s.seq).clone();
        seq.apply(DrumPad(4, None)); // closed hat
        for k in [0, 2, 8] {
            seq.apply(DrumStep(k));
        }
        seq.apply(DrumStep(8)); // accent
        seq.apply(DrumPad(0, None));
        seq.apply(DrumStep(0));
        seq.apply(DrumArm(sequencer::DrumArm::Mute));
        seq.apply(DrumPad(1, None));
        seq.apply(DrumArm(sequencer::DrumArm::Mute));
        let s = Status { seq: std::sync::Arc::new(seq), ..s };
        // the clock is on step 8 of the (1-bar) phrase
        let v = View::build_with_touch(&s, Page::Drums, None, None);
        let q = v.drums.as_ref().unwrap();
        assert_eq!(q.play, Some((0, 8)));
        assert_eq!(v.drum_bars(), Some((0, Some(0))));
        assert_eq!(q.drum, 0);
        assert_eq!(q.steps[0], 1);
        assert_eq!(q.dots[4][8], 2, "accented hat");
        assert_eq!(q.dots[4][2], 1);
        assert!(q.lit[4] && !q.lit[0], "the hat plays on step 8, the kick does not");
        assert!(q.muted[1]);
        assert_eq!(q.labels[0], "KICK");
        assert_eq!(q.params[2], "SWING 50");
        // a muted pad never lights
        let mut seq = (*s.seq).clone();
        seq.apply(DrumArm(sequencer::DrumArm::Mute));
        seq.apply(DrumPad(4, None));
        let s2 = Status { seq: std::sync::Arc::new(seq), ..s.clone() };
        assert!(!View::build_with_touch(&s2, Page::Drums, None, None).drums.unwrap().lit[4]);
        // the playhead moving on changes the view
        let mut s3 = s.clone();
        s3.now += 0.13;
        assert_ne!(v, View::build_with_touch(&s3, Page::Drums, None, None));
    }

    #[test]
    fn settings_view_lists_every_instrument_with_its_channel() {
        let mut st = status();
        let v = View::build_with_touch(&st, Page::Settings, None, None);
        let q = v.settings.as_ref().unwrap();
        assert_eq!(q.rows.len(), 3);
        assert_eq!((q.rows[0].name, q.rows[0].value.as_str()), ("LEAD", "CH 10"));
        assert_eq!((q.rows[1].name, q.rows[1].value.as_str()), ("BASS", "CH 1"));
        assert_eq!((q.rows[2].name, q.rows[2].value.as_str()), ("DRUMS", "CH 10"));
        // a channel change shows up, and only that changes the view
        let mut seq = (*st.seq).clone();
        seq.apply(sequencer::SeqAction::Channel(Instrument::Lead, 2));
        st.seq = std::sync::Arc::new(seq);
        let v2 = View::build_with_touch(&st, Page::Settings, None, None);
        assert_ne!(v, v2);
        assert_eq!(v2.settings.as_ref().unwrap().rows[0].value, "CH 12");
        st.level_db = -3.0;
        assert_eq!(View::build_with_touch(&st, Page::Settings, None, None), v2, "the level meter is not on this page");
        // it renders, tabs included, at every size
        for (w, h) in [(800usize, 480usize), (320, 240), (1280, 800)] {
            let s = Screen::new(w, h);
            let mut c = Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0);
            s.render(&mut c, &v2);
        }
    }

    #[test]
    fn a_pad_tap_carries_the_clock_position() {
        use crate::{Action, Session, TouchEvent, UiConfig};
        use sequencer::{RecPos, SeqAction::*};
        let (w, h) = (800usize, 480usize);
        let screen = Screen::new(w, h);
        let mut ses = Session::new(&UiConfig::default(), Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0));
        ses.set_page(Page::Drums);
        let at = |b: Button| screen.buttons.iter().find(|(x, _)| *x == b).unwrap().1.center();
        let mut got = Vec::new();
        // before the first frame the session knows no grid: the tap has no position
        let p = at(Button::DrumPad(2));
        ses.touch(TouchEvent::Down { x: p.x, y: p.y, t: 1.0 }, Mode::Auto, &mut |a| got.push(a));
        ses.touch(TouchEvent::Up { t: 1.1 }, Mode::Auto, &mut |a| got.push(a));
        assert_eq!(got, vec![Action::Seq(DrumPad(2, None))]);
        // a frame brings the grid: the tap lands on the nearest 16th (the
        // touch latency is subtracted, as for TAP)
        let st = status();
        ses.frame(st.now, &st, &mut |a| got.push(a));
        got.clear();
        let t = st.now + UiConfig::default().tap_offset_s;
        ses.touch(TouchEvent::Down { x: p.x, y: p.y, t }, Mode::Auto, &mut |a| got.push(a));
        ses.touch(TouchEvent::Up { t: t + 0.1 }, Mode::Auto, &mut |a| got.push(a));
        assert_eq!(got, vec![Action::Seq(DrumPad(2, Some(RecPos { bar: 0, step: 8, ahead: false })))]);
        // steps go through the usual dispatch
        got.clear();
        let p = at(Button::DrumStep(5));
        ses.touch(TouchEvent::Down { x: p.x, y: p.y, t: 5.0 }, Mode::Auto, &mut |a| got.push(a));
        ses.touch(TouchEvent::Up { t: 5.1 }, Mode::Auto, &mut |a| got.push(a));
        assert_eq!(got, vec![Action::Seq(DrumStep(5))]);
    }
}
