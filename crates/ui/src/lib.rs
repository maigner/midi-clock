//! Touch-screen front end for midi-clock. No window system: the screen is
//! drawn into an off-screen buffer and pushed to `/dev/fb0`; touches come from
//! evdev. One ordinary-priority thread, a redraw only when something visible
//! changed, so the real-time threads never notice it.

pub mod canvas;
pub mod fb;
pub mod screen;
pub mod touch;

use std::sync::atomic::{AtomicBool, Ordering};

use embedded_graphics::prelude::DrawTarget;

pub use canvas::{Canvas, PixelFormat, Rotation};
pub use screen::{effective_key, Button, Hit, Mode, Page, Screen, Status, View, KEY_MIN_CONFIDENCE};
pub use sequencer::{Instrument, SeqAction, Style};
pub use touch::{TouchConfig, TouchEvent};

#[derive(Clone, Debug)]
pub struct UiConfig {
    pub framebuffer: String,
    /// Empty = autodetect.
    pub touch_device: String,
    pub touch: TouchConfig,
    pub rotation: u32,
    pub fps: f64,
    /// Virtual console to switch to graphics mode; empty = leave alone.
    pub tty: String,
    /// Touch-panel latency subtracted from tap times (seconds).
    pub tap_offset_s: f64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            framebuffer: "/dev/fb0".into(),
            touch_device: String::new(),
            touch: TouchConfig::default(),
            rotation: 0,
            fps: 25.0,
            tty: "/dev/tty1".into(),
            tap_offset_s: 0.010,
        }
    }
}

/// What the screen asks the application to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    SetMode(Mode),
    /// Tap at this monotonic time (touch latency already subtracted).
    Tap(f64),
    /// Change the manual tempo by this many BPM.
    Nudge(f64),
    ToggleRun,
    /// Write a new LEAD or BASS phrase in the key in effect (the
    /// application knows it), at random or by the counterpoint rules,
    /// from this seed.
    Generate(Instrument, Style, u64),
    Seq(SeqAction),
}

/// One interactive session: touch handling, button repeat, and rendering
/// into a canvas whenever something visible changed. Shared by the
/// framebuffer front end and the desktop simulator.
pub struct Session {
    canvas: Canvas,
    screen: Screen,
    tap_offset_s: f64,
    frame: f64,
    pressed: Option<Button>,
    press_t: f64,
    last_repeat: f64,
    finger: Option<(i32, i32)>,
    last_view: Option<View>,
    next_frame: f64,
    page: Page,
    /// Finger scrolling the bass pitch window (note names or grid): y at
    /// which the last semitone was scrolled.
    drag_y: Option<i32>,
    /// Finger dragging a note's length on the sequence or bass grid.
    cell_drag: Option<CellDrag>,
    /// Finger down on a bass cell, direction not yet known: sideways it
    /// becomes a note drag, up or down a scroll, lifted in place a tap.
    pending: Option<PendingCell>,
    /// The live grid and the drum phrase length as of the last frame: a pad
    /// tap is placed on the nearest 16th for recording.
    grid: tracker::Grid,
    drum_bars: u8,
    /// The sequencer state as of the last frame: whether a bass cell holds
    /// a note decides what a vertical drag on it does.
    seq: Option<std::sync::Arc<sequencer::SeqState>>,
    /// The KEY page's mode and whether a key is known, as of the last
    /// frame: which buttons are on screen and live.
    key_auto: bool,
    key_known: bool,
    /// The big number: the live tempo smoothed over `BPM_SMOOTH_S`, and
    /// the value printed, which only follows it outside a small deadband.
    bpm_smooth: f64,
    bpm_printed: f64,
    bpm_at: f64,
}

/// Time constant of the smoothing of the big tempo number.
const BPM_SMOOTH_S: f64 = 2.0;
/// The printed tempo only moves when the smoothed one is this far away.
const BPM_DEADBAND: f64 = 0.25;
/// A change bigger than this (relock, manual +/-) is shown at once.
const BPM_JUMP: f64 = 4.0;

#[derive(Clone, Copy)]
struct PendingCell {
    row: usize,
    step: usize,
    x: i32,
    y: i32,
}

#[derive(Clone, Copy)]
struct CellDrag {
    bass: bool,
    /// Column last reported to the sequencer.
    col: usize,
    /// +1/-1 while the finger is at the right/left edge of the grid, and
    /// since when: held there, the drag moves on to the next/previous bar.
    edge: i32,
    edge_since: f64,
    /// y at which the velocity was last changed: moving up raises it.
    vel_y: i32,
}

