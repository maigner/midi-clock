//! A pattern sequencer that runs from the beat grid. Steps are 16ths: step
//! `k` of the bar sounds at beat `downbeat + k / 4`, tick `(k % 4) * 6`. Being
//! placed on the grid, the notes are on the beat in both auto and manual mode
//! and follow every correction, like the click and the clock.
//!
//! Both the 8-track drum pattern and the single-voice bass line (`Bass`) are
//! phrases of 1, 2, 4 or 8 bars, played by the same thread. Bars are counted
//! from the downbeat of the grid, so a phrase always starts on a "1".
//!
//! `SeqState` is the editable model (UI thread), `Snapshot` what the
//! real-time thread plays, `run` the thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtio::{now, sleep_until, MidiOut};
use serde::{Deserialize, Serialize};
use tracker::Grid;

pub const STEPS: usize = 16;
pub const TRACKS: usize = 8;
pub const PATTERNS: usize = 4;
/// Longest phrase, in bars (drums and bass).
pub const MAX_BARS: usize = 8;
/// Steps a track or the bass line holds.
pub const MAX_STEPS: usize = MAX_BARS * STEPS;
/// Pitch rows the bass page shows at once (one octave).
pub const BASS_ROWS: usize = 12;
/// Highest `Bass::base`: the top row is then MIDI note 127.
pub const BASS_TOP: u8 = 127 - (BASS_ROWS as u8 - 1);

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
    /// MIDI channel, 0-based (shown 1-based).
    pub channel: u8,
    /// Note new steps get.
    pub note: u8,
    pub vel: u8,
    pub muted: bool,
    /// `MAX_STEPS` steps; only the pattern's `bars` bars play.
    pub steps: Vec<Step>,
}

impl Track {
    pub fn new(channel: u8, note: u8) -> Self {
        Self { channel, note, vel: 100, muted: false, steps: vec![Step::default(); MAX_STEPS] }
    }
}

fn one_bar() -> u8 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    /// Phrase length in bars: 1, 2, 4 or 8.
    #[serde(default = "one_bar")]
    pub bars: u8,
    pub tracks: Vec<Track>,
}

impl Default for Pattern {
    fn default() -> Self {
        // General MIDI drums on channel 10: kick, snare, closed hat, open hat, clap, low tom, high tom, crash.
        let notes = [36u8, 38, 42, 46, 39, 45, 50, 49];
        Self { bars: 1, tracks: notes.iter().map(|&n| Track::new(9, n)).collect() }
    }
}

impl Pattern {
    fn normalise(&mut self) {
        if !matches!(self.bars, 1 | 2 | 4 | 8) {
            self.bars = 1;
        }
        self.tracks.resize_with(TRACKS, || Track::new(9, 60));
        for t in &mut self.tracks {
            t.steps.resize(MAX_STEPS, Step::default());
            for st in &mut t.steps {
                st.len = st.len.clamp(1, MAX_STEPS as u16);
            }
            t.channel &= 0x0F;
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
}

impl Default for BassStep {
    fn default() -> Self {
        Self { on: false, note: 36, len: 1 }
    }
}

/// The single-voice bass line: up to 8 bars of 16ths, one note per step,
/// each with a length. A new note always ends the previous one (monophonic).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bass {
    /// MIDI channel, 0-based.
    pub channel: u8,
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
        }
        self.channel &= 0x0F;
        self.vel = self.vel.clamp(1, 127);
        if !matches!(self.bars, 1 | 2 | 4 | 8) {
            self.bars = 1;
        }
        self.base = self.base.min(BASS_TOP);
    }
}

/// What the playing thread needs; replaced wholesale on every edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub pattern: Pattern,
    pub playing: bool,
    pub bass: Bass,
}

/// Which track parameter a +/- press changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    Channel,
    Note,
    Velocity,
}

/// Which bass parameter a +/- press changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BassParam {
    Channel,
    Velocity,
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
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct File {
    patterns: Vec<Pattern>,
    current: usize,
    #[serde(default)]
    bass: Bass,
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

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { pattern: self.patterns[self.current].clone(), playing: self.playing, bass: self.bass.clone() }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let f: File = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s = SeqState { patterns: f.patterns, current: f.current, bass: f.bass, ..SeqState::default() };
        s.bass.normalise();
        s.patterns.resize_with(PATTERNS, Pattern::default);
        s.patterns.truncate(PATTERNS);
        for p in &mut s.patterns {
            p.normalise();
        }
        s.current = s.current.min(PATTERNS - 1);
        Ok(s)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let f = File { patterns: self.patterns.clone(), current: self.current, bass: self.bass.clone() };
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
                    (Param::Channel, _) => tr.channel = bump(tr.channel, 0, 15),
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
                        b.steps[idx] = BassStep { on: true, note, len: 1 };
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
                    b.steps[d.cell] = BassStep { on: true, note: d.note, len: 1 };
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
                    BassParam::Channel => b.channel = (b.channel as i32 + d).clamp(0, 15) as u8,
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
        }
    }
}

/// Note name like "C1" (MIDI 36). Octave numbering with middle C = C3.
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

pub fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[(n % 12) as usize], (n / 12) as i32 - 2)
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

#[derive(Clone, Copy)]
struct Pending {
    /// Beat and 16th of the note-off, on the grid of `epoch`; `t_off` is
    /// recomputed from the current grid while the epoch matches, so a long
    /// note follows tempo corrections.
    n: i64,
    sub: i64,
    epoch: u32,
    t_off: f64,
    channel: u8,
    note: u8,
    /// True for the bass voice, which is monophonic.
    bass: bool,
}

