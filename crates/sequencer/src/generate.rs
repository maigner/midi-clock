//! Writing a phrase in a key: a line composed by the rules of species
//! counterpoint, against the other voice if it has one. The rules choose
//! the pitches; the rhythm is one of several (`Rhythm`), from quarters and
//! halves to groups of three, five and seven 16ths.
//!
//! The counterpoint rules, as taught (Fux, first and second species),
//! reduced to a score per candidate note and a weighted draw, so that every
//! press of the button gives a different but well-formed line:
//! - start and end on the tonic; the penultimate note is a scale step away
//!   from it (leading tone or supertonic);
//! - move by step; thirds are fine, fourths, fifths and the octave rare;
//!   sixths rarer still; sevenths, tritones and anything wider than an
//!   octave never;
//! - a leap is followed by a step back the other way; no more than four
//!   moves in one direction; a note is not repeated;
//! - one climax, ideally in the middle, not reached twice;
//! - against the other voice only consonances on note onsets (unison,
//!   thirds, fifths, sixths, octaves), imperfect ones preferred, no
//!   parallel fifths or octaves, contrary motion preferred, no crossing.

use crate::theory::Key;

/// How the notes of a generated line are grouped. The CPT button moves
/// on to the next rhythm with every press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rhythm {
    /// Quarters and halves with a few eighths.
    #[default]
    Straight,
    /// Groups of three 16ths: 3+3+2 and its relatives.
    Threes,
    /// Groups of five: 5+5+6, 5+5+3+3.
    Fives,
    /// Groups of seven: 7+7+2, or the seven split 3+4.
    Sevens,
}

impl Rhythm {
    pub const ALL: [Rhythm; 4] = [Rhythm::Straight, Rhythm::Threes, Rhythm::Fives, Rhythm::Sevens];

    /// The rhythm after this one, round and round.
    pub fn next(self) -> Rhythm {
        match self {
            Rhythm::Straight => Rhythm::Threes,
            Rhythm::Threes => Rhythm::Fives,
            Rhythm::Fives => Rhythm::Sevens,
            Rhythm::Sevens => Rhythm::Straight,
        }
    }

    /// The size of its groups in 16ths: what the button shows.
    pub fn group(self) -> u8 {
        match self {
            Rhythm::Straight => 4,
            Rhythm::Threes => 3,
            Rhythm::Fives => 5,
            Rhythm::Sevens => 7,
        }
    }
}

/// A small deterministic generator (xorshift*), seeded from the tap time
/// so a sequence can be reproduced in tests.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self((seed ^ 0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in 0..n (0 when n is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.unit() * n as f64) as usize % n
        }
    }

    /// An index drawn with the given weights (uniform if they sum to nothing).
    pub fn weighted(&mut self, w: &[f64]) -> usize {
        let total: f64 = w.iter().filter(|x| x.is_finite() && **x > 0.0).sum();
        if total <= 0.0 || w.is_empty() {
            return self.below(w.len());
        }
        let mut r = self.unit() * total;
        for (i, &x) in w.iter().enumerate() {
            if x.is_finite() && x > 0.0 {
                if r < x {
                    return i;
                }
                r -= x;
            }
        }
        w.len() - 1
    }
}

/// A generated note: step it starts on, length in steps, and how strong
/// its beat is (+1 the downbeat, 0 a beat, -1 off the beat).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenNote {
    pub start: usize,
    pub len: u16,
    pub note: u8,
    pub accent: i8,
}

/// The voice being written and what it is written against.
pub struct Voice<'a> {
    /// The notes the voice may use, ascending (the notes of the key in its
    /// range).
    pub pitches: &'a [u8],
    /// The other voice: the note sounding on each step of the phrase, or an
    /// empty slice when there is none.
    pub other: &'a [Option<u8>],
    /// This voice lies above the other one.
    pub above: bool,
}