/// How long the finger stays at the edge of the grid before the drag moves
/// on to the next bar (and again for every further bar).
const EDGE_HOLD_S: f64 = 0.6;

impl Session {
    pub fn new(cfg: &UiConfig, canvas: Canvas) -> Self {
        let screen = Screen::new(canvas.width(), canvas.height());
        Self {
            canvas,
            screen,
            tap_offset_s: cfg.tap_offset_s,
            frame: 1.0 / cfg.fps.clamp(1.0, 60.0),
            pressed: None,
            press_t: 0.0,
            last_repeat: 0.0,
            finger: None,
            last_view: None,
            next_frame: 0.0,
            page: Page::Tempo,
            drag_y: None,
            cell_drag: None,
            pending: None,
            grid: tracker::Grid::default(),
            drum_bars: 1,
            seq: None,
            key_auto: true,
            key_known: false,
            bpm_smooth: 0.0,
            bpm_printed: 0.0,
            bpm_at: 0.0,
        }
    }

    /// The tempo to print for `status`: the live grid's, smoothed while the
    /// tracker is running so that its per-onset corrections do not make
    /// the last digit flicker. Manual tempo and jumps are shown exactly.
    fn shown_bpm(&mut self, now: f64, status: &Status) -> f64 {
        let target = status.live.bpm();
        let dt = (now - self.bpm_at).clamp(0.0, 1.0);
        self.bpm_at = now;
        let smooth = status.mode == Mode::Auto && status.live.running && (target - self.bpm_smooth).abs() < BPM_JUMP;
        if smooth {
            let a = 1.0 - (-dt / BPM_SMOOTH_S).exp();
            self.bpm_smooth += a * (target - self.bpm_smooth);
            if (self.bpm_smooth - self.bpm_printed).abs() >= BPM_DEADBAND {
                self.bpm_printed = self.bpm_smooth;
            }
        } else {
            self.bpm_smooth = target;
            self.bpm_printed = target;
        }
        self.bpm_printed
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn set_page(&mut self, page: Page) {
        self.page = page;
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn canvas_mut(&mut self) -> &mut Canvas {
        &mut self.canvas
    }

    /// Feed a touch event (physical coordinates). `mode` decides which
    /// buttons are live.
    pub fn touch(&mut self, ev: TouchEvent, mode: Mode, act: &mut impl FnMut(Action)) {
        match ev {
            TouchEvent::Down { x, y, t } => {
                let (lx, ly) = self.canvas.touch_to_logical(x, y);
                self.finger = Some((lx, ly));
                self.pressed = None;
                self.drag_y = None;
                self.cell_drag = None;
                self.pending = None;
                self.press_t = t;
                self.last_repeat = t;
                let hit = self.screen.hit(lx, ly, self.page).filter(|h| !matches!(h, Hit::Button(b) if !b.visible(mode, self.key_auto)));
                match hit {
                    Some(Hit::Button(Button::PageTempo)) => self.page = Page::Tempo,
                    Some(Hit::Button(Button::PageKey)) => self.page = Page::Key,
                    Some(Hit::Button(Button::PageLead)) => self.page = Page::Lead,
                    Some(Hit::Button(Button::PageBass)) => self.page = Page::Bass,
                    Some(Hit::Button(Button::PageDrums)) => self.page = Page::Drums,
                    Some(Hit::Button(Button::PageSettings)) => self.page = Page::Settings,
                    // A pad tap carries where the clock is, so REC can put
                    // the hit on the nearest 16th.
                    Some(Hit::Button(Button::DrumPad(i))) => {
                        self.pressed = Some(Button::DrumPad(i));
                        let pos = sequencer::phrase_nearest(&self.grid, t - self.tap_offset_s, self.drum_bars as usize);
                        act(Action::Seq(SeqAction::DrumPad(i, pos)));
                    }
                    Some(Hit::Button(b)) => {
                        self.pressed = Some(b);
                        if b.enabled(mode, self.key_auto, self.key_known) {
                            dispatch(b, t - self.tap_offset_s, act);
                        }
                    }
                    Some(Hit::Cell(track, step)) => {
                        act(Action::Seq(SeqAction::CellDown(track, step)));
                        self.cell_drag = Some(CellDrag { bass: false, col: step, edge: 0, edge_since: t, vel_y: ly });
                    }
                    Some(Hit::Track(track)) => act(Action::Seq(SeqAction::SelectTrack(track))),
                    // The bass grid scrolls with an up/down drag, so the
                    // note waits until the finger shows what it is doing.
                    Some(Hit::BassCell(row, step)) => self.pending = Some(PendingCell { row, step, x: lx, y: ly }),
                    Some(Hit::BassLabels) => self.drag_y = Some(ly),
                    None => {}
                }
            }
            TouchEvent::Move { x, y, t } => {
                let (lx, ly) = self.canvas.touch_to_logical(x, y);
                self.finger = Some((lx, ly));
                if let Some(b) = self.pressed {
                    if self.screen.hit(lx, ly, self.page) != Some(Hit::Button(b)) {
                        self.pressed = None;
                    }
                }
                if let Some(pc) = self.pending {
                    let (dx, dy) = ((lx - pc.x).abs(), (ly - pc.y).abs());
                    if dx >= self.screen.bass_col_pitch() / 2 && dx >= dy {
                        self.pending = None;
                        act(Action::Seq(SeqAction::BassCellDown(pc.row, pc.step)));
                        self.cell_drag = Some(CellDrag { bass: true, col: pc.step, edge: 0, edge_since: t, vel_y: pc.y });
                    } else if dy >= self.screen.bass_row_pitch() / 2 {
                        self.pending = None;
                        // up or down on a note: its velocity; elsewhere: scroll
                        if self.seq.as_ref().is_some_and(|q| q.bass_has_note(pc.row, pc.step)) {
                            act(Action::Seq(SeqAction::BassCellDown(pc.row, pc.step)));
                            self.cell_drag = Some(CellDrag { bass: true, col: pc.step, edge: 0, edge_since: t, vel_y: pc.y });
                        } else {
                            self.drag_y = Some(pc.y);
                        }
                    }
                }
                // Dragging the note names or the grid up and down scrolls the
                // pitch window with the finger: one row of travel is one semitone.
                if let Some(y0) = self.drag_y {
                    let pitch = self.screen.bass_row_pitch().max(1);
                    let rows = (ly - y0) / pitch;
                    if rows != 0 {
                        act(Action::Seq(SeqAction::BassScroll(rows)));
                        self.drag_y = Some(y0 + rows * pitch);
                    }
                }
                // Dragging along a grid row sets the note's length: the note
                // ends on the column under the finger. Up and down sets its
                // velocity.
                if let Some(cd) = &mut self.cell_drag {
                    let col = self.screen.column_at(lx, cd.bass);
                    if col != cd.col {
                        cd.col = col;
                        act(Action::Seq(if cd.bass { SeqAction::BassCellDrag(col) } else { SeqAction::CellDrag(col) }));
                    }
                    let edge = self.screen.grid_edge(lx, cd.bass);
                    if edge != cd.edge {
                        cd.edge = edge;
                        cd.edge_since = t;
                    }
                    let px = self.screen.vel_pixels().max(1);
                    let dv = (cd.vel_y - ly) / px;
                    if dv != 0 {
                        cd.vel_y -= dv * px;
                        act(Action::Seq(if cd.bass { SeqAction::BassCellVel(dv) } else { SeqAction::CellVel(dv) }));
                    }
                }
            }
            TouchEvent::Up { .. } => {
                self.pressed = None;
                self.finger = None;
                self.drag_y = None;
                if let Some(pc) = self.pending.take() {
                    act(Action::Seq(SeqAction::BassSet(pc.row, pc.step)));
                }
                if let Some(cd) = self.cell_drag.take() {
                    act(Action::Seq(if cd.bass { SeqAction::BassCellUp } else { SeqAction::CellUp }));
                }
            }
        }
    }

    /// Forget any finger (touch device went away).
    pub fn release(&mut self) {
        self.pressed = None;
        self.finger = None;
        self.drag_y = None;
        self.cell_drag = None;
        self.pending = None;
    }

    /// Auto-repeat for held +/- buttons, and a note drag held at the edge
    /// of the grid moving on to the next bar. Call often.
    pub fn tick(&mut self, now: f64, act: &mut impl FnMut(Action)) {
        if let Some(b) = self.pressed {
            if b.repeats() && now - self.press_t > 0.45 && now - self.last_repeat > 0.12 {
                self.last_repeat = now;
                dispatch(b, now, act);
            }
        }
        if let Some(cd) = &mut self.cell_drag {
            if cd.edge != 0 && now - cd.edge_since > EDGE_HOLD_S {
                cd.edge_since = now;
                cd.col = if cd.edge > 0 { sequencer::STEPS - 1 } else { 0 };
                act(Action::Seq(if cd.bass { SeqAction::BassCellDragBar(cd.edge) } else { SeqAction::CellDragBar(cd.edge) }));
            }
        }
    }

    /// Whether a new frame is due at `now`.
    pub fn frame_due(&self, now: f64) -> bool {
        now >= self.next_frame
    }

    /// Render `status` if it differs from what is on screen. Returns true
    /// when the canvas was redrawn and should be presented. While a
    /// sequencer page follows the clock, `act` is told which bar is on
    /// screen so that edits land in it.
    pub fn frame(&mut self, now: f64, status: &Status, act: &mut impl FnMut(Action)) -> bool {
        self.next_frame = now + self.frame;
        self.grid = status.live;
        self.drum_bars = status.seq.drums.bars;
        self.seq = Some(status.seq.clone());
        self.key_auto = status.seq.key.auto;
        self.key_known = effective_key(&status.seq.key, status.key.as_ref()).is_some();
        let bpm_shown = self.shown_bpm(now, status);
        let status = &Status { bpm_shown: Some(bpm_shown), ..status.clone() };
        let view = View::build_with_touch(status, self.page, self.pressed, self.finger);
        if let Some((bar, _)) = view.seq_bars() {
            if !status.seq.bar_lock && bar != status.seq.shown_bar() {
                act(Action::Seq(SeqAction::FollowBar(bar)));
            }
        }
        if let Some((bar, _)) = view.bass_bars() {
            if !status.seq.bass_bar_lock && bar != status.seq.bass_bar {
                act(Action::Seq(SeqAction::BassFollowBar(bar)));
            }
        }
        if let Some((bar, _)) = view.drum_bars() {
            if !status.seq.drum_bar_lock && bar != status.seq.drum_bar {
                act(Action::Seq(SeqAction::DrumFollowBar(bar)));
            }
        }
        if self.last_view.as_ref() == Some(&view) {
            return false;
        }
        self.screen.render(&mut self.canvas, &view);
        self.last_view = Some(view);
        true
    }
}

/// Run the UI on the framebuffer until `stop` is set. `status` is polled
/// before every frame, `act` is called for every button press. Returns an
/// error only when the framebuffer cannot be used at all.
pub fn run(
    cfg: &UiConfig,
    stop: &AtomicBool,
    mut status: impl FnMut() -> Status,
    mut act: impl FnMut(Action),
) -> Result<(), String> {
    let mut fbdev = fb::Framebuffer::open(&cfg.framebuffer).map_err(|e| format!("{}: {e}", cfg.framebuffer))?;
    eprintln!("ui: framebuffer {}", fbdev.describe());
    if !cfg.tty.is_empty() {
        if let Err(e) = fbdev.take_console(&cfg.tty) {
            eprintln!("ui: cannot switch {} to graphics mode ({e}); the text console may draw over the screen", cfg.tty);
        }
    }
    let rotation = Rotation::from_degrees(cfg.rotation).ok_or_else(|| format!("rotation {} not one of 0/90/180/270", cfg.rotation))?;
    let mut session = Session::new(cfg, fbdev.canvas(rotation));
    let open_touch = |verbose: bool| match touch::Touch::open(&cfg.touch_device, cfg.touch.clone(), fbdev.width, fbdev.height) {
        Ok(t) => {
            eprintln!("ui: touch {} ({})", t.path, t.name);
            Some(t)
        }
        Err(e) => {
            if verbose {
                eprintln!("ui: no touch input ({e}); display only, will keep looking");
            }
            None
        }
    };
    let mut touch = open_touch(true);
    let mut next_touch_probe = rtio::now() + 2.0;
    let mut events = Vec::with_capacity(64);

    while !stop.load(Ordering::Relaxed) {
        // --- input ------------------------------------------------------------
        if touch.is_none() && rtio::now() >= next_touch_probe {
            // Hot-plug: a USB touch panel may enumerate after we started.
            next_touch_probe = rtio::now() + 2.0;
            touch = open_touch(false);
        }
        if let Some(t) = touch.as_mut() {
            events.clear();
            if let Err(e) = t.poll(&mut events) {
                // Unplugged: drop it and go back to probing.
                eprintln!("ui: touch read failed ({e}); waiting for the panel to come back");
                touch = None;
                session.release();
                next_touch_probe = rtio::now() + 2.0;
                continue;
            }
            if !events.is_empty() {
                let mode = status().mode;
                for ev in events.drain(..) {
                    session.touch(ev, mode, &mut act);
                }
            }
            session.tick(rtio::now(), &mut act);
        }
        // --- output -------------------------------------------------------------
        let now = rtio::now();
        if session.frame_due(now) {
            let s = status();
            if session.frame(now, &s, &mut act) {
                if let Err(e) = fbdev.present(session.canvas()) {
                    return Err(format!("framebuffer write failed: {e}"));
                }
            }
        }
        rtio::sleep_until(rtio::now() + 0.008);
    }
    // Leave a blank screen rather than a frozen one.
    let _ = session.canvas_mut().clear(embedded_graphics::pixelcolor::Rgb888::new(0, 0, 0));
    let _ = fbdev.present(session.canvas());
    Ok(())
}

fn dispatch(b: Button, t: f64, act: &mut impl FnMut(Action)) {
    let a = match b {
        Button::PageTempo | Button::PageKey | Button::PageLead | Button::PageBass | Button::PageDrums | Button::PageSettings => return,
        Button::ModeAuto => Action::SetMode(Mode::Auto),
        Button::ModeManual => Action::SetMode(Mode::Manual),
        Button::Minus5 => Action::Nudge(-5.0),
        Button::Minus1 => Action::Nudge(-1.0),
        Button::Tap => Action::Tap(t),
        Button::Plus1 => Action::Nudge(1.0),
        Button::Plus5 => Action::Nudge(5.0),
        Button::StartStop => Action::ToggleRun,
        Button::KeyAuto => Action::Seq(SeqAction::KeyAuto(true)),
        Button::KeyManual => Action::Seq(SeqAction::KeyAuto(false)),
        Button::KeyRoot(pc) => Action::Seq(SeqAction::KeyRoot(pc)),
        Button::KeyScale(sc) => Action::Seq(SeqAction::KeyScale(sc)),
        // the press time is the seed: every press writes a different phrase
        Button::Generate(inst, style) => Action::Generate(inst, style, t.to_bits()),
        Button::ParamMinus(p) => Action::Seq(SeqAction::Adjust(p, -1)),
        Button::ParamPlus(p) => Action::Seq(SeqAction::Adjust(p, 1)),
        Button::Bars(n) => Action::Seq(SeqAction::Bars(n)),
        Button::Bar(i) => Action::Seq(SeqAction::Bar(i)),
        Button::BarLock => Action::Seq(SeqAction::ToggleBarLock),
        Button::BassBars(n) => Action::Seq(SeqAction::BassBars(n)),
        Button::BassBar(i) => Action::Seq(SeqAction::BassBar(i)),
        Button::BassBarLock => Action::Seq(SeqAction::ToggleBassBarLock),
        Button::BassMinus(p) => Action::Seq(SeqAction::BassAdjust(p, -1)),
        Button::BassPlus(p) => Action::Seq(SeqAction::BassAdjust(p, 1)),
        Button::BassScroll(d) => Action::Seq(SeqAction::BassScroll(d)),
        Button::DrumMinus(p) => Action::Seq(SeqAction::DrumAdjust(p, -1)),
        Button::DrumPlus(p) => Action::Seq(SeqAction::DrumAdjust(p, 1)),
        Button::DrumBars(n) => Action::Seq(SeqAction::DrumBars(n)),
        Button::DrumBar(i) => Action::Seq(SeqAction::DrumBar(i)),
        Button::DrumBarLock => Action::Seq(SeqAction::ToggleDrumBarLock),
        // pads are handled in `touch` (they need the clock position)
        Button::DrumPad(i) => Action::Seq(SeqAction::DrumPad(i, None)),
        Button::DrumStep(k) => Action::Seq(SeqAction::DrumStep(k)),
        Button::ChannelMinus(i) => Action::Seq(SeqAction::Channel(i, -1)),
        Button::ChannelPlus(i) => Action::Seq(SeqAction::Channel(i, 1)),
    };
    act(a);
}

/// Render one frame off-screen with the given status (for `--ui-snapshot`).
pub fn snapshot(w: usize, h: usize, s: &Status, page: Page, pressed: Option<Button>) -> Vec<u8> {
    let screen = Screen::new(w, h);
    let mut c = Canvas::new(w, h, w * 4, PixelFormat::XRGB8888, Rotation::R0);
    screen.render(&mut c, &View::build_with_touch(s, page, pressed, None));
    c.to_ppm()
}