/// The sequencer thread body. Runs until `stop` is set.
pub fn run<G: FnMut() -> Grid, S: FnMut() -> std::sync::Arc<Snapshot>>(
    mut get_grid: G,
    mut get_snapshot: S,
    midi: &dyn NoteOut,
    lead_s: f64,
    stop: &AtomicBool,
    stats: &SeqStats,
) {
    let poll = 0.0015;
    let mut pending: Vec<Pending> = Vec::with_capacity(TRACKS * 8 + 1);
    let mut last_q: Option<i64> = None;
    let mut epoch = u32::MAX;
    let all_off = |pending: &mut Vec<Pending>| {
        for p in pending.drain(..) {
            if !midi.note_off(p.channel, p.note) {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            all_off(&mut pending);
            return;
        }
        let g = get_grid();
        let snap = get_snapshot();
        let tnow = now();
        if g.running {
            for p in &mut pending {
                if p.epoch == g.epoch {
                    p.t_off = g.tick_time(p.n, (p.sub * 6) as u32) - lead_s;
                }
            }
        }
        // Note-offs are due regardless of the clock state.
        let next_off = pending.iter().map(|p| p.t_off).fold(f64::INFINITY, f64::min);
        if !g.running || !snap.playing {
            if !pending.is_empty() && next_off <= tnow {
                pending.retain(|p| {
                    if p.t_off <= tnow {
                        if !midi.note_off(p.channel, p.note) {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            last_q = None;
            sleep_until(tnow + 0.002);
            continue;
        }
        if g.epoch != epoch {
            epoch = g.epoch;
            last_q = None;
        }
        // Next 16th at or after now + lead, in 16ths relative to the grid's n0.
        let step_len = g.period / 4.0;
        let mut q = ((tnow + lead_s - g.t0) / step_len).ceil() as i64;
        if let Some(lq) = last_q {
            if q <= lq {
                q = lq + 1;
            }
            if q > lq + 2 * STEPS as i64 {
                // The grid jumped far ahead; do not burst.
                last_q = None;
            }
        }
        let n = g.n0 + q.div_euclid(4);
        let sub = q.rem_euclid(4);
        let t_on = g.tick_time(n, (sub * 6) as u32) - lead_s;
        let next = t_on.min(next_off);
        if next - tnow > poll {
            sleep_until(tnow + poll);
            continue;
        }
        sleep_until(next);
        let t = now();
        // Note-offs first, so a retriggered note gets a clean off/on.
        pending.retain(|p| {
            if p.t_off <= t + 0.0002 {
                if !midi.note_off(p.channel, p.note) {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                }
                false
            } else {
                true
            }
        });
        if t_on <= t + 0.0002 {
            let (bar, k) = bar_and_step(&g, n, sub);
            // Note-off `len` steps on: beat, 16th and time on this grid.
            let off = |len: u16| {
                let q_off = q + len.max(1) as i64;
                let (n_off, sub_off) = (g.n0 + q_off.div_euclid(4), q_off.rem_euclid(4));
                (n_off, sub_off, g.tick_time(n_off, (sub_off * 6) as u32) - lead_s)
            };
            let pidx = bar.rem_euclid(snap.pattern.bars.max(1) as i64) as usize * STEPS + k;
            for tr in &snap.pattern.tracks {
                if tr.muted {
                    continue;
                }
                let st = tr.steps.get(pidx).copied().unwrap_or_default();
                if !st.on {
                    continue;
                }
                // Kill the same note if it is still sounding (a repeat inside a long note).
                if let Some(i) = pending.iter().position(|p| p.channel == tr.channel && p.note == st.note) {
                    let p = pending.swap_remove(i);
                    let _ = midi.note_off(p.channel, p.note);
                }
                if !midi.note_on(tr.channel, st.note, st.vel) {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.notes.fetch_add(1, Ordering::Relaxed);
                }
                let (n_off, sub_off, t_off) = off(st.len);
                if pending.len() < pending.capacity() {
                    pending.push(Pending { n: n_off, sub: sub_off, epoch: g.epoch, t_off, channel: tr.channel, note: st.note, bass: false });
                } else {
                    let _ = midi.note_off(tr.channel, st.note);
                }
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
                if !midi.note_on(b.channel, st.note, b.vel) {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.notes.fetch_add(1, Ordering::Relaxed);
                }
                let (n_off, sub_off, t_off) = off(st.len);
                if pending.len() < pending.capacity() {
                    pending.push(Pending { n: n_off, sub: sub_off, epoch: g.epoch, t_off, channel: b.channel, note: st.note, bass: true });
                } else {
                    let _ = midi.note_off(b.channel, st.note);
                }
            }
            last_q = Some(q);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        s.apply(SeqAction::Adjust(Param::Channel, -20));
        assert_eq!(s.pattern().tracks[0].channel, 0);
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

    struct Recorder(std::sync::Mutex<Vec<(f64, bool, u8, u8)>>);
    impl NoteOut for Recorder {
        fn note_on(&self, channel: u8, note: u8, _vel: u8) -> bool {
            self.0.lock().unwrap().push((now(), true, channel, note));
            true
        }
        fn note_off(&self, channel: u8, note: u8) -> bool {
            self.0.lock().unwrap().push((now(), false, channel, note));
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
        assert_eq!(s.bass.steps[20], BassStep { on: true, note: 39, len: 3 });
        assert!(s.apply(SeqAction::BassSet(5, 4)));
        assert_eq!(s.bass.steps[20], BassStep { on: true, note: 41, len: 3 });
        // a tap inside it, in its own row, cuts it and starts a new note there
        assert!(s.apply(SeqAction::BassSet(5, 6)));
        assert_eq!(s.bass.steps[20].len, 2);
        assert_eq!(s.bass.steps[22], BassStep { on: true, note: 41, len: 1 });
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
    }
}