/// Write a phrase of `n` steps: a line by the counterpoint rules above, its
/// notes grouped as `rhythm` says.
pub fn generate(rhythm: Rhythm, key: Key, v: &Voice, n: usize, rng: &mut Rng) -> Vec<GenNote> {
    if v.pitches.is_empty() || n == 0 {
        return Vec::new();
    }
    let onsets = rhythm_counterpoint(rhythm, n, rng);
    let pitches = counterpoint_pitches(key, v, &onsets, rng);
    let accent = |start: usize| if start.is_multiple_of(16) { 1 } else if start.is_multiple_of(4) { 0 } else { -1 };
    onsets.iter().zip(&pitches).map(|(&(start, len), &note)| GenNote { start, len, note, accent: accent(start) }).collect()
}

/// Note lengths in 16ths that fill one bar, or two where a grouping runs
/// across the bar line, and how often the pattern is drawn.
type Pattern = (&'static [u16], f64);

impl Rhythm {
    /// The patterns a phrase is made of, and those for its last bar: one
    /// bar each, ending on a long note.
    fn patterns(self) -> (&'static [Pattern], &'static [Pattern]) {
        match self {
            Rhythm::Straight => (
                &[
                    (&[4, 4, 4, 4], 4.0),
                    (&[8, 4, 4], 2.0),
                    (&[4, 4, 8], 2.0),
                    (&[8, 8], 1.0),
                    (&[4, 2, 2, 4, 4], 1.5),
                    (&[2, 2, 4, 4, 4], 1.0),
                    (&[4, 4, 2, 2, 4], 1.5),
                    (&[4, 4, 4, 2, 2], 1.0),
                    (&[6, 2, 4, 4], 1.0),
                    (&[4, 2, 2, 8], 1.0),
                    (&[6, 2, 6, 2], 1.0),
                    (&[2, 4, 2, 4, 4], 1.0),
                    (&[4, 2, 4, 2, 4], 1.0),
                ],
                &[(&[4, 4, 8], 3.0), (&[8, 8], 2.0), (&[4, 12], 1.0), (&[2, 2, 4, 8], 1.0), (&[4, 4, 4, 4], 1.0)],
            ),
            Rhythm::Threes => (
                &[
                    (&[3, 3, 2, 3, 3, 2], 3.0),
                    (&[3, 3, 3, 3, 4], 3.0),
                    (&[3, 3, 3, 3, 2, 2], 2.0),
                    (&[6, 6, 4], 2.0),
                    (&[3, 3, 2, 4, 4], 1.5),
                    (&[4, 4, 3, 3, 2], 1.5),
                    (&[6, 3, 3, 4], 1.0),
                    (&[3, 3, 4, 3, 3], 1.0),
                    (&[2, 3, 3, 3, 3, 2], 1.0),
                    // threes across the bar line
                    (&[3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2], 2.5),
                    (&[3, 3, 3, 3, 3, 3, 3, 3, 4, 4], 1.5),
                    (&[6, 6, 6, 6, 6, 2], 1.0),
                    (&[6, 6, 6, 6, 3, 3, 2], 1.0),
                ],
                &[(&[3, 3, 2, 8], 3.0), (&[3, 3, 10], 2.0), (&[6, 6, 4], 1.5), (&[3, 3, 3, 3, 4], 1.0), (&[6, 10], 1.0), (&[3, 3, 2, 3, 5], 1.0)],
            ),
            Rhythm::Fives => (
                &[
                    (&[5, 5, 6], 3.0),
                    (&[5, 5, 3, 3], 3.0),
                    (&[5, 3, 5, 3], 2.0),
                    (&[3, 5, 3, 5], 1.5),
                    (&[3, 3, 5, 5], 1.5),
                    (&[5, 6, 5], 1.5),
                    (&[6, 5, 5], 1.0),
                    (&[5, 3, 3, 5], 1.0),
                    (&[5, 5, 2, 2, 2], 1.0),
                    (&[5, 5, 4, 2], 1.0),
                    // fives across the bar line
                    (&[5, 5, 5, 5, 5, 5, 2], 2.5),
                    (&[5, 5, 5, 5, 6, 6], 1.5),
                    (&[5, 5, 3, 5, 5, 3, 3, 3], 1.5),
                    (&[5, 5, 5, 5, 5, 3, 4], 1.0),
                ],
                &[(&[5, 5, 6], 3.0), (&[5, 3, 8], 2.0), (&[3, 5, 8], 1.0), (&[5, 11], 1.0), (&[5, 5, 2, 4], 1.0)],
            ),
            Rhythm::Sevens => (
                &[
                    (&[7, 7, 2], 3.0),
                    (&[3, 4, 3, 4, 2], 2.5),
                    (&[4, 3, 4, 3, 2], 2.0),
                    (&[7, 5, 4], 2.0),
                    (&[7, 9], 1.5),
                    (&[7, 3, 3, 3], 1.5),
                    (&[4, 7, 5], 1.0),
                    (&[2, 7, 7], 1.0),
                    (&[7, 4, 5], 1.0),
                    (&[3, 4, 7, 2], 1.0),
                    // sevens across the bar line
                    (&[7, 7, 7, 7, 4], 2.5),
                    (&[3, 4, 3, 4, 3, 4, 3, 4, 4], 2.0),
                    (&[7, 7, 7, 4, 7], 1.0),
                    (&[7, 7, 7, 7, 2, 2], 1.0),
                ],
                &[(&[7, 9], 3.0), (&[3, 4, 9], 2.0), (&[4, 3, 9], 1.0), (&[7, 3, 6], 1.0), (&[7, 2, 7], 1.0)],
            ),
        }
    }
}

/// Note starts and lengths for a counterpoint line: patterns of the rhythm
/// one after the other, the last bar ending on a long note.
fn rhythm_counterpoint(rhythm: Rhythm, n: usize, rng: &mut Rng) -> Vec<(usize, u16)> {
    let (body, last) = rhythm.patterns();
    let steps = |pattern: &[u16]| pattern.iter().map(|&l| l as usize).sum::<usize>();
    let bars = (n / 16).max(1);
    let mut out = Vec::new();
    let mut pos = 0;
    let mut bar = 0;
    while bar < bars {
        // bars to fill before the last one: a pattern must not run into it
        let room = (bars - 1 - bar) * 16;
        let table = if room == 0 { last } else { body };
        let w: Vec<f64> = table.iter().map(|(p, w)| if room == 0 || steps(p) <= room { *w } else { 0.0 }).collect();
        let pattern = table[rng.weighted(&w)].0;
        for &len in pattern {
            if pos >= n {
                break;
            }
            let len = (len as usize).min(n - pos);
            out.push((pos, len as u16));
            pos += len;
        }
        bar += (steps(pattern) / 16).max(1);
    }
    if out.is_empty() {
        out.push((0, n.min(16) as u16));
    }
    out
}

/// Pitches for the given onsets by the counterpoint rules (see the module
/// doc): every candidate gets a score, the note is drawn with weight
/// `exp(score)`, rule breaches get no weight at all.
fn counterpoint_pitches(key: Key, v: &Voice, onsets: &[(usize, u16)], rng: &mut Rng) -> Vec<u8> {
    let p = v.pitches;
    let m = onsets.len();
    let tonics: Vec<usize> = (0..p.len()).filter(|&i| key.is_tonic(p[i])).collect();
    let mut out = Vec::with_capacity(m);
    // index into `p` of the previous note, the semitones of the last move,
    // the run of moves in one direction (signed), the climax so far
    let mut prev: Option<usize> = None;
    let mut prev_d = 0i32;
    let mut run = 0i32;
    let mut max_i = 0usize;
    let mut climax_at = 0usize;
    // interval class against the other voice at the previous onset, and its note
    let mut prev_iv: Option<(i32, i32)> = None;
    for (i, &(start, _)) in onsets.iter().enumerate() {
        let o = v.other.get(start).copied().flatten();
        // The tonics the line may end on: consonant with the other voice at
        // the last onset. The penultimate note has to be a step from one.
        let o_last = onsets.last().and_then(|&(st, _)| v.other.get(st).copied().flatten());
        let endings: Vec<usize> = tonics
            .iter()
            .copied()
            .filter(|&t| o_last.is_none_or(|o| consonant(p[t], o, v.above)))
            .collect();
        let endings = if endings.is_empty() { tonics.clone() } else { endings };
        // score, and whether the note breaks a rule of form (first and last
        // note; never broken while any note keeps it), a harmonic rule
        // (never broken while any note keeps form and harmony) or a
        // melodic one (broken only when nothing obeys them all)
        let mut scores = vec![(0.0f64, true, true, true); p.len()];
        for (c, &note) in p.iter().enumerate() {
            let mut s = 0.0;
            let (mut form, mut harmonic, mut melodic) = (true, true, true);
            let last = i + 1 == m;
            let penultimate = i + 2 == m && i > 0;
            // --- against the other voice ----------------------------------------
            if let Some(o) = o {
                let diff = note as i32 - o as i32;
                if (v.above && diff < 0) || (!v.above && diff > 0) {
                    harmonic = false; // voices crossing
                }
                let iv = diff.abs() % 12;
                if !matches!(iv, 0 | 3 | 4 | 7 | 8 | 9) {
                    harmonic = false; // dissonant on the onset
                }
                if matches!(iv, 3 | 4 | 8 | 9) {
                    s += 1.0;
                }
                if diff == 0 && !(i == 0 || last) {
                    s -= 1.0; // a unison inside the line
                }
                if let (Some(pi), Some((piv, po))) = (prev, prev_iv) {
                    let my = note as i32 - p[pi] as i32;
                    let their = o as i32 - po;
                    if my != 0 && their != 0 {
                        let similar = (my > 0) == (their > 0);
                        if matches!(iv, 0 | 7) && piv == iv && similar {
                            harmonic = false; // parallel fifths or octaves
                        }
                        s += if similar { -0.4 } else { 0.7 };
                    }
                }
            }
            // --- first, last, penultimate ------------------------------------------
            if i == 0 {
                if key.is_tonic(note) {
                    s += 3.0;
                } else if o.is_some_and(|o| (note as i32 - o as i32).abs() % 12 == 7) {
                    s += 1.0; // a fifth over the other voice
                } else {
                    form = false;
                }
            }
            if last && !key.is_tonic(note) {
                form = false;
            }
            if penultimate && !endings.iter().any(|&t| (t as i32 - c as i32).abs() == 1) {
                melodic = false;
            }
            // --- melody ----------------------------------------------------------------
            if let Some(pi) = prev {
                let d = note as i32 - p[pi] as i32;
                let dd = (c as i32 - pi as i32).abs();
                match (dd, d.abs()) {
                    (0, _) => s -= 2.5,
                    (1, _) => s += 2.0,
                    (2, _) => s += 0.5,
                    (_, 6) | (_, 10) | (_, 11) => melodic = false,
                    (_, x) if x > 12 => melodic = false,
                    (_, 5) | (_, 7) => s -= 0.5,
                    (_, 12) => s -= 1.5,
                    (_, 8) | (_, 9) => s -= 2.0,
                    _ => s -= 1.0,
                }
                if last {
                    // the line ends with a step onto the tonic when the
                    // other voice allows it, a third at most otherwise
                    if dd == 1 {
                        s += 3.0;
                    } else if dd > 2 {
                        melodic = false;
                    }
                }
                if prev_d.abs() >= 5 && d != 0 {
                    if (d > 0) == (prev_d > 0) {
                        s -= 3.0; // a leap carried on
                    } else if dd == 1 {
                        s += 2.0; // a leap answered by a step back
                    }
                }
                if d != 0 && run != 0 && (d > 0) == (run > 0) && run.abs() >= 4 {
                    s -= 2.0;
                }
                if c > max_i {
                    s += if (m / 3..(2 * m).div_ceil(3)).contains(&i) { 0.5 } else { -0.5 };
                } else if c == max_i && climax_at < i {
                    s -= 1.5;
                }
            }
            scores[c] = (s, form, harmonic, melodic);
        }
        let weights = |keep: &dyn Fn(bool, bool, bool) -> bool| -> Vec<f64> {
            scores.iter().map(|&(s, f, h, ml)| if keep(f, h, ml) { (s * 1.3).exp() } else { 0.0 }).collect()
        };
        let tiers = [weights(&|f, h, ml| f && h && ml), weights(&|f, h, _| f && h), weights(&|f, _, _| f)];
        let c = if let Some(w) = tiers.iter().find(|w| w.iter().any(|&x| x > 0.0)) {
            rng.weighted(w)
        } else {
            // nothing fits: the nearest note to where we are, a tonic if the
            // line has to end
            let from = prev.unwrap_or(0) as i32;
            let pool: Vec<usize> = if i + 1 == m && !tonics.is_empty() { tonics.clone() } else { (0..p.len()).collect() };
            *pool.iter().min_by_key(|&&j| (j as i32 - from).abs()).unwrap_or(&0)
        };
        if let Some(pi) = prev {
            let d = p[c] as i32 - p[pi] as i32;
            if d != 0 {
                run = if run != 0 && (d > 0) == (run > 0) { run + d.signum() } else { d.signum() };
            }
            prev_d = d;
        }
        if prev.is_none() || c > max_i {
            max_i = c;
            climax_at = i;
        }
        prev_iv = o.map(|o| ((p[c] as i32 - o as i32).abs() % 12, o as i32));
        prev = Some(c);
        out.push(p[c]);
    }
    out
}

/// Whether `note` is consonant with `other` (unison, third, fifth, sixth,
/// octave and their compounds) and on the right side of it.
fn consonant(note: u8, other: u8, above: bool) -> bool {
    let diff = note as i32 - other as i32;
    let side_ok = if above { diff >= 0 } else { diff <= 0 };
    side_ok && matches!(diff.abs() % 12, 0 | 3 | 4 | 7 | 8 | 9)
}

/// Whether a line (the note sounding on each step) is worth writing
/// against: it has notes, and nearly all of them belong to the key.
pub fn in_key_line(other: &[Option<u8>], key: Key) -> bool {
    let notes: Vec<u8> = other.iter().flatten().copied().collect();
    if notes.is_empty() {
        return false;
    }
    let ok = notes.iter().filter(|&&n| key.contains(n)).count();
    ok * 5 >= notes.len() * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theory::Scale;

    fn seeds() -> impl Iterator<Item = u64> {
        (1..=40u64).map(|i| i * 7919)
    }

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let mut a = Rng::new(5);
        let mut b = Rng::new(5);
        for _ in 0..100 {
            assert_eq!(a.next(), b.next());
            let u = a.unit();
            assert!((0.0..1.0).contains(&u));
            assert_eq!(u, b.unit());
            assert!(a.below(7) < 7);
            b.below(7);
        }
        assert_eq!(a.below(0), 0);
        let mut counts = [0usize; 3];
        for _ in 0..3000 {
            counts[a.weighted(&[1.0, 0.0, 3.0])] += 1;
        }
        assert_eq!(counts[1], 0);
        assert!(counts[2] > counts[0] * 2, "{counts:?}");
    }

    /// Melodic checks that every counterpoint line must pass.
    fn check_melody(key: Key, line: &[u8], pitches: &[u8]) {
        assert!(line.len() >= 2);
        assert!(key.is_tonic(line[0]), "starts on the tonic: {line:?}");
        assert!(key.is_tonic(*line.last().unwrap()), "ends on the tonic: {line:?}");
        let idx = |n: u8| pitches.iter().position(|&p| p == n).unwrap();
        let pen = line[line.len() - 2];
        let end = line[line.len() - 1];
        assert!((idx(pen) as i32 - idx(end) as i32).abs() <= 2, "a step or a third onto the final tonic: {line:?}");
        for w in line.windows(2) {
            let d = (w[1] as i32 - w[0] as i32).abs();
            assert!(d <= 12 && !matches!(d, 6 | 10 | 11), "bad interval {d} in {line:?}");
        }
    }

    #[test]
    fn every_pattern_fills_its_bars() {
        for r in Rhythm::ALL {
            let (body, last) = r.patterns();
            for (p, w) in body {
                let n: u16 = p.iter().sum();
                assert!(n == 16 || n == 32, "{r:?} {p:?} is {n} steps");
                assert!(*w > 0.0 && p.iter().all(|&l| l >= 2));
            }
            assert!(body.iter().any(|(p, _)| p.iter().sum::<u16>() == 16), "{r:?}: nothing for a single bar");
            for (p, _) in last {
                assert_eq!(p.iter().sum::<u16>(), 16, "{r:?} {p:?}");
                assert!(*p.last().unwrap() >= 4, "{r:?} {p:?} does not end on a long note");
            }
            assert_eq!(Rhythm::ALL.iter().filter(|x| x.group() == r.group()).count(), 1);
        }
        let mut r = Rhythm::default();
        for want in Rhythm::ALL {
            assert_eq!(r, want);
            r = r.next();
        }
        assert_eq!(r, Rhythm::default(), "round and round");
    }

    #[test]
    fn rhythms_cover_the_phrase_in_their_groups() {
        for r in Rhythm::ALL {
            for bars in [1usize, 2, 4, 8] {
                let n = bars * 16;
                let mut across = false;
                for seed in seeds() {
                    let onsets = rhythm_counterpoint(r, n, &mut Rng::new(seed));
                    let mut pos = 0;
                    for &(start, len) in &onsets {
                        assert_eq!(start, pos, "{r:?} {onsets:?}");
                        pos += len as usize;
                    }
                    assert_eq!(pos, n, "{r:?} {onsets:?}");
                    // the last bar begins on its downbeat and ends long
                    assert!(onsets.iter().any(|&(s, _)| s == n - 16), "{r:?} {onsets:?}");
                    assert!(onsets.last().unwrap().1 >= 4);
                    let lens: Vec<u16> = onsets.iter().map(|o| o.1).collect();
                    let grouped = match r {
                        Rhythm::Straight => lens.iter().all(|l| l % 2 == 0),
                        Rhythm::Threes => lens.iter().any(|&l| l == 3 || l == 6),
                        Rhythm::Fives => lens.contains(&5),
                        Rhythm::Sevens => lens.contains(&7) || lens.windows(2).any(|w| w[0] + w[1] == 7),
                    };
                    assert!(grouped, "{r:?} {lens:?}");
                    across |= onsets.iter().any(|&(s, l)| s % 16 != 0 && (s % 16) + l as usize > 16);
                }
                // with room for it, a grouping sometimes runs over a bar line
                assert_eq!(across, r != Rhythm::Straight && bars >= 4, "{r:?} in {bars} bars");
            }
        }
    }

    #[test]
    fn counterpoint_alone_is_a_cantus_firmus() {
        for (scale, rhythm) in [Scale::Major, Scale::Minor, Scale::Mixolydian, Scale::PentatonicMinor, Scale::HarmonicMinor].into_iter().zip(Rhythm::ALL.into_iter().cycle()) {
            let key = Key::new(7, scale);
            let pitches = key.notes_between(31, 43);
            let v = Voice { pitches: &pitches, other: &[], above: false };
            let mut steps_total = 0;
            let mut moves = 0;
            for seed in seeds() {
                let notes = generate(rhythm, key, &v, 64, &mut Rng::new(seed));
                let line: Vec<u8> = notes.iter().map(|g| g.note).collect();
                check_melody(key, &line, &pitches);
                assert_eq!(notes[0].start, 0);
                assert!(notes.last().unwrap().len >= 4, "ends on a long note: {notes:?}");
                // the rhythm covers the phrase without gaps or overlaps
                for (a, b) in notes.iter().zip(&notes[1..]) {
                    assert_eq!(a.start + a.len as usize, b.start);
                }
                assert_eq!(notes.last().map(|g| g.start + g.len as usize), Some(64));
                let idx = |n: u8| pitches.iter().position(|&p| p == n).unwrap() as i32;
                for w in line.windows(2) {
                    moves += 1;
                    if (idx(w[1]) - idx(w[0])).abs() <= 1 {
                        steps_total += 1;
                    }
                }
            }
            assert!(steps_total * 2 > moves, "{scale:?}: mostly stepwise ({steps_total} of {moves})");
        }
    }

    #[test]
    fn counterpoint_against_a_bass_is_consonant_and_above() {
        let key = Key::new(0, Scale::Major);
        // a bass line: C G A F, a bar each, over 4 bars
        let bass = [48u8, 55, 57, 53];
        let other: Vec<Option<u8>> = (0..64).map(|i| Some(bass[i / 16])).collect();
        let pitches = key.notes_from_tonic(60, 8);
        let v = Voice { pitches: &pitches, other: &other, above: true };
        for (seed, rhythm) in seeds().flat_map(|s| Rhythm::ALL.map(|r| (s, r))) {
            let notes = generate(rhythm, key, &v, 64, &mut Rng::new(seed));
            let line: Vec<u8> = notes.iter().map(|g| g.note).collect();
            check_melody(key, &line, &pitches);
            let mut prev: Option<(i32, i32, u8)> = None;
            for g in &notes {
                let o = other[g.start].unwrap();
                assert!(g.note > o);
                let iv = (g.note as i32 - o as i32) % 12;
                assert!(matches!(iv, 0 | 3 | 4 | 7 | 8 | 9), "dissonance {iv} at {}: {line:?}", g.start);
                if let Some((piv, po, pn)) = prev {
                    let my = g.note as i32 - pn as i32;
                    let their = o as i32 - po;
                    if matches!(iv, 0 | 7) && piv == iv && my != 0 && their != 0 {
                        assert_ne!(my > 0, their > 0, "parallel perfect interval at {}: {line:?}", g.start);
                    }
                }
                prev = Some((iv, o as i32, g.note));
            }
        }
        // a line in the key is worth writing against, drums are not
        assert!(in_key_line(&other, key));
        assert!(!in_key_line(&[], key));
        let drums: Vec<Option<u8>> = vec![Some(36), Some(38), Some(42), Some(46)];
        assert!(!in_key_line(&drums, key));
    }

    #[test]
    fn counterpoint_below_a_melody_stays_below() {
        let key = Key::new(9, Scale::Minor);
        let melody = [69u8, 71, 72, 74, 76, 74, 71, 69];
        let other: Vec<Option<u8>> = (0..32).map(|i| Some(melody[i / 4])).collect();
        let pitches = key.notes_between(33, 45);
        let v = Voice { pitches: &pitches, other: &other, above: false };
        for (seed, rhythm) in seeds().flat_map(|s| Rhythm::ALL.map(|r| (s, r))) {
            let notes = generate(rhythm, key, &v, 32, &mut Rng::new(seed));
            assert!(key.is_tonic(notes.last().unwrap().note));
            for (i, g) in notes.iter().enumerate() {
                let o = other[g.start].unwrap();
                assert!(g.note < o);
                let consonant = matches!((o as i32 - g.note as i32) % 12, 0 | 3 | 4 | 7 | 8 | 9);
                // the line ends on the tonic even where the melody happens to
                // be dissonant against it (it resolves later)
                let tonic_possible = key.notes_between(33, 45).iter().any(|&t| key.is_tonic(t) && matches!((o as i32 - t as i32) % 12, 0 | 3 | 4 | 7 | 8 | 9));
                assert!(consonant || (i + 1 == notes.len() && !tonic_possible), "step {}: {} under {o} in {notes:?}", g.start, g.note);
            }
        }
    }
}
