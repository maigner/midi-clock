//! A pattern sequencer that runs from the beat grid. Steps are 16ths: step
//! `k` of the bar sounds at beat `downbeat + k / 4`, tick `(k % 4) * 6`. Being
//! placed on the grid, the notes are on the beat in both auto and manual mode
//! and follow every correction, like the click and the clock.
//!
//! The 8-track drum pattern, the single-voice bass line (`Bass`) and the
//! drum kit (`Drums`: pads, one step row per instrument, accents and swing,
//! after the DrumBrute) are phrases of 1, 2, 4 or 8 bars, played by the same
//! thread. Bars are counted from the downbeat of the grid, so a phrase
//! always starts on a "1".
//!
//! `SeqState` is the editable model (UI thread), `Snapshot` what the
//! real-time thread plays, `run` the thread.

pub mod generate;
pub mod theory;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtio::{now, sleep_until, MidiOut};
use serde::{Deserialize, Serialize};
use tracker::Grid;

pub use generate::{GenNote, Rhythm, Rng, Voice};
pub use theory::{Key, KeySetup, Scale, PITCH_NAMES};

/// Tonic of a generated LEAD line: the lowest of the eight track notes is
/// the tonic at or above middle C (C3 here).
pub const LEAD_GEN_LOW: u8 = 60;
/// A generated bass line spans the octave up from the tonic at or above
/// this note (G0), so the tonic lands between G0 and F#1.
pub const BASS_GEN_LOW: u8 = 31;

pub const STEPS: usize = 16;
pub const TRACKS: usize = 8;
pub const PATTERNS: usize = 4;
/// Longest phrase, in bars (drums and bass).
pub const MAX_BARS: usize = 8;
/// Steps a track or the bass line holds.
pub const MAX_STEPS: usize = MAX_BARS * STEPS;
/// Pitch rows the bass page shows at once (one octave).
pub const BASS_ROWS: usize = 8;
/// Highest `Bass::base`: the top row is then MIDI note 127.
pub const BASS_TOP: u8 = 127 - (BASS_ROWS as u8 - 1);
/// Instruments (pads) of the drum kit.
pub const DRUMS: usize = 10;
/// Swing of the drum kit in percent: 50 is straight, 75 moves every second
/// 16th half a step late (the triplet feel is at 67).
pub const SWING_MIN: u8 = 50;
pub const SWING_MAX: u8 = 75;
/// Velocity of an accented drum step.
pub const ACCENT_VEL: u8 = 127;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub on: bool,
    pub note: u8,
    pub vel: u8,
    /// Length in steps (1..=MAX_STEPS); the note-off lands on the grid `len`
    /// steps after the note-on. May run past the bar and past the end of
    /// the phrase (it then wraps).
    #[serde(default = "one_step")]
    pub len: u16,
}

impl Default for Step {
    fn default() -> Self {
        Self { on: false, note: 60, vel: 100, len: 1 }
    }
}

fn one_step() -> u16 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    /// Note new steps get.
    pub note: u8,
    pub vel: u8,
    pub muted: bool,
    /// `MAX_STEPS` steps; only the pattern's `bars` bars play.
    pub steps: Vec<Step>,
    /// Files from before the pattern had one channel gave every track its
    /// own; the first track's becomes the pattern's when such a file loads.
    #[serde(default, rename = "channel", skip_serializing)]
    old_channel: Option<u8>,
}

impl Track {
    pub fn new(note: u8) -> Self {
        Self { note, vel: 100, muted: false, steps: vec![Step::default(); MAX_STEPS], old_channel: None }
    }
}

fn one_bar() -> u8 {
    1
}

fn drum_channel() -> u8 {
    9
}

/// The LEAD phrase: `TRACKS` polyphonic tracks, all on one MIDI channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    /// Phrase length in bars: 1, 2, 4 or 8.
    #[serde(default = "one_bar")]
    pub bars: u8,
    /// MIDI channel of every track, 0-based (shown 1-based).
    #[serde(default = "drum_channel")]
    pub channel: u8,
    pub tracks: Vec<Track>,
}

impl Default for Pattern {
    fn default() -> Self {
        // General MIDI drums on channel 10: kick, snare, closed hat, open hat, clap, low tom, high tom, crash.
        let notes = [36u8, 38, 42, 46, 39, 45, 50, 49];
        Self { bars: 1, channel: drum_channel(), tracks: notes.iter().map(|&n| Track::new(n)).collect() }
    }
}

impl Pattern {
    fn normalise(&mut self) {
        if !matches!(self.bars, 1 | 2 | 4 | 8) {
            self.bars = 1;
        }
        if let Some(c) = self.tracks.iter().find_map(|t| t.old_channel) {
            self.channel = c;
        }
        self.channel &= 0x0F;
        self.tracks.resize_with(TRACKS, || Track::new(60));
        for t in &mut self.tracks {
            t.old_channel = None;
            t.steps.resize(MAX_STEPS, Step::default());
            for st in &mut t.steps {
                st.len = st.len.clamp(1, MAX_STEPS as u16);
            }
            t.note &= 0x7F;
            t.vel = t.vel.clamp(1, 127);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BassStep {
    pub on: bool,
    pub note: u8,
    /// Length in steps, as `Step::len`.
    #[serde(default = "one_step")]
    pub len: u16,
    /// Files from before every bass note had its own velocity get the
    /// default; a new note gets `Bass::vel`.
    #[serde(default = "default_vel")]
    pub vel: u8,
}

impl Default for BassStep {
    fn default() -> Self {
        Self { on: false, note: 36, len: 1, vel: default_vel() }
    }
}

fn default_vel() -> u8 {
    100
}

/// The single-voice bass line: up to 8 bars of 16ths, one note per step,
/// each with a length and a velocity. A new note always ends the previous
/// one (monophonic).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bass {
    /// MIDI channel, 0-based.
    pub channel: u8,
    /// Velocity a new note gets.
    pub vel: u8,
    /// Phrase length in bars: 1, 2, 4 or 8.
    pub bars: u8,
    /// Lowest note of the pitch window shown on screen (`BASS_ROWS` semitones).
    pub base: u8,
    /// `MAX_STEPS` steps; only the first `bars` bars play.
    pub steps: Vec<BassStep>,
}

impl Default for Bass {
    fn default() -> Self {
        Self { channel: 0, vel: 100, bars: 2, base: 36, steps: vec![BassStep::default(); MAX_STEPS] }
    }
}

impl Bass {
    fn normalise(&mut self) {
        self.steps.resize(MAX_STEPS, BassStep::default());
        for st in &mut self.steps {
            st.note &= 0x7F;
            st.len = st.len.clamp(1, MAX_STEPS as u16);
            st.vel = st.vel.clamp(1, 127);
        }
        self.channel &= 0x0F;
        self.vel = self.vel.clamp(1, 127);
        if !matches!(self.bars, 1 | 2 | 4 | 8) {
            self.bars = 1;
        }
        self.base = self.base.min(BASS_TOP);
    }
}

/// One 16th of a drum instrument: a hit or not, plain or accented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrumStep {
    pub on: bool,
    #[serde(default)]
    pub accent: bool,
}

/// One instrument (pad) of the drum kit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drum {
    pub note: u8,
    /// Velocity of a plain hit; an accented one plays `ACCENT_VEL`.
    pub vel: u8,
    pub muted: bool,
    pub solo: bool,
    /// `MAX_STEPS` steps; only the kit's `bars` bars play.
    pub steps: Vec<DrumStep>,
}

impl Drum {
    pub fn new(note: u8) -> Self {
        Self { note, vel: 100, muted: false, solo: false, steps: vec![DrumStep::default(); MAX_STEPS] }
    }
}

/// The drum kit of the DRUMS page: `DRUMS` instruments on one MIDI channel,
/// each with its own row of one-16th hits, a phrase of 1..8 bars and swing
/// on every second 16th. Worked like a DrumBrute: pick an instrument on its
/// pad, set its steps, accent some, mute and solo, record pads live.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drums {
    /// MIDI channel, 0-based.
    pub channel: u8,
    /// Phrase length in bars: 1, 2, 4 or 8.
    pub bars: u8,
    /// `SWING_MIN..=SWING_MAX` percent.
    pub swing: u8,
    pub drums: Vec<Drum>,
}

impl Default for Drums {
    fn default() -> Self {
        // General MIDI: kick, snare, rim, clap, closed hat, open hat, low tom, high tom, crash, ride.
        let notes = [36u8, 38, 37, 39, 42, 46, 45, 50, 49, 51];
        Self { channel: 9, bars: 1, swing: SWING_MIN, drums: notes.iter().map(|&n| Drum::new(n)).collect() }
    }
}

impl Drums {
    fn normalise(&mut self) {
        if !matches!(self.bars, 1 | 2 | 4 | 8) {
            self.bars = 1;
        }
        let defaults = Drums::default();
        for i in self.drums.len()..DRUMS {
            self.drums.push(defaults.drums[i].clone());
        }
        self.drums.truncate(DRUMS);
        for d in &mut self.drums {
            d.steps.resize(MAX_STEPS, DrumStep::default());
            d.note &= 0x7F;
            d.vel = d.vel.clamp(1, 127);
        }
        self.channel &= 0x0F;
        self.swing = self.swing.clamp(SWING_MIN, SWING_MAX);
    }

    /// Whether instrument `i` sounds: not muted and, while any instrument is
    /// soloed, one of the soloed.
    pub fn audible(&self, i: usize) -> bool {
        let d = &self.drums[i];
        !d.muted && (d.solo || !self.drums.iter().any(|d| d.solo))
    }

    /// How far every second 16th plays late, in steps: 0 at 50 % swing,
    /// 0.5 at 75 %.
    pub fn swing_shift(&self) -> f64 {
        (self.swing.clamp(SWING_MIN, SWING_MAX) - SWING_MIN) as f64 / 50.0
    }
}

/// Pad hit to play right away: the screen's pad was tapped. A new `serial`
/// for every tap, so the playing thread fires each once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadHit {
    pub serial: u64,
    pub channel: u8,
    pub note: u8,
    pub vel: u8,
}

/// What the playing thread needs; replaced wholesale on every edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub pattern: Pattern,
    pub playing: bool,
    pub bass: Bass,
    pub drums: Drums,
    pub hit: Option<PadHit>,
    /// Extra lead for every note in ms, on top of the configured one: the
    /// synth's own latency, dialled in by ear on the SETUP page.
    pub note_lead_ms: i32,
}

/// Which track parameter a +/- press changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    Note,
    Velocity,
}

/// Which bass parameter a +/- press changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BassParam {
    Velocity,
}

/// Which drum kit parameter a +/- press changes: the kit's swing, the
/// selected instrument's note and velocity. The channel is set on SETUP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrumParam {
    Note,
    Velocity,
    Swing,
}

/// What the next pad taps on the DRUMS page do, as the MUTE / SOLO / ERASE
/// buttons of a DrumBrute: armed, a pad toggles its instrument's mute or
/// solo, or erases its steps, instead of selecting and playing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrumArm {
    #[default]
    Off,
    Mute,
    Solo,
    Clear,
}

/// Where the clock is when a pad is tapped: the nearest 16th of the drum
/// phrase, and whether that 16th is still to come (the sequencer then plays
/// the recorded step itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecPos {
    pub bar: usize,
    pub step: usize,
    pub ahead: bool,
}

/// One of the things the sequencer plays, each on its own MIDI channel: the
/// LEAD pattern (all its tracks together), the bass line, or the drum kit.
/// The SETUP page lists them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instrument {
    Lead,
    Bass,
    Drums,
}

