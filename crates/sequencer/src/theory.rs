//! Keys and scales: what the KEY page sets by hand (or the key detector
//! hears), and what the generators write notes from.

use serde::{Deserialize, Serialize};

/// Pitch class names, sharps.
pub const PITCH_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// A scale: the intervals above the tonic that belong to the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scale {
    #[default]
    Major,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Minor,
    Locrian,
    HarmonicMinor,
    MelodicMinor,
    PentatonicMajor,
    PentatonicMinor,
    Blues,
}

impl Scale {
    /// Every scale, in the order the KEY page lists them: the seven church
    /// modes first, then the rest.
    pub const ALL: [Scale; 12] = [
        Scale::Major,
        Scale::Dorian,
        Scale::Phrygian,
        Scale::Lydian,
        Scale::Mixolydian,
        Scale::Minor,
        Scale::Locrian,
        Scale::HarmonicMinor,
        Scale::MelodicMinor,
        Scale::PentatonicMajor,
        Scale::PentatonicMinor,
        Scale::Blues,
    ];

    /// Semitones above the tonic, ascending, tonic first.
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Scale::Major => &[0, 2, 4, 5, 7, 9, 11],
            Scale::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Scale::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            Scale::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            Scale::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Scale::Minor => &[0, 2, 3, 5, 7, 8, 10],
            Scale::Locrian => &[0, 1, 3, 5, 6, 8, 10],
            Scale::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Scale::MelodicMinor => &[0, 2, 3, 5, 7, 9, 11],
            Scale::PentatonicMajor => &[0, 2, 4, 7, 9],
            Scale::PentatonicMinor => &[0, 3, 5, 7, 10],
            Scale::Blues => &[0, 3, 5, 6, 7, 10],
        }
    }

    /// The name on the KEY page.
    pub fn name(self) -> &'static str {
        match self {
            Scale::Major => "MAJOR",
            Scale::Dorian => "DORIAN",
            Scale::Phrygian => "PHRYGIAN",
            Scale::Lydian => "LYDIAN",
            Scale::Mixolydian => "MIXOLYD",
            Scale::Minor => "MINOR",
            Scale::Locrian => "LOCRIAN",
            Scale::HarmonicMinor => "HARM MIN",
            Scale::MelodicMinor => "MEL MIN",
            Scale::PentatonicMajor => "PENT MAJ",
            Scale::PentatonicMinor => "PENT MIN",
            Scale::Blues => "BLUES",
        }
    }

    /// Short name for narrow screens.
    pub fn short(self) -> &'static str {
        match self {
            Scale::Major => "MAJ",
            Scale::Dorian => "DOR",
            Scale::Phrygian => "PHR",
            Scale::Lydian => "LYD",
            Scale::Mixolydian => "MIX",
            Scale::Minor => "MIN",
            Scale::Locrian => "LOC",
            Scale::HarmonicMinor => "HMIN",
            Scale::MelodicMinor => "MMIN",
            Scale::PentatonicMajor => "PMAJ",
            Scale::PentatonicMinor => "PMIN",
            Scale::Blues => "BLUES",
        }
    }
}

/// A key: a tonic pitch class and a scale on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// 0 = C ... 11 = B.
    pub root: u8,
    pub scale: Scale,
}

impl Key {
    pub fn new(root: u8, scale: Scale) -> Self {
        Self { root: root % 12, scale }
    }

    /// "A MINOR", "F# DORIAN".
    pub fn name(self) -> String {
        format!("{} {}", PITCH_NAMES[(self.root % 12) as usize], self.scale.name())
    }

    /// Whether a MIDI note (or pitch class) belongs to the key.
    pub fn contains(self, note: u8) -> bool {
        let pc = (note as i32 - self.root as i32).rem_euclid(12) as u8;
        self.scale.intervals().contains(&pc)
    }

    /// Whether a pitch class is the tonic.
    pub fn is_tonic(self, note: u8) -> bool {
        note % 12 == self.root % 12
    }

    /// The notes of the key from `low` (inclusive) to `high` (inclusive),
    /// ascending.
    pub fn notes_between(self, low: u8, high: u8) -> Vec<u8> {
        (low..=high.min(127)).filter(|&n| self.contains(n)).collect()
    }

    /// `count` notes of the key upwards from the tonic at or above `from`.
    pub fn notes_from_tonic(self, from: u8, count: usize) -> Vec<u8> {
        let tonic = self.tonic_at_or_above(from);
        (tonic..=127).filter(|&n| self.contains(n)).take(count).collect()
    }

    /// The tonic at or above `n`.
    pub fn tonic_at_or_above(self, n: u8) -> u8 {
        let up = (self.root as i32 - n as i32).rem_euclid(12);
        (n as i32 + up).min(127) as u8
    }

    /// The tonic at or below `n`.
    pub fn tonic_at_or_below(self, n: u8) -> u8 {
        let down = (n as i32 - self.root as i32).rem_euclid(12);
        (n as i32 - down).max(0) as u8
    }
}

/// The KEY page: hear the key from the microphone, or set it by hand.
/// Saved with the patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeySetup {
    /// True: the key detector's estimate is the key. False: `key` is.
    pub auto: bool,
    /// The key set by hand (kept while in auto, so switching back finds it).
    pub key: Key,
}

impl Default for KeySetup {
    fn default() -> Self {
        Self { auto: true, key: Key::new(0, Scale::Major) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_know_their_notes() {
        let am = Key::new(9, Scale::Minor);
        assert_eq!(am.name(), "A MINOR");
        for n in [57u8, 59, 60, 62, 64, 65, 67, 69] {
            assert!(am.contains(n), "{n}");
        }
        assert!(!am.contains(58) && !am.contains(61));
        assert_eq!(am.notes_between(57, 69), vec![57, 59, 60, 62, 64, 65, 67, 69]);
        assert_eq!(am.notes_from_tonic(60, 8), vec![69, 71, 72, 74, 76, 77, 79, 81]);
        assert_eq!(am.tonic_at_or_above(60), 69);
        assert_eq!(am.tonic_at_or_above(69), 69);
        assert_eq!(am.tonic_at_or_below(60), 57);
        assert!(am.is_tonic(45) && !am.is_tonic(46));
        let fs = Key::new(6, Scale::Dorian);
        assert_eq!(fs.name(), "F# DORIAN");
        assert_eq!(Key::new(0, Scale::PentatonicMajor).notes_between(60, 72), vec![60, 62, 64, 67, 69, 72]);
        for s in Scale::ALL {
            let iv = s.intervals();
            assert_eq!(iv[0], 0);
            assert!(iv.windows(2).all(|w| w[0] < w[1]) && *iv.last().unwrap() < 12, "{s:?}");
            assert!(!s.name().is_empty() && s.short().len() <= 5);
        }
    }
}
