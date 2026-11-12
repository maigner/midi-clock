//! The beat grid: the contract between the tracker and everything that emits
//! on the beat.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing heard yet.
    Idle,
    /// Onsets are arriving, tempo not yet established.
    Locking,
    /// Clock running, onsets confirm the grid.
    Running,
    /// Clock running, but no onsets have matched for a while.
    Coasting,
    /// Clock stopped after prolonged silence.
    Stopped,
}

/// A transport message of an external clock master.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Start,
    Continue,
    Stop,
}

/// What an external clock master (MIDI IN) said about its transport. A grid
/// that carries one hands the master's Start, Continue and Stop on to the
/// MIDI output as they came; `running` then only says whether ticks go out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transport {
    /// Counts the master's Start, Continue and Stop messages.
    pub seq: u32,
    /// The latest of them, `None` until the first.
    pub last: Option<Command>,
    /// The tick (24 per beat, tick 0 opens beat 0) the master's clock began
    /// or resumed with: 0 after a Start, where it stopped after a Continue.
    pub first_tick: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// Incremented every time the clock is (re)started. Consumers reset their
    /// beat counters when it changes.
    pub epoch: u32,
    /// Whether MIDI clock should be running.
    pub running: bool,
    pub state: State,
    /// Monotonic time of beat `n0`.
    pub t0: f64,
    /// Index of the reference beat. Beat 0 is the beat on which Start was sent.
    pub n0: i64,
    /// Beat period in seconds.
    pub period: f64,
    /// 0..1, how well recent onsets agree with the grid.
    pub confidence: f32,
    pub bar_len: u32,
    /// Beats `n` with `n % bar_len == downbeat` are beat one of a bar.
    pub downbeat: u32,
    /// Whether `downbeat` is established rather than assumed: set by a tap
    /// count-in, or by the tracker once the kick pattern names one beat
    /// decisively for a whole bar and fits `bar_len` better than another
    /// meter. The accent click needs it.
    pub downbeat_known: bool,
    /// Set while an external master with a transport of its own is in
    /// charge. `None`: Start/Stop on the MIDI output follow `running`.
    pub transport: Option<Transport>,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            epoch: 0,
            running: false,
            state: State::Idle,
            t0: 0.0,
            n0: 0,
            period: 0.5,
            confidence: 0.0,
            bar_len: 4,
            downbeat: 0,
            downbeat_known: false,
            transport: None,
        }
    }
}

impl Grid {
    #[inline]
    pub fn beat_time(&self, n: i64) -> f64 {
        self.t0 + (n - self.n0) as f64 * self.period
    }

    #[inline]
    pub fn bpm(&self) -> f64 {
        60.0 / self.period
    }

    /// Nearest beat index to `t` and the signed error `t - beat_time`.
    #[inline]
    pub fn nearest_beat(&self, t: f64) -> (i64, f64) {
        let x = (t - self.t0) / self.period;
        let n = x.round() as i64 + self.n0;
        (n, t - self.beat_time(n))
    }

    /// First beat index whose time is >= `t`.
    #[inline]
    pub fn first_beat_at_or_after(&self, t: f64) -> i64 {
        ((t - self.t0) / self.period).ceil() as i64 + self.n0
    }

    #[inline]
    pub fn is_downbeat(&self, n: i64) -> bool {
        self.bar_len > 0 && (n - self.downbeat as i64).rem_euclid(self.bar_len as i64) == 0
    }

    /// Whether beat `n` gets the accent: it is beat one of a bar and the
    /// one is known, not guessed.
    #[inline]
    pub fn accent(&self, n: i64) -> bool {
        self.downbeat_known && self.is_downbeat(n)
    }

    /// Time of MIDI tick `m` (0..24) of beat `n`.
    #[inline]
    pub fn tick_time(&self, n: i64, m: u32) -> f64 {
        self.t0 + ((n - self.n0) as f64 + m as f64 / 24.0) * self.period
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_math() {
        let g = Grid { t0: 10.0, n0: 4, period: 0.5, ..Grid::default() };
        assert_eq!(g.beat_time(4), 10.0);
        assert_eq!(g.beat_time(6), 11.0);
        assert_eq!(g.nearest_beat(10.9).0, 6);
        assert!((g.nearest_beat(10.9).1 + 0.1).abs() < 1e-9);
        assert_eq!(g.first_beat_at_or_after(10.0), 4);
        assert_eq!(g.first_beat_at_or_after(10.01), 5);
        assert!((g.tick_time(5, 12) - 10.75).abs() < 1e-9);
        assert!(g.is_downbeat(4) && g.is_downbeat(8) && !g.is_downbeat(5));
    }
}
