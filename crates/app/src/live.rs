//! Who drives the beat grid: the tracker (auto), the manual clock or the
//! external MIDI clock (MIDI IN). All three publish into `grid`, which the
//! click and MIDI clock read; a mutex around the mode keeps a mode switch
//! from racing with a publish. The lock is held only for one pointer store.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use tracker::Grid;
use ui::Mode;

pub struct Live {
    mode: Mutex<Mode>,
    mode_fast: AtomicU8,
    /// The grid the click and MIDI clock use.
    pub grid: ArcSwap<Grid>,
    /// What the tracker thinks, regardless of mode (shown as "mic hears").
    pub tracker_grid: ArcSwap<Grid>,
    /// What the MIDI input's clock says, regardless of mode.
    pub midi_in_grid: ArcSwap<Grid>,
    /// Tracker thread: take over the live grid on the next hop.
    pub adopt: AtomicBool,
    /// Tracker thread: the beat nearest this monotonic time (as f64 bits,
    /// 0 = nothing pending) is beat one. Set by the ONE button in auto mode.
    pub set_one: AtomicU64,
    /// The same for the MIDI input thread in MIDI IN mode.
    pub set_one_midi: AtomicU64,
}

impl Live {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode: Mutex::new(mode),
            mode_fast: AtomicU8::new(mode as u8),
            grid: ArcSwap::from_pointee(Grid::default()),
            tracker_grid: ArcSwap::from_pointee(Grid::default()),
            midi_in_grid: ArcSwap::from_pointee(Grid::default()),
            adopt: AtomicBool::new(false),
            set_one: AtomicU64::new(0),
            set_one_midi: AtomicU64::new(0),
        }
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode_fast.load(Ordering::Relaxed))
    }

    /// Called by the tracker thread whenever its grid changed.
    pub fn publish_tracker(&self, g: Grid) {
        self.tracker_grid.store(Arc::new(g));
        let m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if *m == Mode::Auto {
            self.grid.store(Arc::new(g));
        }
    }

    /// Called by the UI thread in manual mode.
    pub fn publish_manual(&self, g: Grid) {
        let m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if *m == Mode::Manual {
            self.grid.store(Arc::new(g));
        }
    }

    /// Called by the MIDI input thread whenever the external clock's grid changed.
    pub fn publish_midi_in(&self, g: Grid) {
        self.midi_in_grid.store(Arc::new(g));
        let m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if *m == Mode::MidiIn {
            self.grid.store(Arc::new(g));
        }
    }

    /// Switch mode. Returns the grid that was live at the moment of the switch,
    /// so the new owner can continue from it.
    pub fn set_mode(&self, mode: Mode) -> Grid {
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        *m = mode;
        self.mode_fast.store(mode as u8, Ordering::Relaxed);
        let g = **self.grid.load();
        match mode {
            Mode::Auto => self.adopt.store(true, Ordering::Relaxed),
            // The external clock is what it is: no adopting its predecessor.
            Mode::MidiIn => self.grid.store(Arc::new(**self.midi_in_grid.load())),
            Mode::Manual => {}
        }
        g
    }
}