impl Instrument {
    /// Every instrument, in the order the SETUP page shows them.
    pub fn all() -> impl Iterator<Item = Instrument> {
        [Instrument::Lead, Instrument::Bass, Instrument::Drums].into_iter()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeqAction {
    /// A tap on step `step` of the shown bar on track `track`: `CellDown`
    /// followed by `CellUp`.
    ToggleStep(usize, usize),
    /// Finger down on a cell (track, step of the shown bar). An empty cell
    /// gets a one-step note; a cell inside a longer note (its tail) gets a
    /// new note that cuts the old one; the head of a note starts a drag
    /// and clears the note if the finger lifts without moving.
    CellDown(usize, usize),
    /// The finger moved to this step of the drag's bar (the bar the drag
    /// started in, moved by `CellDragBar`): the note being dragged now ends
    /// there. Dragging left of the head makes it one step; a following note
    /// caps the length.
    CellDrag(usize),
    /// The finger is held at the right (+1) or left (-1) edge of the grid:
    /// the drag moves on to the next (previous) bar, which is shown and
    /// locked, and the note ends at the finger's column there. Past the end
    /// of the phrase the drag wraps to its first bar.
    CellDragBar(i32),
    /// The finger moved up or down while dragging: the dragged note's
    /// velocity changes by this much (the drag then no longer counts as a
    /// tap, so lifting the finger leaves the note).
    CellVel(i32),
    /// Finger up: a tap on a note's head clears it.
    CellUp,
    SelectTrack(usize),
    SelectPattern(usize),
    /// Phrase length of the current pattern in bars (1, 2, 4 or 8).
    Bars(u8),
    /// Which bar of the pattern the screen shows and edits; the screen then
    /// stays on it (the bar is locked).
    Bar(usize),
    /// The clock moved on to this bar: show it, unless the shown bar is locked.
    FollowBar(usize),
    /// Lock the screen to the shown bar, or let it follow the clock again.
    ToggleBarLock,
    /// Change a parameter of the selected track, or of the selected step in edit mode.
    Adjust(Param, i32),
    TogglePlay,
    ToggleMute,
    ToggleEdit,
    /// Clear the selected track (or the selected step in edit mode).
    Clear,
    /// Bass: a tap on step `step` of the shown bar in pitch row `row`
    /// (0 = the shown base note): `BassCellDown` followed by `BassCellUp`.
    /// The same cell again clears the note; another row moves it.
    BassSet(usize, usize),
    /// Bass: as `CellDown`, in pitch row `row`.
    BassCellDown(usize, usize),
    /// Bass: as `CellDrag`.
    BassCellDrag(usize),
    /// Bass: as `CellDragBar`.
    BassCellDragBar(i32),
    /// Bass: as `CellVel`.
    BassCellVel(i32),
    /// Bass: as `CellUp`.
    BassCellUp,
    /// Bass: phrase length in bars (1, 2, 4 or 8).
    BassBars(u8),
    /// Bass: which bar the screen shows and edits (and stays on).
    BassBar(usize),
    /// Bass: the clock moved on to this bar; show it unless locked.
    BassFollowBar(usize),
    ToggleBassBarLock,
    BassAdjust(BassParam, i32),
    /// Bass: scroll the pitch window on screen by this many semitones
    /// (the notes stay where they are).
    BassScroll(i32),
    /// Drums: a tap on pad `i`. Selects the instrument and plays it at
    /// once; with REC on and the clock running (`pos` says where it is) the
    /// hit is also written to the nearest 16th. While MUTE or SOLO is armed
    /// the tap toggles that for the instrument instead; while CLEAR is
    /// armed it erases the instrument's steps and disarms.
    DrumPad(usize, Option<RecPos>),
    /// Drums: a tap on step `k` of the shown bar for the selected
    /// instrument: off -> hit -> accented hit -> off.
    DrumStep(usize),
    /// Drums: phrase length in bars (1, 2, 4 or 8).
    DrumBars(u8),
    /// Drums: which bar the screen shows and edits (and stays on).
    DrumBar(usize),
    /// Drums: the clock moved on to this bar; show it unless locked.
    DrumFollowBar(usize),
    ToggleDrumBarLock,
    DrumAdjust(DrumParam, i32),
    /// Drums: arm MUTE, SOLO or CLEAR for the pads; the same again disarms.
    DrumArm(DrumArm),
    /// Drums: live recording of pad taps on and off.
    ToggleDrumRec,
    /// SETUP: move an instrument's MIDI channel by this much (clamped to
    /// 1..16). The lead's channel is the current pattern's.
    Channel(Instrument, i32),
    /// Change the extra note lead by this many milliseconds (SETUP page).
    NoteLead(i32),
    /// KEY: hear the key from the microphone (true) or set it by hand.
    KeyAuto(bool),
    /// KEY: the tonic set by hand (0 = C ... 11 = B); switches to manual.
    KeyRoot(u8),
    /// KEY: the scale set by hand; switches to manual.
    KeyScale(Scale),
    /// LEAD or BASS: write a new phrase in the key by the counterpoint
    /// rules, from this seed, in the instrument's next rhythm (which then
    /// moves on). Replaces the phrase; on the lead the eight tracks become
    /// the notes of the key up from the tonic, on the bass the pitch
    /// window scrolls to the tonic.
    Generate(Instrument, Key, u64),
    /// LEAD or BASS: take every note out of the phrase. The tracks' notes
    /// and the phrase length stay.
    ClearPhrase(Instrument),
}

impl SeqAction {
    /// True for actions that only change what the screen shows: nothing to
    /// save, nothing the playing thread needs to know.
    pub fn view_only(self) -> bool {
        matches!(
            self,
            SeqAction::Bar(_)
                | SeqAction::FollowBar(_)
                | SeqAction::ToggleBarLock
                | SeqAction::BassBar(_)
                | SeqAction::BassFollowBar(_)
                | SeqAction::ToggleBassBarLock
                | SeqAction::DrumBar(_)
                | SeqAction::DrumFollowBar(_)
                | SeqAction::ToggleDrumBarLock
                | SeqAction::DrumArm(_)
                | SeqAction::ToggleDrumRec
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct File {
    patterns: Vec<Pattern>,
    current: usize,
    #[serde(default)]
    bass: Bass,
    #[serde(default)]
    drums: Drums,
    #[serde(default)]
    key: KeySetup,
    #[serde(default)]
    note_lead_ms: i32,
}

/// The editable model. Every method that changes something returns true, so
/// the caller knows to publish a new snapshot and save.
#[derive(Clone, Debug)]
pub struct SeqState {
    pub patterns: Vec<Pattern>,
    pub current: usize,
    pub track: usize,
    /// Bar of the pattern shown on screen.
    pub bar: usize,
    /// Keep showing `bar` while the clock plays other bars (otherwise the
    /// screen follows the playing bar).
    pub bar_lock: bool,
    pub edit: bool,
    /// Absolute step index (bar * STEPS + step) in edit mode.
    pub selected_step: Option<usize>,
    pub playing: bool,
    pub bass: Bass,
    /// Bar of the bass phrase shown on screen.
    pub bass_bar: usize,
    /// As `bar_lock`, for the bass page.
    pub bass_bar_lock: bool,
    pub drums: Drums,
    /// Selected instrument of the drum kit (its steps are on screen).
    pub drum: usize,
    /// Bar of the drum phrase shown on screen.
    pub drum_bar: usize,
    /// As `bar_lock`, for the drums page.
    pub drum_bar_lock: bool,
    /// What pad taps do at the moment.
    pub drum_arm: DrumArm,
    /// Pad taps are written to the phrase while the clock runs.
    pub drum_rec: bool,
    /// The rhythm the next generated lead phrase gets; every one written
    /// moves it on, so repeated presses go through them all.
    pub lead_rhythm: Rhythm,
    /// The same for the bass line.
    pub bass_rhythm: Rhythm,
    /// The KEY page: auto or by hand, and the key set by hand.
    pub key: KeySetup,
    /// Extra lead for every note in ms (SETUP page), `NOTE_LEAD_RANGE`.
    pub note_lead_ms: i32,
    /// The last pad tap, for the playing thread.
    hit: Option<PadHit>,
    /// A finger dragging a note's length on one of the grids.
    drag: Option<Drag>,
}

/// A note whose length is being dragged. `head` is its absolute step.
#[derive(Clone, Copy, Debug)]
struct Drag {
    bass: bool,
    track: usize,
    head: usize,
    /// Absolute step the finger went down on (the head, or a step inside
    /// the note) and, for the bass, the note of its row.
    cell: usize,
    note: u8,
    /// Bar the drag columns are in, counted on from the head's bar without
    /// wrapping (bar `bars` is the phrase's first bar again), so a note can
    /// be dragged around the end of the phrase.
    bar: i64,
    moved: bool,
    /// The finger went down on the note's head: lifting it without moving
    /// clears the note.
    tap_clears: bool,
}

impl Default for SeqState {
    fn default() -> Self {
        Self {
            patterns: (0..PATTERNS).map(|_| Pattern::default()).collect(),
            current: 0,
            track: 0,
            bar: 0,
            bar_lock: false,
            edit: false,
            selected_step: None,
            playing: true,
            bass: Bass::default(),
            bass_bar: 0,
            bass_bar_lock: false,
            drums: Drums::default(),
            drum: 0,
            drum_bar: 0,
            drum_bar_lock: false,
            drum_arm: DrumArm::Off,
            drum_rec: false,
            lead_rhythm: Rhythm::default(),
            bass_rhythm: Rhythm::default(),
            key: KeySetup::default(),
            note_lead_ms: 0,
            hit: None,
            drag: None,
        }
    }
}

impl SeqState {
    pub fn pattern(&self) -> &Pattern {
        &self.patterns[self.current]
    }

    /// The bar shown for editing, clamped to the pattern's length.
    pub fn shown_bar(&self) -> usize {
        self.bar.min(self.pattern().bars.max(1) as usize - 1)
    }

    /// The bar of the drum phrase shown for editing, clamped to its length.
    pub fn drum_shown_bar(&self) -> usize {
        self.drum_bar.min(self.drums.bars.max(1) as usize - 1)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            pattern: self.patterns[self.current].clone(),
            playing: self.playing,
            bass: self.bass.clone(),
            drums: self.drums.clone(),
            hit: self.hit,
            note_lead_ms: self.note_lead_ms,
        }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let f: File = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s = SeqState { patterns: f.patterns, current: f.current, bass: f.bass, drums: f.drums, key: f.key, note_lead_ms: f.note_lead_ms.clamp(NOTE_LEAD_RANGE.0, NOTE_LEAD_RANGE.1), ..SeqState::default() };
        s.key.key.root %= 12;
        s.bass.normalise();
        s.drums.normalise();
        s.patterns.resize_with(PATTERNS, Pattern::default);
        s.patterns.truncate(PATTERNS);
        for p in &mut s.patterns {
            p.normalise();
        }
        s.current = s.current.min(PATTERNS - 1);
        Ok(s)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let f = File { patterns: self.patterns.clone(), current: self.current, bass: self.bass.clone(), drums: self.drums.clone(), key: self.key, note_lead_ms: self.note_lead_ms };
        let text = toml::to_string(&f).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn apply(&mut self, a: SeqAction) -> bool {
        let cur = self.current;
        match a {
            SeqAction::ToggleStep(t, k) => {
                let a = self.apply(SeqAction::CellDown(t, k));
                self.apply(SeqAction::CellUp) || a
            }
            SeqAction::CellDown(t, k) => {
                if t >= TRACKS || k >= STEPS {
                    return false;
                }
                self.track = t;
                self.drag = None;
                let bar = self.shown_bar();
                let idx = bar * STEPS + k;
                if self.edit {
                    self.selected_step = Some(idx);
                    return true;
                }
                let n = phrase_steps(self.patterns[cur].bars);
                let tr = &mut self.patterns[cur].tracks[t];
                let cover = cover_map(|i| tr.steps[i].on.then_some(tr.steps[i].len as usize), n);
                let drag = Drag { bass: false, track: t, head: idx, cell: idx, note: tr.note, bar: bar as i64, moved: false, tap_clears: false };
                match cover[idx] {
                    Some(h) if h as usize == idx => {
                        self.drag = Some(Drag { tap_clears: true, ..drag });
                        false
                    }
                    Some(h) => {
                        // inside a note: a new one that cuts it ... (a tap), or
                        // dragging its tail: decided by whether the finger moves
                        self.drag = Some(Drag { head: h as usize, bar: drag_bar(h as usize, bar, n), ..drag });
                        false
                    }
                    None => {
                        tr.steps[idx] = Step { on: true, note: tr.note, vel: tr.vel, len: 1 };
                        self.drag = Some(drag);
                        true
                    }
                }
            }
            SeqAction::CellDrag(k) => {
                let Some(d) = self.drag.filter(|d| !d.bass) else {
                    return false;
                };
                if k >= STEPS {
                    return false;
                }
                let n = phrase_steps(self.patterns[cur].bars);
                let tr = &mut self.patterns[cur].tracks[d.track];
                let len = drag_len(d, k, n, |i| tr.steps[i].on);
                self.drag = Some(Drag { moved: true, ..d });
                if tr.steps[d.head].len == len {
                    return false;
                }
                tr.steps[d.head].len = len;
                true
            }
            SeqAction::CellDragBar(dir) => {
                let Some(d) = self.drag.filter(|d| !d.bass) else {
                    return false;
                };
                let n = phrase_steps(self.patterns[cur].bars);
                let Some(bar) = drag_bar_step(d, dir, n) else {
                    return false;
                };
                self.bar = bar.rem_euclid((n / STEPS) as i64) as usize;
                self.bar_lock = true;
                self.drag = Some(Drag { bar, ..d });
                self.apply(SeqAction::CellDrag(if dir > 0 { STEPS - 1 } else { 0 }));
                true
            }
            SeqAction::CellVel(dv) => {
                let Some(d) = self.drag.filter(|d| !d.bass) else {
                    return false;
                };
                self.drag = Some(Drag { moved: true, ..d });
                let st = &mut self.patterns[cur].tracks[d.track].steps[d.head];
                let vel = (st.vel as i32 + dv).clamp(1, 127) as u8;
                if vel == st.vel {
                    return false;
                }
                st.vel = vel;
                true
            }
            SeqAction::CellUp => {
                let Some(d) = self.drag.take().filter(|d| !d.bass) else {
                    return false;
                };
                if d.moved {
                    return false;
                }
                let n = phrase_steps(self.patterns[cur].bars);
                let tr = &mut self.patterns[cur].tracks[d.track];
                if d.tap_clears {
                    tr.steps[d.head].on = false;
                } else if d.cell != d.head {
                    // a tap inside a note: a new note there cuts it
                    tr.steps[d.head].len = ((d.cell + n - d.head) % n).max(1) as u16;
                    tr.steps[d.cell] = Step { on: true, note: tr.note, vel: tr.vel, len: 1 };
                } else {
                    return false;
                }
                true
            }
            SeqAction::SelectTrack(t) => {
                if t >= TRACKS {
                    return false;
                }
                self.track = t;
                self.selected_step = None;
                true
            }
            SeqAction::SelectPattern(p) => {
                if p >= PATTERNS {
                    return false;
                }
                self.current = p;
                self.selected_step = None;
                true
            }
            SeqAction::Bars(n) => {
                if !matches!(n, 1 | 2 | 4 | 8) {
                    return false;
                }
                self.drag = None;
                let old = self.patterns[cur].bars.max(1) as usize;
                for t in &mut self.patterns[cur].tracks {
                    extend_bars(&mut t.steps, old, n as usize);
                }
                self.patterns[cur].bars = n;
                self.bar = self.bar.min(n as usize - 1);
                self.selected_step = None;
                true
            }
            SeqAction::Bar(b) => {
                if b >= self.patterns[cur].bars as usize {
                    return false;
                }
                if self.bar != b {
                    self.selected_step = None;
                }
                self.bar = b;
                self.bar_lock = true;
                true
            }
            SeqAction::FollowBar(b) => {
                if self.bar_lock || b >= self.patterns[cur].bars as usize || self.bar == b {
                    return false;
                }
                self.bar = b;
                true
            }
            SeqAction::ToggleBarLock => {
                self.bar_lock = !self.bar_lock;
                true
            }
            SeqAction::Adjust(param, d) => {
                let tr = &mut self.patterns[cur].tracks[self.track];
                let bump = |v: u8, lo: u8, hi: u8| (v as i32 + d).clamp(lo as i32, hi as i32) as u8;
                match (param, self.edit.then_some(self.selected_step).flatten()) {
                    (Param::Note, Some(k)) => tr.steps[k].note = bump(tr.steps[k].note, 0, 127),
                    (Param::Velocity, Some(k)) => tr.steps[k].vel = bump(tr.steps[k].vel, 1, 127),
                    (Param::Note, None) => tr.note = bump(tr.note, 0, 127),
                    (Param::Velocity, None) => tr.vel = bump(tr.vel, 1, 127),
                }
                true
            }
            SeqAction::TogglePlay => {
                self.playing = !self.playing;
                true
            }
            SeqAction::ToggleMute => {
                let tr = &mut self.patterns[cur].tracks[self.track];
                tr.muted = !tr.muted;
                true
            }
            SeqAction::ToggleEdit => {
                self.edit = !self.edit;
                if !self.edit {
                    self.selected_step = None;
                }
                true
            }
            SeqAction::Clear => {
                let tr = &mut self.patterns[cur].tracks[self.track];
                match self.edit.then_some(self.selected_step).flatten() {
                    Some(k) => tr.steps[k].on = false,
                    None => {
                        for s in &mut tr.steps {
                            s.on = false;
                        }
                    }
                }
                true
            }
            SeqAction::BassSet(row, k) => {
                let a = self.apply(SeqAction::BassCellDown(row, k));
                self.apply(SeqAction::BassCellUp) || a
            }
            SeqAction::BassCellDown(row, k) => {
                if row >= BASS_ROWS || k >= STEPS {
                    return false;
                }
                self.drag = None;
                let bar = self.bass_bar.min(self.bass.bars as usize - 1);
                let idx = bar * STEPS + k;
                let note = (self.bass.base + row as u8).min(127);
                let n = phrase_steps(self.bass.bars);
                let b = &mut self.bass;
                let cover = cover_map(|i| b.steps[i].on.then_some(b.steps[i].len as usize), n);
                let drag = Drag { bass: true, track: 0, head: idx, cell: idx, note, bar: bar as i64, moved: false, tap_clears: false };
                match cover[idx] {
                    Some(h) if h as usize == idx && b.steps[idx].note == note => {
                        self.drag = Some(Drag { tap_clears: true, ..drag });
                        false
                    }
                    Some(h) if h as usize == idx => {
                        // another row on the head: move the note, keep its length
                        b.steps[idx].note = note;
                        self.drag = Some(drag);
                        true
                    }
                    Some(h) if b.steps[h as usize].note == note => {
                        // inside the note's own row: drag its tail, or cut it with a tap
                        self.drag = Some(Drag { head: h as usize, bar: drag_bar(h as usize, bar, n), ..drag });
                        false
                    }
                    covering => {
                        if let Some(h) = covering {
                            b.steps[h as usize].len = ((idx + n - h as usize) % n).max(1) as u16;
                        }
                        b.steps[idx] = BassStep { on: true, note, len: 1, vel: b.vel };
                        self.drag = Some(drag);
                        true
                    }
                }
            }
            SeqAction::BassCellDrag(k) => {
                let Some(d) = self.drag.filter(|d| d.bass) else {
                    return false;
                };
                if k >= STEPS {
                    return false;
                }
                let n = phrase_steps(self.bass.bars);
                let b = &mut self.bass;
                let len = drag_len(d, k, n, |i| b.steps[i].on);
                self.drag = Some(Drag { moved: true, ..d });
                if b.steps[d.head].len == len {
                    return false;
                }
                b.steps[d.head].len = len;
                true
            }
            SeqAction::BassCellDragBar(dir) => {
                let Some(d) = self.drag.filter(|d| d.bass) else {
                    return false;
                };
                let n = phrase_steps(self.bass.bars);
                let Some(bar) = drag_bar_step(d, dir, n) else {
                    return false;
                };
                self.bass_bar = bar.rem_euclid((n / STEPS) as i64) as usize;
                self.bass_bar_lock = true;
                self.drag = Some(Drag { bar, ..d });
                self.apply(SeqAction::BassCellDrag(if dir > 0 { STEPS - 1 } else { 0 }));
                true
            }
            SeqAction::BassCellVel(dv) => {
                let Some(d) = self.drag.filter(|d| d.bass) else {
                    return false;
                };
                self.drag = Some(Drag { moved: true, ..d });
                let st = &mut self.bass.steps[d.head];
                let vel = (st.vel as i32 + dv).clamp(1, 127) as u8;
                if vel == st.vel {
                    return false;
                }
                st.vel = vel;
                true
            }
            SeqAction::BassCellUp => {
                let Some(d) = self.drag.take().filter(|d| d.bass) else {
                    return false;
                };
                if d.moved {
                    return false;
                }
                let n = phrase_steps(self.bass.bars);
                let b = &mut self.bass;
                if d.tap_clears {
                    b.steps[d.head].on = false;
                } else if d.cell != d.head {
                    b.steps[d.head].len = ((d.cell + n - d.head) % n).max(1) as u16;
                    b.steps[d.cell] = BassStep { on: true, note: d.note, len: 1, vel: b.vel };
                } else {
                    return false;
                }
                true
            }
            SeqAction::BassBars(n) => {
                if !matches!(n, 1 | 2 | 4 | 8) {
                    return false;
                }
                self.drag = None;
                extend_bars(&mut self.bass.steps, self.bass.bars.max(1) as usize, n as usize);
                self.bass.bars = n;
                self.bass_bar = self.bass_bar.min(n as usize - 1);
                true
            }
            SeqAction::BassBar(b) => {
                if b >= self.bass.bars as usize {
                    return false;
                }
                self.bass_bar = b;
                self.bass_bar_lock = true;
                true
            }
            SeqAction::BassFollowBar(b) => {
                if self.bass_bar_lock || b >= self.bass.bars as usize || self.bass_bar == b {
                    return false;
                }
                self.bass_bar = b;
                true
            }
            SeqAction::ToggleBassBarLock => {
                self.bass_bar_lock = !self.bass_bar_lock;
                true
            }
            SeqAction::BassAdjust(param, d) => {
                let b = &mut self.bass;
                match param {
                    BassParam::Velocity => b.vel = (b.vel as i32 + d).clamp(1, 127) as u8,
                }
                true
            }
            SeqAction::BassScroll(d) => {
                let base = (self.bass.base as i32 + d).clamp(0, BASS_TOP as i32) as u8;
                if base == self.bass.base {
                    return false;
                }
                self.bass.base = base;
                true
            }
            SeqAction::DrumPad(i, pos) => {
                if i >= DRUMS {
                    return false;
                }
                let d = &mut self.drums.drums[i];
                match self.drum_arm {
                    DrumArm::Mute => d.muted = !d.muted,
                    DrumArm::Solo => d.solo = !d.solo,
                    DrumArm::Clear => {
                        for st in &mut d.steps {
                            *st = DrumStep::default();
                        }
                        self.drum = i;
                        self.drum_arm = DrumArm::Off;
                    }
                    DrumArm::Off => {
                        self.drum = i;
                        // Recording: the hit goes on the nearest 16th. If that
                        // 16th is still ahead the sequencer plays it from
                        // there, so the pad stays quiet rather than flam.
                        let mut play = true;
                        if let Some(p) = pos.filter(|_| self.drum_rec) {
                            if p.bar < self.drums.bars.max(1) as usize && p.step < STEPS {
                                let st = &mut d.steps[p.bar * STEPS + p.step];
                                if !st.on {
                                    *st = DrumStep { on: true, accent: false };
                                }
                                play = !p.ahead;
                            }
                        }
                        if play {
                            let serial = self.hit.map_or(0, |h| h.serial) + 1;
                            self.hit = Some(PadHit { serial, channel: self.drums.channel, note: d.note, vel: d.vel });
                        }
                    }
                }
                true
            }
            SeqAction::DrumStep(k) => {
                if k >= STEPS {
                    return false;
                }
                let idx = self.drum_shown_bar() * STEPS + k;
                let st = &mut self.drums.drums[self.drum.min(DRUMS - 1)].steps[idx];
                *st = match (st.on, st.accent) {
                    (false, _) => DrumStep { on: true, accent: false },
                    (true, false) => DrumStep { on: true, accent: true },
                    (true, true) => DrumStep::default(),
                };
                true
            }
            SeqAction::DrumBars(n) => {
                if !matches!(n, 1 | 2 | 4 | 8) {
                    return false;
                }
                let old = self.drums.bars.max(1) as usize;
                for d in &mut self.drums.drums {
                    extend_bars(&mut d.steps, old, n as usize);
                }
                self.drums.bars = n;
                self.drum_bar = self.drum_bar.min(n as usize - 1);
                true
            }
            SeqAction::DrumBar(b) => {
                if b >= self.drums.bars as usize {
                    return false;
                }
                self.drum_bar = b;
                self.drum_bar_lock = true;
                true
            }
            SeqAction::DrumFollowBar(b) => {
                if self.drum_bar_lock || b >= self.drums.bars as usize || self.drum_bar == b {
                    return false;
                }
                self.drum_bar = b;
                true
            }
            SeqAction::ToggleDrumBarLock => {
                self.drum_bar_lock = !self.drum_bar_lock;
                true
            }
            SeqAction::DrumAdjust(param, d) => {
                let bump = |v: u8, lo: u8, hi: u8| (v as i32 + d).clamp(lo as i32, hi as i32) as u8;
                let k = &mut self.drums;
                let dr = &mut k.drums[self.drum.min(DRUMS - 1)];
                match param {
                    DrumParam::Note => dr.note = bump(dr.note, 0, 127),
                    DrumParam::Velocity => dr.vel = bump(dr.vel, 1, 127),
                    DrumParam::Swing => k.swing = bump(k.swing, SWING_MIN, SWING_MAX),
                }
                true
            }
            SeqAction::DrumArm(a) => {
                self.drum_arm = if self.drum_arm == a { DrumArm::Off } else { a };
                true
            }
            SeqAction::ToggleDrumRec => {
                self.drum_rec = !self.drum_rec;
                true
            }
            SeqAction::Channel(inst, d) => {
                let ch = match inst {
                    Instrument::Lead => &mut self.patterns[cur].channel,
                    Instrument::Bass => &mut self.bass.channel,
                    Instrument::Drums => &mut self.drums.channel,
                };
                let new = (*ch as i32 + d).clamp(0, 15) as u8;
                if new == *ch {
                    return false;
                }
                *ch = new;
                true
            }
            SeqAction::NoteLead(d) => {
                let new = (self.note_lead_ms + d).clamp(NOTE_LEAD_RANGE.0, NOTE_LEAD_RANGE.1);
                if new == self.note_lead_ms {
                    return false;
                }
                self.note_lead_ms = new;
                true
            }
            SeqAction::KeyAuto(auto) => {
                if self.key.auto == auto {
                    return false;
                }
                self.key.auto = auto;
                true
            }
            SeqAction::KeyRoot(root) => {
                let root = root % 12;
                if !self.key.auto && self.key.key.root == root {
                    return false;
                }
                self.key.auto = false;
                self.key.key.root = root;
                true
            }
            SeqAction::KeyScale(scale) => {
                if !self.key.auto && self.key.key.scale == scale {
                    return false;
                }
                self.key.auto = false;
                self.key.key.scale = scale;
                true
            }
            SeqAction::Generate(inst, key, seed) => self.generate(inst, key, seed),
            SeqAction::ClearPhrase(inst) => {
                self.drag = None;
                match inst {
                    Instrument::Lead => {
                        for st in self.patterns[cur].tracks.iter_mut().flat_map(|tr| tr.steps.iter_mut()) {
                            st.on = false;
                        }
                        self.selected_step = None;
                        true
                    }
                    Instrument::Bass => {
                        for st in &mut self.bass.steps {
                            st.on = false;
                        }
                        true
                    }
                    Instrument::Drums => false,
                }
            }
        }
    }

    /// The note sounding on each step of the LEAD phrase: the lowest of the
    /// tracks' notes there.
    fn lead_line(&self) -> Vec<Option<u8>> {
        let p = self.pattern();
        let n = phrase_steps(p.bars);
        let mut line = vec![None; n];
        for tr in &p.tracks {
            if tr.muted {
                continue;
            }
            let map = cover_map(|i| tr.steps[i].on.then_some(tr.steps[i].len as usize), n);
            for (i, l) in line.iter_mut().enumerate() {
                if let Some(h) = map[i] {
                    let note = tr.steps[h as usize].note;
                    *l = Some(l.map_or(note, |x: u8| x.min(note)));
                }
            }
        }
        line
    }

    /// The note sounding on each step of the bass phrase.
    fn bass_line(&self) -> Vec<Option<u8>> {
        let b = &self.bass;
        let n = phrase_steps(b.bars);
        let map = cover_map(|i| b.steps[i].on.then_some(b.steps[i].len as usize), n);
        (0..n).map(|i| map[i].map(|h| b.steps[h as usize].note)).collect()
    }

    /// Whether the bass cell (pitch row of the shown window, step of the
    /// shown bar) is inside a note of that row: a drag on it changes the
    /// note's velocity rather than scrolling the window.
    pub fn bass_has_note(&self, row: usize, k: usize) -> bool {
        if row >= BASS_ROWS || k >= STEPS {
            return false;
        }
        let b = &self.bass;
        let n = phrase_steps(b.bars);
        let idx = self.bass_bar.min(b.bars.max(1) as usize - 1) * STEPS + k;
        let map = cover_map(|i| b.steps[i].on.then_some(b.steps[i].len as usize), n);
        map.get(idx).copied().flatten().is_some_and(|h| b.steps[h as usize].note == b.base.saturating_add(row as u8))
    }

    /// `SeqAction::Generate`.
    fn generate(&mut self, inst: Instrument, key: Key, seed: u64) -> bool {
        let mut rng = Rng::new(seed);
        self.drag = None;
        // velocity of a generated note: by the beat
        let velocity = |base: u8, accent: i8| (base as i32 + 12 * accent as i32).clamp(1, 127) as u8;
        match inst {
            Instrument::Lead => {
                let bass = self.bass_line();
                let cur = self.current;
                let p = &mut self.patterns[cur];
                let n = phrase_steps(p.bars);
                let pitches = key.notes_from_tonic(LEAD_GEN_LOW, TRACKS);
                if pitches.len() < TRACKS {
                    return false;
                }
                let other: Vec<Option<u8>> = if in_key(&bass, key) { (0..n).map(|i| bass[i % bass.len()]).collect() } else { Vec::new() };
                let v = Voice { pitches: &pitches, other: &other, above: true };
                let notes = generate::generate(self.lead_rhythm, key, &v, n, &mut rng);
                for (t, tr) in p.tracks.iter_mut().enumerate().take(TRACKS) {
                    tr.note = pitches[t];
                    for st in tr.steps.iter_mut().take(n) {
                        st.on = false;
                    }
                }
                for g in notes {
                    let Some(t) = pitches.iter().position(|&x| x == g.note) else { continue };
                    let tr = &mut p.tracks[t];
                    let vel = velocity(tr.vel, g.accent);
                    tr.steps[g.start] = Step { on: true, note: g.note, vel, len: g.len };
                }
                self.selected_step = None;
                self.lead_rhythm = self.lead_rhythm.next();
                true
            }
            Instrument::Bass => {
                let lead = self.lead_line();
                let b = &mut self.bass;
                let n = phrase_steps(b.bars);
                let low = key.tonic_at_or_above(BASS_GEN_LOW);
                let pitches = key.notes_between(low, low + 12);
                if pitches.is_empty() {
                    return false;
                }
                let other: Vec<Option<u8>> = if in_key(&lead, key) { (0..n).map(|i| lead[i % lead.len()]).collect() } else { Vec::new() };
                let v = Voice { pitches: &pitches, other: &other, above: false };
                let notes = generate::generate(self.bass_rhythm, key, &v, n, &mut rng);
                for st in b.steps.iter_mut().take(n) {
                    st.on = false;
                }
                for g in notes {
                    if b.steps[g.start].on {
                        continue; // one voice: the first note on a step wins
                    }
                    let vel = velocity(b.vel, g.accent);
                    b.steps[g.start] = BassStep { on: true, note: g.note, len: g.len, vel };
                }
                b.base = low.min(BASS_TOP);
                self.bass_rhythm = self.bass_rhythm.next();
                true
            }
            Instrument::Drums => false,
        }
    }

    /// The MIDI channel (0-based) an instrument plays on.
    pub fn channel(&self, inst: Instrument) -> u8 {
        match inst {
            Instrument::Lead => self.pattern().channel,
            Instrument::Bass => self.bass.channel,
            Instrument::Drums => self.drums.channel,
        }
    }
}

/// `generate::in_key_line` for the other voice of a phrase.
fn in_key(line: &[Option<u8>], key: Key) -> bool {
    !line.is_empty() && generate::in_key_line(line, key)
}

/// Grow a phrase from `old` to `new` bars: the bars that come into play are
/// copies of the ones already there, repeated (a 2-bar phrase made 8 bars
/// long plays its two bars four times). Shrinking keeps the leading bars and
/// leaves the rest as they are; they are simply not played.
fn extend_bars<T: Clone>(steps: &mut [T], old: usize, new: usize) {
    let old = old.max(1);
    for bar in old..new.min(MAX_BARS) {
        let src = bar % old;
        let (head, tail) = steps.split_at_mut(bar * STEPS);
        tail[..STEPS].clone_from_slice(&head[src * STEPS..(src + 1) * STEPS]);
    }
}

/// Steps in a phrase of `bars` bars.
pub fn phrase_steps(bars: u8) -> usize {
    bars.clamp(1, MAX_BARS as u8) as usize * STEPS
}

/// Which note sounds on each step of a phrase of `n` steps: the absolute
/// step of the note's head, for the head itself and for the steps its
/// length covers (wrapping at the end of the phrase). A note ends where
/// the next one starts. `on_len` gives the length of the note starting at a
/// step, or `None` if none does.
pub fn cover_map(on_len: impl Fn(usize) -> Option<usize>, n: usize) -> [Option<u16>; MAX_STEPS] {
    let n = n.clamp(1, MAX_STEPS);
    let mut map = [None; MAX_STEPS];
    for (i, m) in map.iter_mut().enumerate().take(n) {
        if on_len(i).is_some() {
            *m = Some(i as u16);
        }
    }
    for i in 0..n {
        let Some(len) = on_len(i) else { continue };
        for j in 1..len.min(n) {
            let c = (i + j) % n;
            if map[c].is_some() {
                break;
            }
            map[c] = Some(i as u16);
        }
    }
    map
}

/// The drag bar (see `Drag::bar`) for a finger on a step inside the note
/// with head `head`, in bar `bar` of a phrase of `n` steps: the bar itself
/// when it comes after the head's bar, a phrase later when the note wrapped
/// around the end of the phrase to get there.
fn drag_bar(head: usize, bar: usize, n: usize) -> i64 {
    if bar * STEPS > head {
        bar as i64
    } else {
        (bar + n / STEPS) as i64
    }
}

/// The drag bar after moving it by `dir` bars, if the drag can go there:
/// not before the head's bar and not a whole phrase past it.
fn drag_bar_step(d: Drag, dir: i32, n: usize) -> Option<i64> {
    let bar = d.bar + dir.signum() as i64;
    let head_bar = (d.head / STEPS) as i64;
    (bar >= head_bar && bar - head_bar < (n / STEPS) as i64).then_some(bar)
}

/// Length of the dragged note when the finger is on column `k` of the drag
/// bar: from the head to that step; one step when the finger is left of the
/// head; never longer than the phrase or than up to the next note on the
/// track.
fn drag_len(d: Drag, k: usize, n: usize, on: impl Fn(usize) -> bool) -> u16 {
    let idx = d.bar * STEPS as i64 + k as i64;
    let len = (idx - d.head as i64 + 1).clamp(1, n as i64) as usize;
    let next = (1..n).find(|j| on((d.head + j) % n)).unwrap_or(n);
    len.min(next).max(1) as u16
}

/// Note name like "C1" (MIDI 36). Octave numbering with middle C = C3.
pub fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[(n % 12) as usize], (n / 12) as i32 - 2)
}

/// Short name of a General MIDI percussion note for the pads ("KICK",
/// "SNARE", ...), the note name for anything else.
pub fn drum_name(n: u8) -> String {
    let s = match n {
        35 => "KICK2",
        36 => "KICK",
        37 => "RIM",
        38 => "SNARE",
        39 => "CLAP",
        40 => "SNAR2",
        41 | 43 => "FTOM",
        42 => "CHAT",
        44 => "PHAT",
        45 => "LTOM",
        46 => "OHAT",
        47 | 48 => "MTOM",
        49 => "CRASH",
        50 => "HTOM",
        51 => "RIDE",
        52 => "CHINA",
        53 => "BELL",
        54 => "TAMB",
        55 => "SPLSH",
        56 => "COWBL",
        57 => "CRSH2",
        59 => "RIDE2",
        60 | 61 => "BONGO",
        62..=64 => "CONGA",
        65 | 66 => "TIMB",
        67 | 68 => "AGOGO",
        69 => "CABSA",
        70 => "MARAC",
        75 => "CLAVE",
        76 | 77 => "WBLCK",
        80 | 81 => "TRIAN",
        82 => "SHAKR",
        _ => return note_name(n),
    };
    s.to_string()
}

/// Bar (counted from the grid's downbeat, may be negative) and step in that
/// bar for beat `n` and 16th `sub` (0..3).
#[inline]
pub fn bar_and_step(g: &Grid, n: i64, sub: i64) -> (i64, usize) {
    let bl = g.bar_len.max(1) as i64;
    let rel = n - g.downbeat as i64;
    (rel.div_euclid(bl), ((rel.rem_euclid(bl) * 4 + sub) as usize) % STEPS)
}

/// Position of a step in the bar for beat `n` and 16th `sub` (0..3).
#[inline]
pub fn step_in_bar(g: &Grid, n: i64, sub: i64) -> usize {
    bar_and_step(g, n, sub).1
}

/// Beat and 16th the clock is on at `t`, if it runs.
fn quantise(g: &Grid, t: f64) -> Option<(i64, i64)> {
    if !g.running || g.period <= 0.0 {
        return None;
    }
    let q = ((t - g.t0) / g.period * 4.0).floor() as i64;
    Some((g.n0 + q.div_euclid(4), q.rem_euclid(4)))
}

/// The step the playhead is on at time `t`, if the clock runs.
pub fn playhead(g: &Grid, t: f64) -> Option<usize> {
    quantise(g, t).map(|(n, sub)| step_in_bar(g, n, sub))
}

/// Playhead in a phrase of `bars` bars: (bar within the phrase, step).
pub fn phrase_playhead(g: &Grid, t: f64, bars: usize) -> Option<(usize, usize)> {
    quantise(g, t).map(|(n, sub)| {
        let (bar, k) = bar_and_step(g, n, sub);
        (bar.rem_euclid(bars.max(1) as i64) as usize, k)
    })
}

/// The 16th nearest to `t` in a phrase of `bars` bars, for recording a pad
/// tap; `None` while the clock stands.
pub fn phrase_nearest(g: &Grid, t: f64, bars: usize) -> Option<RecPos> {
    if !g.running || g.period <= 0.0 {
        return None;
    }
    let x = (t - g.t0) / g.period * 4.0;
    let q = x.round() as i64;
    let (n, sub) = (g.n0 + q.div_euclid(4), q.rem_euclid(4));
    let (bar, step) = bar_and_step(g, n, sub);
    Some(RecPos { bar: bar.rem_euclid(bars.max(1) as i64) as usize, step, ahead: q as f64 > x })
}

/// Where notes go. `MidiOut` for real, a recorder for tests and the simulator.
pub trait NoteOut {
    fn note_on(&self, channel: u8, note: u8, vel: u8) -> bool;
    fn note_off(&self, channel: u8, note: u8) -> bool;
}

impl NoteOut for MidiOut {
    fn note_on(&self, channel: u8, note: u8, vel: u8) -> bool {
        MidiOut::note_on(self, channel, note, vel).is_ok()
    }
    fn note_off(&self, channel: u8, note: u8) -> bool {
        MidiOut::note_off(self, channel, note).is_ok()
    }
}

#[derive(Default)]
pub struct SeqStats {
    pub notes: AtomicU64,
    pub errors: AtomicU64,
}

/// `Pending::epoch` of a note-off at a fixed time (a pad hit): never
/// re-anchored to the grid.
const NO_EPOCH: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Pending {
    /// Beat and 16th of the note-off, on the grid of `epoch`; `t_off` is
    /// recomputed from the current grid while the epoch matches, so a long
    /// note follows tempo corrections.
    n: i64,
    sub: i64,
    epoch: u32,
    /// Swing: how many steps after the 16th the note-off falls.
    shift: f64,
    t_off: f64,
    channel: u8,
    note: u8,
    /// True for the bass voice, which is monophonic.
    bass: bool,
}

/// A drum hit the swing holds back: it goes out `shift` steps after its 16th.
#[derive(Clone, Copy)]
struct Delayed {
    n: i64,
    sub: i64,
    epoch: u32,
    shift: f64,
    t_on: f64,
    channel: u8,
    note: u8,
    vel: u8,
}

/// Send a note-on and remember its note-off `p`. The same note still
/// sounding on that channel is released first (a repeat inside a long
/// note), so the synth gets a clean off/on.
fn strike(midi: &dyn NoteOut, stats: &SeqStats, pending: &mut Vec<Pending>, vel: u8, p: Pending) {
    if let Some(i) = pending.iter().position(|q| q.channel == p.channel && q.note == p.note) {
        let q = pending.swap_remove(i);
        let _ = midi.note_off(q.channel, q.note);
    }
    if !midi.note_on(p.channel, p.note, vel) {
        stats.errors.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.notes.fetch_add(1, Ordering::Relaxed);
    }
    if pending.len() < pending.capacity() {
        pending.push(p);
    } else {
        let _ = midi.note_off(p.channel, p.note);
    }
}

/// Extra note lead the SETUP page allows, in ms: negative delays the notes.
pub const NOTE_LEAD_RANGE: (i32, i32) = (-50, 150);

/// A 16th the grid moved into the past (a phase correction pulled the beat
/// earlier while the sequencer was waiting for it) is still played, up to
/// this late, or half a step if that is less. Later than that the grid
/// really jumped (a relock) and the step is skipped.
const CATCH_UP_S: f64 = 0.06;

/// The sequencer thread body. Runs until `stop` is set.
pub fn run<G: FnMut() -> Grid, S: FnMut() -> std::sync::Arc<Snapshot>>(
    mut get_grid: G,
    mut get_snapshot: S,
    midi: &dyn NoteOut,
    base_lead_s: f64,
    stop: &AtomicBool,
    stats: &SeqStats,
) {
    let poll = 0.0015;
    let mut pending: Vec<Pending> = Vec::with_capacity(TRACKS * 8 + DRUMS * 2 + 2);
    let mut delayed: Vec<Delayed> = Vec::with_capacity(DRUMS * 2);
    // Last 16th played, as an absolute index (beat * 4 + 16th). Absolute, not
    // relative to `n0`: the tracker re-anchors `n0`/`t0` every tempo
    // estimate while beat numbers stay put.
    let mut last_a: Option<i64> = None;
    let mut epoch = u32::MAX;
    // Serial of the last pad hit seen; the first snapshot's is adopted, not
    // played (it may be a tap from before the thread started).
    let mut last_hit: Option<u64> = None;
    let all_off = |pending: &mut Vec<Pending>| {
        for p in pending.drain(..) {
            if !midi.note_off(p.channel, p.note) {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    let offs_due = |pending: &mut Vec<Pending>, t: f64| {
        pending.retain(|p| {
            if p.t_off <= t {
                if !midi.note_off(p.channel, p.note) {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                }
                false
            } else {
                true
            }
        });
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            all_off(&mut pending);
            return;
        }
        let g = get_grid();
        let snap = get_snapshot();
        let lead_s = base_lead_s + snap.note_lead_ms as f64 / 1000.0;
        let tnow = now();
        if g.running {
            let step_len = g.period / 4.0;
            for p in &mut pending {
                if p.epoch == g.epoch {
                    p.t_off = g.tick_time(p.n, (p.sub * 6) as u32) + p.shift * step_len - lead_s;
                }
            }
            for d in &mut delayed {
                if d.epoch == g.epoch {
                    d.t_on = g.tick_time(d.n, (d.sub * 6) as u32) + d.shift * step_len - lead_s;
                }
            }
        }
        // A pad tapped on the screen plays at once, clock or no clock.
        let serial = snap.hit.map_or(0, |h| h.serial);
        if let (Some(h), Some(last)) = (snap.hit, last_hit) {
            if h.serial > last {
                let p = Pending { n: 0, sub: 0, epoch: NO_EPOCH, shift: 0.0, t_off: tnow + 0.1, channel: h.channel, note: h.note, bass: false };
                strike(midi, stats, &mut pending, h.vel, p);
            }
        }
        last_hit = Some(serial);
        // Note-offs are due regardless of the clock state.
        let next_off = pending.iter().map(|p| p.t_off).fold(f64::INFINITY, f64::min);
        if !g.running || !snap.playing {
            delayed.clear();
            if !pending.is_empty() && next_off <= tnow {
                offs_due(&mut pending, tnow);
            }
            last_a = None;
            sleep_until(tnow + 0.002);
            continue;
        }
        if g.epoch != epoch {
            epoch = g.epoch;
            last_a = None;
        }
        // Next 16th at or after now + lead, as an absolute 16th index.
        let step_len = g.period / 4.0;
        let mut a = g.n0 * 4 + ((tnow + lead_s - g.t0) / step_len).ceil() as i64;
        if let Some(la) = last_a {
            if a <= la {
                a = la + 1;
            }
            if a > la + 2 * STEPS as i64 {
                // The grid jumped far ahead; do not burst.
                last_a = None;
            } else if a > la + 1 {
                // The 16th after the last one played is already behind us:
                // a correction moved the grid earlier. Play it late rather
                // than drop it, unless it is too far gone.
                let m = la + 1;
                let t_missed = g.tick_time(m.div_euclid(4), (m.rem_euclid(4) * 6) as u32) - lead_s;
                if tnow - t_missed <= CATCH_UP_S.min(step_len * 0.5) {
                    a = m;
                }
            }
        }
        let n = a.div_euclid(4);
        let sub = a.rem_euclid(4);
        let t_on = g.tick_time(n, (sub * 6) as u32) - lead_s;
        let next_delayed = delayed.iter().map(|d| d.t_on).fold(f64::INFINITY, f64::min);
        let next = t_on.min(next_off).min(next_delayed);
        if next - tnow > poll {
            sleep_until(tnow + poll);
            continue;
        }
        sleep_until(next);
        let t = now();
        // Note-offs first, so a retriggered note gets a clean off/on.
        offs_due(&mut pending, t + 0.0002);
        // Note-off `len` steps after 16th `a0`: beat, 16th and time on this grid.
        let off = |a0: i64, len: u16, shift: f64| {
            let a_off = a0 + len.max(1) as i64;
            let (n_off, sub_off) = (a_off.div_euclid(4), a_off.rem_euclid(4));
            (n_off, sub_off, g.tick_time(n_off, (sub_off * 6) as u32) + shift * step_len - lead_s)
        };
        // Swung drum hits that are due.
        let mut i = 0;
        while i < delayed.len() {
            if delayed[i].t_on > t + 0.0002 {
                i += 1;
                continue;
            }
            let d = delayed.swap_remove(i);
            let (n_off, sub_off, t_off) = off(d.n * 4 + d.sub, 1, d.shift);
            let p = Pending { n: n_off, sub: sub_off, epoch: d.epoch, shift: d.shift, t_off, channel: d.channel, note: d.note, bass: false };
            strike(midi, stats, &mut pending, d.vel, p);
        }
        if t_on <= t + 0.0002 {
            let (bar, k) = bar_and_step(&g, n, sub);
            let pidx = bar.rem_euclid(snap.pattern.bars.max(1) as i64) as usize * STEPS + k;
            for tr in &snap.pattern.tracks {
                if tr.muted {
                    continue;
                }
                let st = tr.steps.get(pidx).copied().unwrap_or_default();
                if !st.on {
                    continue;
                }
                let (n_off, sub_off, t_off) = off(a, st.len, 0.0);
                let p = Pending { n: n_off, sub: sub_off, epoch: g.epoch, shift: 0.0, t_off, channel: snap.pattern.channel, note: st.note, bass: false };
                strike(midi, stats, &mut pending, st.vel, p);
            }
            // --- bass: one voice, the phrase wraps every `bars` bars ------------
            let b = &snap.bass;
            let idx = bar.rem_euclid(b.bars.max(1) as i64) as usize * STEPS + k;
            if let Some(st) = b.steps.get(idx).filter(|st| st.on) {
                pending.retain(|p| {
                    if p.bass {
                        if !midi.note_off(p.channel, p.note) {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    !p.bass
                });
                let (n_off, sub_off, t_off) = off(a, st.len, 0.0);
                let p = Pending { n: n_off, sub: sub_off, epoch: g.epoch, shift: 0.0, t_off, channel: b.channel, note: st.note, bass: true };
                strike(midi, stats, &mut pending, st.vel, p);
            }
            // --- drums: one-16th hits, plain or accented; every second 16th
            // is held back by the swing and fired from `delayed`.
            let d = &snap.drums;
            let idx = bar.rem_euclid(d.bars.max(1) as i64) as usize * STEPS + k;
            let shift = if k % 2 == 1 { d.swing_shift() } else { 0.0 };
            for (i, dr) in d.drums.iter().enumerate().take(DRUMS) {
                let Some(st) = dr.steps.get(idx).filter(|st| st.on) else { continue };
                if !d.audible(i) {
                    continue;
                }
                let vel = if st.accent { ACCENT_VEL } else { dr.vel };
                if shift > 0.0 && delayed.len() < delayed.capacity() {
                    let t_on = t_on + shift * step_len;
                    delayed.push(Delayed { n, sub, epoch: g.epoch, shift, t_on, channel: d.channel, note: dr.note, vel });
                } else {
                    let (n_off, sub_off, t_off) = off(a, 1, 0.0);
                    let p = Pending { n: n_off, sub: sub_off, epoch: g.epoch, shift: 0.0, t_off, channel: d.channel, note: dr.note, bass: false };
                    strike(midi, stats, &mut pending, vel, p);
                }
            }
            last_a = Some(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_page_channels() {
        use SeqAction as A;
        let mut s = SeqState::default();
        let all: Vec<_> = Instrument::all().collect();
        assert_eq!(all, [Instrument::Lead, Instrument::Bass, Instrument::Drums]);
        assert_eq!((s.channel(Instrument::Lead), s.channel(Instrument::Bass), s.channel(Instrument::Drums)), (9, 0, 9));
        // each instrument on its own, the others untouched
        assert!(s.apply(A::Channel(Instrument::Lead, 1)));
        assert_eq!(s.pattern().channel, 10);
        assert_eq!(s.patterns[1].channel, 9, "the other patterns keep theirs");
        assert_eq!(s.drums.channel, 9);
        assert!(s.apply(A::Channel(Instrument::Bass, 3)));
        assert_eq!(s.bass.channel, 3);
        assert!(s.apply(A::Channel(Instrument::Drums, -1)));
        assert_eq!(s.drums.channel, 8);
        // clamped to the 16 channels; nothing to save when nothing moved
        assert!(s.apply(A::Channel(Instrument::Bass, -100)));
        assert_eq!(s.bass.channel, 0);
        assert!(!s.apply(A::Channel(Instrument::Bass, -1)));
        assert!(s.apply(A::Channel(Instrument::Drums, 100)));
        assert_eq!(s.drums.channel, 15);
        assert!(!s.apply(A::Channel(Instrument::Drums, 1)));
        assert!(!A::Channel(Instrument::Bass, 1).view_only(), "a channel change is played and saved");
        // the playing thread sees it
        assert_eq!(s.snapshot().bass.channel, 0);
        assert_eq!(s.snapshot().pattern.channel, 10);
        // a file from when every track had a channel: the first track's is the pattern's
        let old = "current = 0\n[[patterns]]\n[[patterns.tracks]]\nchannel = 3\nnote = 36\nvel = 100\nmuted = false\nsteps = []\n[[patterns.tracks]]\nchannel = 5\nnote = 38\nvel = 100\nmuted = false\nsteps = []\n";
        let dir = std::env::temp_dir().join(format!("chan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        std::fs::write(&path, old).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.pattern().channel, 3);
        assert_eq!(l.patterns[1].channel, 9, "patterns the file did not have get the default");
        l.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("channel = 3\n") && !text.contains("channel = 5"), "saved once, for the pattern:\n{text}");
        assert_eq!(SeqState::load(&path).unwrap().pattern().channel, 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_and_edit_steps() {
        let mut s = SeqState::default();
        assert!(s.apply(SeqAction::ToggleStep(1, 4)));
        let st = s.pattern().tracks[1].steps[4];
        assert!(st.on && st.note == 38 && st.vel == 100 && st.len == 1);
        assert_eq!(s.track, 1);
        // edit mode: select the step and change its note only
        s.apply(SeqAction::ToggleEdit);
        s.apply(SeqAction::ToggleStep(1, 4));
        assert_eq!(s.selected_step, Some(4));
        s.apply(SeqAction::Adjust(Param::Note, 2));
        assert_eq!(s.pattern().tracks[1].steps[4].note, 40);
        assert_eq!(s.pattern().tracks[1].note, 38);
        s.apply(SeqAction::ToggleEdit);
        s.apply(SeqAction::ToggleStep(1, 4));
        assert!(!s.pattern().tracks[1].steps[4].on);
    }

    #[test]
    fn pattern_bars() {
        let mut s = SeqState::default();
        assert!(!s.apply(SeqAction::Bar(1)), "a 1-bar pattern has no bar 2");
        assert!(s.apply(SeqAction::Bars(4)));
        assert!(s.apply(SeqAction::Bar(2)));
        assert!(s.apply(SeqAction::ToggleStep(0, 5)));
        assert!(s.pattern().tracks[0].steps[2 * STEPS + 5].on);
        assert!(!s.pattern().tracks[0].steps[5].on);
        // edit mode addresses the step in the shown bar
        s.apply(SeqAction::ToggleEdit);
        s.apply(SeqAction::ToggleStep(0, 5));
        assert_eq!(s.selected_step, Some(2 * STEPS + 5));
        s.apply(SeqAction::Adjust(Param::Note, 1));
        assert_eq!(s.pattern().tracks[0].steps[2 * STEPS + 5].note, 37);
        // shrinking the phrase pulls the shown bar back in
        assert!(s.apply(SeqAction::Bars(2)));
        assert_eq!(s.shown_bar(), 1);
        assert_eq!(s.selected_step, None);
        // growing repeats the bars already there: bars 1-2 become 1-2-1-2-1-2-1-2
        s.apply(SeqAction::ToggleEdit);
        assert!(s.apply(SeqAction::Bar(0)));
        assert!(s.apply(SeqAction::ToggleStep(1, 3)));
        assert!(s.apply(SeqAction::Bar(1)));
        assert!(s.apply(SeqAction::ToggleStep(1, 9)));
        assert!(s.apply(SeqAction::Bars(8)));
        let st = &s.pattern().tracks[1].steps;
        for bar in 0..8 {
            assert_eq!(st[bar * STEPS + 3].on, bar % 2 == 0, "bar {bar} step 3");
            assert_eq!(st[bar * STEPS + 9].on, bar % 2 == 1, "bar {bar} step 9");
        }
        // the old bar 3 (edited above at step 5, note 37) was overwritten by bar 1
        assert!(!st[2 * STEPS + 5].on);
        assert_eq!(st[2 * STEPS + 5].note, st[5].note);
        // picking a bar locks the screen to it; following then does nothing
        assert!(s.bar_lock);
        assert_eq!(s.shown_bar(), 1);
        assert!(!s.apply(SeqAction::FollowBar(3)));
        assert_eq!(s.shown_bar(), 1);
        assert!(s.apply(SeqAction::ToggleBarLock));
        assert!(!s.bar_lock);
        assert!(s.apply(SeqAction::FollowBar(3)));
        assert_eq!(s.shown_bar(), 3);
        assert!(!s.apply(SeqAction::FollowBar(3)), "already there");
        assert!(!s.apply(SeqAction::FollowBar(8)), "beyond the phrase");
        assert!(SeqAction::FollowBar(0).view_only() && !SeqAction::Bars(1).view_only());
        // a 16-step file from before phrases still loads, as one bar
        let old = "current = 0\n[[patterns]]\n[[patterns.tracks]]\nchannel = 9\nnote = 36\nvel = 100\ngate = 50\nmuted = false\nsteps = []\n";
        let dir = std::env::temp_dir().join(format!("bars-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        std::fs::write(&path, old).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.pattern().bars, 1);
        assert_eq!(l.pattern().tracks[0].steps.len(), MAX_STEPS);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adjust_clamps() {
        let mut s = SeqState::default();
        s.apply(SeqAction::Adjust(Param::Note, -500));
        assert_eq!(s.pattern().tracks[0].note, 0);
        s.apply(SeqAction::Adjust(Param::Velocity, -500));
        assert_eq!(s.pattern().tracks[0].vel, 1);
    }

    #[test]
    fn drag_sets_note_length() {
        use SeqAction::*;
        let mut s = SeqState::default();
        s.apply(Bars(2));
        s.apply(Bar(0));
        let len = |s: &SeqState, i: usize| s.pattern().tracks[3].steps[i].len;
        let cov = |s: &SeqState| {
            let tr = &s.pattern().tracks[3];
            cover_map(|i| tr.steps[i].on.then_some(tr.steps[i].len as usize), 32)
        };
        // down on an empty cell makes a one-step note at once
        assert!(s.apply(CellDown(3, 4)));
        assert_eq!(len(&s, 4), 1);
        // dragging right lengthens it, dragging left of the head leaves one step
        assert!(s.apply(CellDrag(9)));
        assert_eq!(len(&s, 4), 6);
        assert!(!s.apply(CellDrag(9)), "same column again changes nothing");
        assert!(s.apply(CellDrag(2)));
        assert_eq!(len(&s, 4), 1);
        assert!(s.apply(CellDrag(15)));
        assert!(!s.apply(CellUp), "lifting after a drag does not clear");
        assert_eq!(len(&s, 4), 12);
        assert_eq!(cov(&s)[4], Some(4));
        assert_eq!(cov(&s)[15], Some(4));
        assert_eq!(cov(&s)[16], None);
        // a tap on the head clears the note
        assert!(!s.apply(CellDown(3, 4)));
        assert!(s.apply(CellUp));
        assert!(!s.pattern().tracks[3].steps[4].on);
        // across the bar: from step 14, hold at the right edge -> bar 2 shows,
        // locked, the note fills it; drag back to its step 1
        s.apply(CellDown(3, 14));
        s.apply(CellDrag(15));
        assert!(s.apply(CellDragBar(1)));
        assert_eq!((s.shown_bar(), s.bar_lock), (1, true));
        assert_eq!(len(&s, 14), 18);
        assert!(s.apply(CellDrag(1)));
        assert!(!s.apply(CellUp));
        assert_eq!(len(&s, 14), 4);
        assert!(!s.apply(CellDragBar(1)), "a note cannot be longer than the phrase");
        // in bar 2 the continuation can be dragged on
        assert!(!s.apply(CellDown(3, 0)), "the tail belongs to the note in bar 1: nothing changes yet");
        assert!(s.apply(CellDrag(3)));
        assert!(!s.apply(CellUp));
        assert_eq!(len(&s, 14), 6);
        assert_eq!(cov(&s)[16 + 3], Some(14));
        assert_eq!(cov(&s)[16 + 4], None);
        // back over the edge to the left: the note ends on step 1 of bar 1 ... no,
        // the drag cannot go before the head's bar; from bar 2 it can
        s.apply(CellDown(3, 3));
        assert!(s.apply(CellDragBar(-1)));
        assert_eq!((s.shown_bar(), len(&s, 14)), (0, 1));
        assert!(!s.apply(CellDragBar(-1)));
        s.apply(CellDrag(15));
        s.apply(CellUp);
        assert_eq!(len(&s, 14), 2);
        s.apply(CellDown(3, 14));
        s.apply(CellDragBar(1));
        s.apply(CellDrag(3));
        s.apply(CellUp);
        assert_eq!(len(&s, 14), 6);
        assert!(s.apply(Bar(1)));
        // a tap inside a note cuts it and starts a new one; a later note caps a drag
        assert!(s.apply(ToggleStep(3, 1)));
        assert_eq!(len(&s, 14), 3);
        assert_eq!(len(&s, 17), 1);
        s.apply(ToggleStep(3, 8));
        s.apply(CellDown(3, 1));
        s.apply(CellDrag(12));
        s.apply(CellUp);
        assert_eq!(len(&s, 17), 7, "stops before the note on step 8");
        // the phrase wraps: a note at the end of the last bar dragged over the
        // edge runs into bar 1, up to the next note there (step 14)
        s.apply(ToggleStep(3, 15));
        s.apply(CellDown(3, 15));
        assert!(s.apply(CellDragBar(1)));
        assert_eq!(s.shown_bar(), 0);
        assert_eq!(len(&s, 31), 15);
        assert!(s.apply(CellDrag(2)));
        s.apply(CellUp);
        assert_eq!(len(&s, 31), 4);
        assert_eq!(cov(&s)[2], Some(31));
        assert_eq!(cov(&s)[3], None);
        // and its wrapped tail in bar 1 can be dragged too
        assert!(!s.apply(CellDown(3, 1)));
        assert!(s.apply(CellDrag(5)));
        s.apply(CellUp);
        assert_eq!(len(&s, 31), 7);
        // a file from before lengths (with gates) still loads; lengths are 1
        let old = "current = 0\n[[patterns]]\n[[patterns.tracks]]\nchannel = 9\nnote = 36\nvel = 100\ngate = 50\nmuted = false\n[[patterns.tracks.steps]]\non = true\nnote = 36\nvel = 100\n";
        let dir = std::env::temp_dir().join(format!("len-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        std::fs::write(&path, old).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert!(l.pattern().tracks[0].steps[0].on && l.pattern().tracks[0].steps[0].len == 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut s = SeqState::default();
        s.apply(SeqAction::ToggleStep(0, 0));
        s.apply(SeqAction::ToggleStep(0, 8));
        s.apply(SeqAction::SelectPattern(2));
        s.apply(SeqAction::ToggleStep(3, 15));
        let dir = std::env::temp_dir().join(format!("seq-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        s.save(&path).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.patterns, s.patterns);
        assert_eq!(l.current, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// (time, on?, channel, note, velocity)
    type Event = (f64, bool, u8, u8, u8);
    struct Recorder(std::sync::Mutex<Vec<Event>>);
    impl NoteOut for Recorder {
        fn note_on(&self, channel: u8, note: u8, vel: u8) -> bool {
            self.0.lock().unwrap().push((now(), true, channel, note, vel));
            true
        }
        fn note_off(&self, channel: u8, note: u8) -> bool {
            self.0.lock().unwrap().push((now(), false, channel, note, 0));
            true
        }
    }

    #[test]
    fn plays_steps_on_the_grid() {
        // 240 BPM: a 16th every 62.5 ms. Steps 0 and 8 on track 0, step 4 on track 1.
        let mut st = SeqState::default();
        st.apply(SeqAction::ToggleStep(0, 0));
        st.apply(SeqAction::ToggleStep(0, 8));
        st.apply(SeqAction::ToggleStep(1, 4));
        // the kick on step 8 rings for 3 steps
        st.apply(SeqAction::CellDown(0, 8));
        st.apply(SeqAction::CellDrag(10));
        st.apply(SeqAction::CellUp);
        let snap = std::sync::Arc::new(st.snapshot());
        let t0 = now() + 0.05;
        let grid = Grid { running: true, epoch: 1, t0, n0: 0, period: 0.25, ..Grid::default() };
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 1.02); // one bar plus a bit
                stop.store(true, Ordering::Relaxed);
            });
            run(|| grid, || snap.clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        let ons: Vec<_> = ev.iter().filter(|e| e.1).collect();
        // bar 1: steps 0, 4, 8; bar 2: step 0 (at t0 + 1.0)
        assert_eq!(ons.len(), 4, "{ev:?}");
        let expect = [(0, 36u8), (4, 38), (8, 36), (16, 36)];
        for (e, (step, note)) in ons.iter().zip(expect) {
            let want = t0 + step as f64 * 0.0625;
            assert!((e.0 - want).abs() < 0.003, "step {step}: {} vs {want}", e.0);
            assert_eq!(e.3, note);
        }
        // every note-on got its note-off: one step later, three for the long kick
        let offs: Vec<_> = ev.iter().filter(|e| !e.1).collect();
        assert_eq!(offs.len(), 4);
        let snare_off = offs.iter().find(|e| e.3 == 38).unwrap();
        assert!((snare_off.0 - (t0 + 5.0 * 0.0625)).abs() < 0.004, "len: {}", snare_off.0 - t0);
        let kick_offs: Vec<f64> = offs.iter().filter(|e| e.3 == 36).map(|e| e.0 - t0).collect();
        assert!(kick_offs.iter().any(|&t| (t - 11.0 * 0.0625).abs() < 0.004), "long kick off: {kick_offs:?}");
    }

    #[test]
    fn follows_a_grid_that_re_anchors_every_beat() {
        // The tracker moves n0/t0 to the most recent beat on every tempo
        // estimate. Beat numbers stay put, so the sequencer must keep firing.
        let mut st = SeqState::default();
        st.apply(SeqAction::ToggleStep(0, 0));
        st.apply(SeqAction::ToggleStep(0, 4));
        st.apply(SeqAction::ToggleStep(0, 8));
        st.apply(SeqAction::ToggleStep(0, 12));
        let snap = std::sync::Arc::new(st.snapshot());
        let t0 = now() + 0.05;
        let base = Grid { running: true, epoch: 1, t0, n0: 0, period: 0.25, ..Grid::default() };
        let get_grid = || {
            let (n, _) = base.nearest_beat(now());
            Grid { t0: base.beat_time(n), n0: n, ..base }
        };
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 1.02);
                stop.store(true, Ordering::Relaxed);
            });
            run(get_grid, || snap.clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        let ons: Vec<f64> = ev.iter().filter(|e| e.1).map(|e| e.0 - t0).collect();
        assert_eq!(ons.len(), 5, "{ons:?}");
        for (i, t) in ons.iter().enumerate() {
            assert!((t - i as f64 * 0.25).abs() < 0.003, "beat {i}: {t}");
        }
    }

    #[test]
    fn a_phase_correction_backwards_does_not_drop_a_step() {
        // Bass on every beat at 240 BPM. 10 ms before beat 1 the grid is
        // pulled 30 ms earlier (a tracker phase correction), so beat 1 is
        // suddenly 20 ms in the past: it must still be played, a little
        // late, not skipped.
        let mut st = SeqState::default();
        for k in [0, 4, 8, 12] {
            st.apply(SeqAction::BassSet(0, k));
        }
        let snap = std::sync::Arc::new(st.snapshot());
        let t0 = now() + 0.05;
        let base = Grid { running: true, epoch: 1, t0, n0: 0, period: 0.25, ..Grid::default() };
        let get_grid = || if now() >= t0 + 0.24 { Grid { t0: t0 - 0.03, ..base } } else { base };
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 0.9);
                stop.store(true, Ordering::Relaxed);
            });
            run(get_grid, || snap.clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        let ons: Vec<f64> = ev.iter().filter(|e| e.1).map(|e| e.0 - t0).collect();
        // beats 0, 1 (late), 2, 3 on the shifted grid
        assert_eq!(ons.len(), 4, "{ons:?}");
        assert!((ons[1] - 0.24).abs() < 0.004, "beat 1 should play right after the shift: {ons:?}");
        assert!((ons[2] - 0.47).abs() < 0.004 && (ons[3] - 0.72).abs() < 0.004, "{ons:?}");
    }

    #[test]
    fn bass_edits_and_round_trip() {
        let mut s = SeqState::default();
        assert_eq!(s.bass.bars, 2);
        assert!(s.apply(SeqAction::BassSet(0, 0))); // C1 on step 0 of bar 0
        assert!(s.apply(SeqAction::BassSet(7, 4))); // G1 on step 4
        assert!(s.apply(SeqAction::BassSet(7, 4))); // same cell again clears it
        assert!(s.apply(SeqAction::BassSet(3, 4))); // D#1
        assert!(s.apply(SeqAction::BassBar(1)));
        assert!(s.apply(SeqAction::BassSet(0, 8)));
        assert!(!s.apply(SeqAction::BassBar(2)), "bar 3 does not exist in a 2-bar phrase");
        let st = &s.bass.steps;
        assert!(st[0].on && st[0].note == 36);
        assert!(st[4].on && st[4].note == 39);
        assert!(st[16 + 8].on && st[16 + 8].note == 36);
        assert!(s.apply(SeqAction::BassScroll(5)));
        assert_eq!(s.bass.base, 41);
        assert!(s.apply(SeqAction::BassScroll(-100)));
        assert_eq!(s.bass.base, 0);
        assert!(!s.apply(SeqAction::BassScroll(-1)), "already at the bottom");
        assert!(s.apply(SeqAction::BassScroll(127)));
        assert_eq!(s.bass.base, BASS_TOP);
        assert!(s.apply(SeqAction::BassScroll(36 - BASS_TOP as i32)));
        // a tap on another row moves the note and keeps its length (bar 2 is shown)
        s.apply(SeqAction::BassCellDown(3, 4));
        s.apply(SeqAction::BassCellDrag(6));
        s.apply(SeqAction::BassCellUp);
        assert_eq!(s.bass.steps[20], BassStep { on: true, note: 39, len: 3, vel: 100 });
        assert!(s.apply(SeqAction::BassSet(5, 4)));
        assert_eq!(s.bass.steps[20], BassStep { on: true, note: 41, len: 3, vel: 100 });
        // a tap inside it, in its own row, cuts it and starts a new note there
        assert!(s.apply(SeqAction::BassSet(5, 6)));
        assert_eq!(s.bass.steps[20].len, 2);
        assert_eq!(s.bass.steps[22], BassStep { on: true, note: 41, len: 1, vel: 100 });
        // a tap on the head in its own row clears it
        assert!(s.apply(SeqAction::BassSet(5, 4)));
        assert!(!s.bass.steps[20].on);
        assert!(s.apply(SeqAction::BassBars(1)));
        assert_eq!(s.bass_bar, 0);
        assert!(!s.apply(SeqAction::BassBars(3)));
        // picking bar 2 above locked the screen; unlock so it follows the clock
        assert!(s.bass_bar_lock);
        assert!(s.apply(SeqAction::ToggleBassBarLock));
        assert!(!s.bass_bar_lock);
        assert!(!s.apply(SeqAction::BassFollowBar(1)), "a 1-bar phrase has no bar 2");
        // growing again copies bar 1 into every new bar (the old bar 2 is gone)
        assert!(s.apply(SeqAction::BassBars(4)));
        assert!(s.apply(SeqAction::BassFollowBar(1)));
        assert_eq!(s.bass_bar, 1);
        let st = &s.bass.steps;
        for bar in 0..4 {
            assert!(st[bar * STEPS].on && st[bar * STEPS].note == 36, "bar {bar}");
            assert!(st[bar * STEPS + 4].on && st[bar * STEPS + 4].note == 39, "bar {bar}");
            assert!(!st[bar * STEPS + 8].on, "bar {bar}");
        }
        let dir = std::env::temp_dir().join(format!("bass-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        s.save(&path).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.bass, s.bass);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bass_plays_the_phrase_with_one_voice() {
        // 240 BPM, 2 bars. Bar 0: C1 on step 0 ringing 4 steps, E1 on step 2
        // (cuts the C), bar 1: D1 on step 0. Bar 2 wraps back to bar 0.
        let mut st = SeqState::default();
        st.apply(SeqAction::BassCellDown(0, 0));
        st.apply(SeqAction::BassCellDrag(3)); // C1, 4 steps
        st.apply(SeqAction::BassCellUp);
        st.apply(SeqAction::BassCellDown(4, 2)); // E1 inside the C: cuts it to 2 steps
        st.apply(SeqAction::BassCellDrag(5)); // 4 steps
        st.apply(SeqAction::BassCellUp);
        assert_eq!(st.bass.steps[0].len, 2);
        assert_eq!(st.bass.steps[2].len, 4);
        st.apply(SeqAction::BassBar(1));
        st.apply(SeqAction::BassSet(2, 0));
        let snap = std::sync::Arc::new(st.snapshot());
        let t0 = now() + 0.05;
        let grid = Grid { running: true, epoch: 1, t0, n0: 0, period: 0.25, ..Grid::default() };
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 2.02);
                stop.store(true, Ordering::Relaxed);
            });
            run(|| grid, || snap.clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        let ons: Vec<_> = ev.iter().filter(|e| e.1).collect();
        let expect = [(0, 36u8), (2, 40), (16, 38), (32, 36)];
        assert_eq!(ons.len(), expect.len(), "{ev:?}");
        for (e, (step, note)) in ons.iter().zip(expect) {
            let want = t0 + step as f64 * 0.0625;
            assert!((e.0 - want).abs() < 0.003, "step {step}: {} vs {want}", e.0);
            assert_eq!(e.3, note);
            assert_eq!(e.2, 0);
        }
        // the C ends where the E starts, the E rings its 4 steps
        let offs: Vec<_> = ev.iter().filter(|e| !e.1).collect();
        let c_off = offs.iter().find(|e| e.3 == 36).unwrap();
        assert!((c_off.0 - (t0 + 2.0 * 0.0625)).abs() < 0.004, "cut: {}", c_off.0 - t0);
        let e_off = offs.iter().find(|e| e.3 == 40).unwrap();
        assert!((e_off.0 - (t0 + 6.0 * 0.0625)).abs() < 0.004, "len: {}", e_off.0 - t0);
    }

    #[test]
    fn drum_edits_and_round_trip() {
        use SeqAction as A;
        let mut s = SeqState::default();
        assert_eq!(s.drums.bars, 1);
        assert_eq!(s.snapshot().hit, None);
        // a pad tap selects the instrument and plays it
        assert!(s.apply(A::DrumPad(1, None)));
        assert_eq!(s.drum, 1);
        let hit = s.snapshot().hit.unwrap();
        assert_eq!((hit.serial, hit.channel, hit.note, hit.vel), (1, 9, 38, 100));
        // step taps cycle off -> hit -> accent -> off on the selected pad
        assert!(s.apply(A::DrumStep(4)));
        assert_eq!(s.drums.drums[1].steps[4], DrumStep { on: true, accent: false });
        assert!(s.apply(A::DrumStep(4)));
        assert_eq!(s.drums.drums[1].steps[4], DrumStep { on: true, accent: true });
        assert!(s.apply(A::DrumStep(4)));
        assert!(!s.drums.drums[1].steps[4].on);
        assert!(!s.apply(A::DrumStep(16)) && !s.apply(A::DrumPad(DRUMS, None)));
        // MUTE, SOLO and CLEAR are armed, then applied by pad taps, which
        // then neither select nor play
        s.apply(A::DrumStep(0));
        assert!(s.apply(A::DrumArm(DrumArm::Mute)));
        assert!(s.apply(A::DrumPad(0, None)));
        assert!(s.drums.drums[0].muted && s.drum == 1);
        assert_eq!(s.snapshot().hit.unwrap().serial, 1, "no hit while armed");
        assert!(!s.drums.audible(0) && s.drums.audible(1));
        assert!(s.apply(A::DrumArm(DrumArm::Solo)));
        assert_eq!(s.drum_arm, DrumArm::Solo);
        s.apply(A::DrumPad(2, None));
        assert!(s.drums.drums[2].solo && !s.drums.audible(1) && s.drums.audible(2));
        assert!(s.apply(A::DrumArm(DrumArm::Solo)), "the same button again disarms");
        assert_eq!(s.drum_arm, DrumArm::Off);
        for (arm, pad) in [(DrumArm::Mute, 0), (DrumArm::Solo, 2)] {
            s.apply(A::DrumArm(arm));
            s.apply(A::DrumPad(pad, None));
            s.apply(A::DrumArm(arm));
        }
        assert!((0..DRUMS).all(|i| s.drums.audible(i)));
        s.apply(A::DrumArm(DrumArm::Clear));
        s.apply(A::DrumPad(1, None));
        assert!(s.drums.drums[1].steps.iter().all(|st| !st.on));
        assert_eq!(s.drum_arm, DrumArm::Off, "clear disarms itself");
        // parameters
        s.apply(A::DrumAdjust(DrumParam::Swing, 100));
        assert_eq!(s.drums.swing, SWING_MAX);
        assert!((s.drums.swing_shift() - 0.5).abs() < 1e-9);
        s.apply(A::DrumAdjust(DrumParam::Swing, -100));
        assert_eq!(s.drums.swing, SWING_MIN);
        assert_eq!(s.drums.swing_shift(), 0.0);
        s.apply(A::DrumAdjust(DrumParam::Note, 4));
        assert_eq!(s.drums.drums[1].note, 42);
        s.apply(A::DrumAdjust(DrumParam::Velocity, 100));
        assert_eq!(s.drums.drums[1].vel, 127);
        // bars, as the other phrases
        s.apply(A::DrumStep(3));
        assert!(s.apply(A::DrumBars(4)));
        assert!(s.apply(A::DrumBar(3)));
        assert!(s.drum_bar_lock);
        assert!(s.drums.drums[1].steps[3 * STEPS + 3].on, "copied into the new bars");
        s.apply(A::DrumStep(3)); // accent in bar 4 only
        assert!(s.drums.drums[1].steps[3 * STEPS + 3].accent && !s.drums.drums[1].steps[3].accent);
        assert!(!s.apply(A::DrumFollowBar(1)), "locked");
        s.apply(A::ToggleDrumBarLock);
        assert!(s.apply(A::DrumFollowBar(1)));
        assert_eq!(s.drum_shown_bar(), 1);
        assert!(!s.apply(A::DrumBar(4)));
        assert!(A::DrumFollowBar(0).view_only() && A::DrumArm(DrumArm::Mute).view_only() && A::ToggleDrumRec.view_only());
        assert!(!A::DrumPad(0, None).view_only() && !A::DrumStep(0).view_only());
        // recording: a pad tap lands on the given 16th; when that 16th is
        // still ahead the sequencer plays it and the pad stays quiet
        assert!(s.apply(A::ToggleDrumRec));
        let serial = s.snapshot().hit.unwrap().serial;
        s.apply(A::DrumPad(3, Some(RecPos { bar: 2, step: 7, ahead: false })));
        assert!(s.drums.drums[3].steps[2 * STEPS + 7].on);
        assert_eq!(s.snapshot().hit.unwrap().serial, serial + 1);
        s.apply(A::DrumPad(3, Some(RecPos { bar: 2, step: 9, ahead: true })));
        assert!(s.drums.drums[3].steps[2 * STEPS + 9].on);
        assert_eq!(s.snapshot().hit.unwrap().serial, serial + 1);
        s.apply(A::ToggleDrumRec);
        s.apply(A::DrumPad(3, Some(RecPos { bar: 2, step: 11, ahead: false })));
        assert!(!s.drums.drums[3].steps[2 * STEPS + 11].on, "REC off");
        assert_eq!(s.snapshot().hit.unwrap().serial, serial + 2);
        // round trip; a file from before the kit gets the default kit
        let dir = std::env::temp_dir().join(format!("drums-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        s.save(&path).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.drums, s.drums);
        assert_eq!(l.snapshot().hit, None);
        std::fs::write(&path, "current = 0\n[[patterns]]\n[[patterns.tracks]]\nchannel = 9\nnote = 36\nvel = 100\nmuted = false\nsteps = []\n").unwrap();
        assert_eq!(SeqState::load(&path).unwrap().drums, Drums::default());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(drum_name(36), "KICK");
        assert_eq!(drum_name(100), "E6");
    }

    #[test]
    fn drums_play_with_swing_accent_mute_and_solo() {
        use SeqAction as A;
        // 240 BPM, one bar: an accented kick on step 0 and a plain one on
        // step 8, the closed hat on steps 0..3 with the swing at 75 % (the
        // odd 16ths play half a step late), a muted snare on step 4.
        let mut st = SeqState::default();
        st.apply(A::DrumPad(0, None));
        st.apply(A::DrumStep(0));
        st.apply(A::DrumStep(0));
        st.apply(A::DrumStep(8));
        st.apply(A::DrumPad(4, None));
        for k in 0..4 {
            st.apply(A::DrumStep(k));
        }
        st.apply(A::DrumPad(1, None));
        st.apply(A::DrumStep(4));
        st.apply(A::DrumArm(DrumArm::Mute));
        st.apply(A::DrumPad(1, None));
        st.apply(A::DrumArm(DrumArm::Mute));
        st.apply(A::DrumAdjust(DrumParam::Swing, 25));
        assert!(st.snapshot().hit.is_some(), "pad taps from before the thread starts must not play");
        let snap = std::sync::Arc::new(st.snapshot());
        let t0 = now() + 0.05;
        let grid = Grid { running: true, epoch: 1, t0, n0: 0, period: 0.25, ..Grid::default() };
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 1.02);
                stop.store(true, Ordering::Relaxed);
            });
            run(|| grid, || snap.clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        let step = 0.0625;
        let ons: Vec<(f64, u8, u8)> = ev.iter().filter(|e| e.1).map(|e| ((e.0 - t0) / step, e.3, e.4)).collect();
        let expect = [(0.0, 36u8, 127u8), (0.0, 42, 100), (1.5, 42, 100), (2.0, 42, 100), (3.5, 42, 100), (8.0, 36, 100), (16.0, 36, 127), (16.0, 42, 100)];
        assert_eq!(ons.len(), expect.len(), "{ons:?}");
        for ((t, note, vel), (want_t, want_note, want_vel)) in ons.iter().zip(expect) {
            assert!((t - want_t).abs() * step < 0.003, "{note} at {t} steps, wanted {want_t}: {ons:?}");
            assert_eq!((*note, *vel), (want_note, want_vel), "at {t}: {ons:?}");
        }
        assert!(ev.iter().all(|e| e.2 == 9));
        // a swung hit is released a step after it sounded (the one at 1.5 is
        // cut short by the hat on step 2; the one at 3.5 rings its step)
        let offs: Vec<f64> = ev.iter().filter(|e| !e.1 && e.3 == 42).map(|e| (e.0 - t0) / step).collect();
        assert!(offs.iter().any(|t| (t - 4.5).abs() * step < 0.004), "{offs:?}");
        assert!(offs.iter().any(|t| (t - 2.0).abs() * step < 0.004), "{offs:?}");
        // soloing the kick silences the hat
        let mut st2 = st.clone();
        st2.apply(A::DrumArm(DrumArm::Solo));
        st2.apply(A::DrumPad(0, None));
        let snap = st2.snapshot();
        assert!(snap.drums.audible(0) && !snap.drums.audible(4) && !snap.drums.audible(1));
    }

    #[test]
    fn a_pad_tap_plays_at_once_with_the_clock_stopped() {
        let st = SeqState::default();
        let cur = std::sync::Mutex::new(std::sync::Arc::new(st.snapshot()));
        let grid = Grid::default();
        let rec = Recorder(std::sync::Mutex::new(Vec::new()));
        let stop = AtomicBool::new(false);
        let stats = SeqStats::default();
        let t0 = now();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                sleep_until(t0 + 0.05);
                let mut s2 = st.clone();
                s2.apply(SeqAction::DrumPad(2, None));
                *cur.lock().unwrap() = std::sync::Arc::new(s2.snapshot());
                sleep_until(t0 + 0.1);
                // another edit republishes the same tap: it does not play again
                s2.apply(SeqAction::DrumStep(3));
                *cur.lock().unwrap() = std::sync::Arc::new(s2.snapshot());
                sleep_until(t0 + 0.3);
                stop.store(true, Ordering::Relaxed);
            });
            run(|| grid, || cur.lock().unwrap().clone(), &rec, 0.0, &stop, &stats);
        });
        let ev = rec.0.into_inner().unwrap();
        assert_eq!(ev.len(), 2, "{ev:?}");
        let (t_on, on, ch, note, vel) = ev[0];
        assert!(on && ch == 9 && note == 37 && vel == 100);
        assert!(t_on - (t0 + 0.05) < 0.01, "late by {}", t_on - t0 - 0.05);
        assert!(!ev[1].1 && (ev[1].0 - t_on - 0.1).abs() < 0.01, "{ev:?}");
    }

    #[test]
    fn a_vertical_drag_sets_the_velocity() {
        use SeqAction::*;
        let mut s = SeqState::default();
        // lead: down on an empty cell makes the note, dragging up raises it
        assert!(s.apply(CellDown(2, 3)));
        assert!(s.apply(CellVel(10)));
        assert!(!s.apply(CellVel(0)));
        assert!(!s.apply(CellUp), "moved: lifting the finger keeps the note");
        assert_eq!(s.pattern().tracks[2].steps[3].vel, 110);
        assert!(s.pattern().tracks[2].steps[3].on);
        // on the head of a note: velocity down, still no clearing
        assert!(!s.apply(CellDown(2, 3)));
        assert!(s.apply(CellVel(-200)));
        assert!(!s.apply(CellUp));
        assert_eq!(s.pattern().tracks[2].steps[3].vel, 1);
        assert!(s.pattern().tracks[2].steps[3].on);
        // and clamped at the top; a length drag at the same time still works
        assert!(!s.apply(CellDown(2, 3)));
        assert!(s.apply(CellVel(300)));
        assert!(s.apply(CellDrag(6)));
        assert!(!s.apply(CellUp));
        assert_eq!((s.pattern().tracks[2].steps[3].vel, s.pattern().tracks[2].steps[3].len), (127, 4));
        assert!(!s.apply(CellVel(5)), "no drag, nothing to change");
        // bass: a new note carries the line's velocity, then its own
        assert!(s.apply(BassAdjust(BassParam::Velocity, -20)));
        assert!(s.apply(BassSet(0, 0)));
        assert_eq!(s.bass.steps[0].vel, 80);
        assert!(s.bass_has_note(0, 0) && !s.bass_has_note(1, 0) && !s.bass_has_note(0, 1));
        assert!(!s.apply(BassCellDown(0, 0)));
        assert!(s.apply(BassCellVel(30)));
        assert!(s.apply(BassCellDrag(3)));
        assert!(!s.apply(BassCellUp));
        assert_eq!((s.bass.steps[0].vel, s.bass.steps[0].len), (110, 4));
        assert!(s.bass_has_note(0, 2), "inside the note's tail, in its row");
        assert!(!s.bass_has_note(0, 4));
        // dragging inside the tail changes the head's velocity
        assert!(!s.apply(BassCellDown(0, 2)));
        assert!(s.apply(BassCellVel(-10)));
        assert!(!s.apply(BassCellUp));
        assert_eq!(s.bass.steps[0].vel, 100);
        assert!(!s.apply(BassCellVel(1)));
        // the velocity survives a save and load, and old files load
        let dir = std::env::temp_dir().join(format!("vel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        s.save(&path).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.bass.steps[0].vel, 100);
        assert_eq!(l.bass.vel, 80);
        let old = "current = 0\npatterns = []\n[bass]\nchannel = 0\nvel = 90\nbars = 1\nbase = 36\n[[bass.steps]]\non = true\nnote = 40\n";
        std::fs::write(&path, old).unwrap();
        let l = SeqState::load(&path).unwrap();
        assert_eq!(l.bass.steps[0], BassStep { on: true, note: 40, len: 1, vel: 100 });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn key_setup_and_generation() {
        use SeqAction::*;
        let mut s = SeqState::default();
        assert!(s.key.auto);
        assert!(!s.apply(KeyAuto(true)));
        assert!(s.apply(KeyRoot(9)), "setting a root switches to manual");
        assert!(!s.key.auto);
        assert_eq!(s.key.key, Key::new(9, Scale::Major));
        assert!(s.apply(KeyScale(Scale::Minor)));
        assert!(!s.apply(KeyScale(Scale::Minor)));
        assert!(s.apply(KeyAuto(true)));
        assert!(s.apply(KeyScale(Scale::Minor)), "back to manual, same key");
        assert_eq!(s.key.key.name(), "A MINOR");
        // saved with the patterns
        let dir = std::env::temp_dir().join(format!("key-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("patterns.toml");
        s.save(&path).unwrap();
        assert_eq!(SeqState::load(&path).unwrap().key, s.key);
        std::fs::remove_dir_all(&dir).unwrap();
        // a lead phrase: the tracks are the key's notes up from the tonic,
        // every note in the key, inside the phrase
        let key = Key::new(9, Scale::Minor);
        s.apply(Bars(2));
        let before = s.clone();
        assert!(s.apply(Generate(Instrument::Lead, key, 1)));
        let p = s.pattern();
        assert_eq!(p.tracks.iter().map(|t| t.note).collect::<Vec<_>>(), vec![69, 71, 72, 74, 76, 77, 79, 81]);
        let mut count = 0;
        for tr in &p.tracks {
            for (i, st) in tr.steps.iter().enumerate() {
                if st.on {
                    count += 1;
                    assert!(i < 32, "outside the 2-bar phrase");
                    assert_eq!(st.note, tr.note);
                    assert!(key.contains(st.note));
                    assert!((1..=127).contains(&st.vel));
                }
            }
        }
        assert!(count >= 4, "{count} notes");
        // the same seed gives the same phrase, another seed a different one
        let mut s2 = before.clone();
        s2.apply(Generate(Instrument::Lead, key, 1));
        assert_eq!(s2.patterns, s.patterns);
        let mut s2 = before.clone();
        s2.apply(Generate(Instrument::Lead, key, 2));
        assert_ne!(s2.patterns, s.patterns);
        // every press moves on to the next rhythm, each instrument on its
        // own: quarters and halves first, then groups of three
        assert_eq!((s.lead_rhythm, s.bass_rhythm), (Rhythm::Threes, Rhythm::Straight));
        let lens = |s: &SeqState| -> Vec<u16> { s.pattern().tracks.iter().flat_map(|t| t.steps.iter().filter(|st| st.on).map(|st| st.len)).collect() };
        assert!(lens(&s).iter().all(|l| l % 2 == 0), "{:?}", lens(&s));
        let mut s2 = s.clone();
        s2.apply(Generate(Instrument::Lead, key, 1));
        assert!(lens(&s2).iter().any(|&l| l == 3 || l == 6), "{:?}", lens(&s2));
        for _ in 0..3 {
            s2.apply(Generate(Instrument::Lead, key, 1));
        }
        assert_eq!(s2.lead_rhythm, Rhythm::Threes, "round and round");
        // a counterpoint bass line under it: in the key, one voice, the
        // window scrolled to the tonic, consonant with the lead where it sounds
        assert!(s.apply(Generate(Instrument::Bass, key, 3)));
        assert_eq!(s.bass.base, 33, "A0 at the bottom of the window");
        let lead = s.lead_line();
        let n = phrase_steps(s.bass.bars);
        let bass: Vec<_> = s.bass.steps.iter().take(n).enumerate().filter(|(_, st)| st.on).collect();
        assert!(bass.len() >= 4);
        assert!(key.is_tonic(bass[0].1.note) && bass[0].0 == 0);
        assert!(key.is_tonic(bass.last().unwrap().1.note));
        for (i, st) in &bass {
            assert!(key.contains(st.note) && (33..=45).contains(&st.note));
            assert!(st.vel >= 88 && st.vel <= 112, "velocity follows the beat: {}", st.vel);
            if let Some(o) = lead[i % lead.len()] {
                assert!(st.note < o);
                assert!(matches!((o as i32 - st.note as i32) % 12, 0 | 3 | 4 | 7 | 8 | 9), "step {i}: {} under {o}", st.note);
            }
        }
        // and a counterpoint lead over the bass, on the eight track notes
        assert!(s.apply(Generate(Instrument::Lead, key, 4)));
        let bass_line = s.bass_line();
        let p = s.pattern();
        let mut per_step = [0; 32];
        for tr in &p.tracks {
            for (i, st) in tr.steps.iter().enumerate().take(32) {
                if st.on {
                    per_step[i] += 1;
                    assert_eq!(st.note, tr.note);
                    if let Some(o) = bass_line[i % bass_line.len()] {
                        assert!(matches!((st.note as i32 - o as i32) % 12, 0 | 3 | 4 | 7 | 8 | 9), "step {i}: {} over {o}", st.note);
                    }
                }
            }
        }
        assert!(per_step.iter().all(|&c| c <= 1), "a composed line has one voice");
        assert_eq!(per_step[0], 1, "starts on the downbeat");
        // drums have no key
        assert!(!s.apply(Generate(Instrument::Drums, key, 1)));
        assert!(!Generate(Instrument::Lead, key, 1).view_only());
        assert!(!KeyRoot(1).view_only());
        // CLEAR empties a phrase and leaves the rest as it is
        let notes: Vec<u8> = s.pattern().tracks.iter().map(|t| t.note).collect();
        let base = s.bass.base;
        assert!(s.apply(ClearPhrase(Instrument::Lead)));
        assert!(s.pattern().tracks.iter().all(|t| t.steps.iter().all(|st| !st.on)));
        assert_eq!(s.pattern().tracks.iter().map(|t| t.note).collect::<Vec<_>>(), notes);
        assert_eq!(s.pattern().bars, 2);
        assert!(s.bass.steps.iter().any(|st| st.on), "the bass is another phrase");
        assert!(s.apply(ClearPhrase(Instrument::Bass)));
        assert!(s.bass.steps.iter().all(|st| !st.on));
        assert_eq!(s.bass.base, base);
        assert!(!s.apply(ClearPhrase(Instrument::Drums)), "the drums are cleared pad by pad");
        assert!(!ClearPhrase(Instrument::Lead).view_only());
    }

    #[test]
    fn playhead_and_step_positions() {
        let g = Grid { running: true, t0: 10.0, n0: 4, period: 0.5, downbeat: 0, bar_len: 4, ..Grid::default() };
        assert_eq!(playhead(&g, 10.0), Some(0));
        assert_eq!(playhead(&g, 10.13), Some(1));
        assert_eq!(playhead(&g, 11.9), Some(15));
        assert_eq!(playhead(&g, 12.0), Some(0));
        let g2 = Grid { downbeat: 1, ..g };
        assert_eq!(step_in_bar(&g2, 5, 0), 0);
        assert_eq!(step_in_bar(&g2, 4, 2), 14);
        assert_eq!(note_name(36), "C1");
        assert_eq!(note_name(61), "C#3");
        // bars count from the downbeat and wrap in the phrase
        assert_eq!(bar_and_step(&g2, 1, 0), (0, 0));
        assert_eq!(bar_and_step(&g2, 5, 0), (1, 0));
        assert_eq!(bar_and_step(&g2, 9, 1), (2, 1));
        assert_eq!(bar_and_step(&g2, 0, 0), (-1, 12));
        // n0 = 4 is beat 4 = the second bar after the downbeat
        assert_eq!(phrase_playhead(&g, 10.0, 2), Some((1, 0)));
        assert_eq!(phrase_playhead(&g, 12.0, 2), Some((0, 0)));
        assert_eq!(phrase_playhead(&g, 14.1, 2), Some((1, 0)));
        assert_eq!(phrase_playhead(&g, 14.1, 4), Some((3, 0)));
        // the nearest 16th for recording, and whether it is still to come
        let near = |t: f64| phrase_nearest(&g, t, 2).unwrap();
        assert_eq!(near(10.0), RecPos { bar: 1, step: 0, ahead: false });
        assert_eq!(near(10.10), RecPos { bar: 1, step: 1, ahead: true });
        assert_eq!(near(10.15), RecPos { bar: 1, step: 1, ahead: false });
        assert_eq!(near(9.95), RecPos { bar: 1, step: 0, ahead: true });
        assert_eq!(near(9.9), RecPos { bar: 0, step: 15, ahead: false });
        assert_eq!(phrase_nearest(&Grid::default(), 10.0, 2), None);
    }
}
