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
pub use screen::{Button, Hit, Mode, Page, Screen, Status, View};
pub use sequencer::SeqAction;
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
    Relock,
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
    /// Finger dragging the bass note names: y at which the last semitone was
    /// scrolled.
    drag_y: Option<i32>,
}

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
        }
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
                self.press_t = t;
                self.last_repeat = t;
                match self.screen.hit(lx, ly, self.page) {
                    Some(Hit::Button(Button::PageTempo)) => self.page = Page::Tempo,
                    Some(Hit::Button(Button::PageSequence)) => self.page = Page::Sequence,
                    Some(Hit::Button(Button::PageBass)) => self.page = Page::Bass,
                    Some(Hit::Button(b)) => {
                        self.pressed = Some(b);
                        if b.enabled(mode) {
                            dispatch(b, t - self.tap_offset_s, act);
                        }
                    }
                    Some(Hit::Cell(track, step)) => act(Action::Seq(SeqAction::ToggleStep(track, step))),
                    Some(Hit::Track(track)) => act(Action::Seq(SeqAction::SelectTrack(track))),
                    Some(Hit::BassCell(row, step)) => act(Action::Seq(SeqAction::BassSet(row, step))),
                    Some(Hit::BassLabels) => self.drag_y = Some(ly),
                    None => {}
                }
            }
            TouchEvent::Move { x, y, .. } => {
                let (lx, ly) = self.canvas.touch_to_logical(x, y);
                self.finger = Some((lx, ly));
                if let Some(b) = self.pressed {
                    if self.screen.hit(lx, ly, self.page) != Some(Hit::Button(b)) {
                        self.pressed = None;
                    }
                }
                // Dragging the note names scrolls the pitch window with the
                // finger: one row of travel is one semitone.
                if let Some(y0) = self.drag_y {
                    let pitch = self.screen.bass_row_pitch().max(1);
                    let rows = (ly - y0) / pitch;
                    if rows != 0 {
                        act(Action::Seq(SeqAction::BassScroll(rows)));
                        self.drag_y = Some(y0 + rows * pitch);
                    }
                }
            }
            TouchEvent::Up { .. } => {
                self.pressed = None;
                self.finger = None;
                self.drag_y = None;
            }
        }
    }

    /// Forget any finger (touch device went away).
    pub fn release(&mut self) {
        self.pressed = None;
        self.finger = None;
        self.drag_y = None;
    }

    /// Auto-repeat for held +/- buttons. Call often.
    pub fn tick(&mut self, now: f64, act: &mut impl FnMut(Action)) {
        if let Some(b) = self.pressed {
            if b.repeats() && now - self.press_t > 0.45 && now - self.last_repeat > 0.12 {
                self.last_repeat = now;
                dispatch(b, now, act);
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
        Button::PageTempo | Button::PageSequence | Button::PageBass => return,
        Button::ModeAuto => Action::SetMode(Mode::Auto),
        Button::ModeManual => Action::SetMode(Mode::Manual),
        Button::Minus5 => Action::Nudge(-5.0),
        Button::Minus1 => Action::Nudge(-1.0),
        Button::Tap => Action::Tap(t),
        Button::Plus1 => Action::Nudge(1.0),
        Button::Plus5 => Action::Nudge(5.0),
        Button::StartStop => Action::ToggleRun,
        Button::Relock => Action::Relock,
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
