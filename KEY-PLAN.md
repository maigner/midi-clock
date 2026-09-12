# Key Detection from the Sound Input — Plan

> **Status (2026-09-12):** phases 0–3 are implemented in `crates/key`, the
> `[key]` config section, the key thread in `crates/app` and the **KEY** tab
> (README, "Key"). Differences from the plan below: the readout got its own
> tab with AUTO/MANUAL instead of a line on the TEMPO tab, and the manual
> side sets a root and one of twelve scales; there is no RELOCK button any
> more, so nothing resets the detector but silence; peaks must also be
> within 50 dB of the frame's strongest and above -90 dBFS (window sidelobes
> and the digital floor of quiet recordings were counted otherwise); the
> `eval synth-tonal` / `eval key` tools and phase 4 (field recordings) are
> still to do.

Goal: while the band plays, the box also reports which key the music is in
("A minor", "F major"), with a confidence, so the screen can show it and the
BASS sequencer can highlight the notes of that key. Same audio interface, no
extra hardware. The input is a **room mic**: one microphone somewhere in the
rehearsal room that hears the whole band, drums included, at a distance.

Unlike the beat tracker, this has no latency requirement: a result that is right
after 5–10 seconds and stays right is what matters. That decides most of the
design below: key detection runs at ordinary priority, off the real-time path,
and integrates over many seconds.

---

## 0. Method in one paragraph

Take the mono input, throw away everything above 4 kHz, and every ~170 ms take a
long FFT (≈0.7 s window) so that bass notes are resolved. Pick the spectral
peaks, estimate the band's tuning (A4 is not always 440 Hz), and fold every
peak onto one of the 12 pitch classes, weighted by its log amplitude. Sum these
**chroma** vectors over a sliding window of ~12 s. Correlate the summed chroma
with 24 key profiles (12 major, 12 minor: the Krumhansl–Kessler / Temperley
"how much does each scale degree occur" profiles). The best profile is the key;
the margin over the runner-up is the confidence. Apply hysteresis so the readout
does not flicker between a key and its relative.

This is the standard, well-understood approach (Krumhansl–Schmuckler, as used by
KeyFinder, Mixed In Key, Essentia). It is cheap, needs no training data, and its
failure modes are known and listed in §6.

---

## 1. What a room mic gives us, and what it costs

A room mic is the right input for this job. Harmony instruments (bass, guitar,
keys, vocals) all reach it, whereas a mic at the kit hears mostly drums. Three
things still need handling:

1. **Drums are still in the mix**, just not dominant. A snare or crash is a
   broadband burst that flattens the spectrum for ~100 ms. Defence: count only
   spectral *peaks* that stand ≥ 6 dB above the whitened floor (broadband
   energy raises the floor and is rejected), and weight every frame by its
   tonalness (1 − spectral flatness), so burst frames contribute little.
2. **Reverb and distance.** The room smears notes in time and adds a diffuse
   tail. For key detection this is mostly harmless and even helps: sustained
   harmonic content is what we integrate anyway, and a 12 s window is much
   longer than any rehearsal-room decay. What it does cost is note *onsets*,
   which we do not need here.
3. **Room modes and hum.** A room has standing waves below ~200 Hz and the
   PA/backline can put 50 Hz hum and its harmonics into the mic. Both are
   stationary peaks at fixed frequencies, and a stationary peak biases one
   pitch class forever. Defence: a slow **long-term spectral background**
   (running mean of the log spectrum, τ ≈ 60 s) is subtracted before peak
   picking. Music changes; room modes and hum do not, so they end up in the
   background and disappear. This also removes fan noise and the like.

If the spike (§7, phase 0) shows that drum bleed is still a problem in a
drum-heavy room, the next step is harmonic/percussive separation on the
spectrogram (a median filter across time per bin keeps harmonic ridges, a
median across frequency keeps percussive columns; keep the harmonic part). It
is a few dozen lines and cheap at this frame rate; it adds ~0.5 s of latency,
which is irrelevant here.

The interface has two inputs. A config option `audio.key_channel` lets key
detection listen to a different channel than the beat tracker, e.g. a mixer
aux feed, if one ever turns out to be better than the room mic. Defaults to
the mic channel, and the room mic is the expected setup.

---

## 2. Signal processing, concretely

| Stage | Choice | Why |
|---|---|---|
| Decimation | 48 kHz → 12 kHz, ×4, 63-tap Kaiser low-pass FIR at 5 kHz | Everything relevant is below 4 kHz (C8 = 4186 Hz). 4× fewer samples make the long FFT cheap and sharpen resolution. |
| Frame | N = 8192 at 12 kHz (683 ms), Hann, hop 2048 (171 ms, ~6 frames/s) | 1.46 Hz per bin. At A1 (55 Hz) a semitone is 3.3 Hz, ≥ 2 bins, resolvable with interpolation. |
| Range | 55 Hz (A1) … 4 kHz | Below A1 the resolution is marginal and it is mostly kick; above 4 kHz only overtones. |
| Background | Per-bin running mean of the log-magnitude, τ = `key.background_s` (60 s), subtracted | Removes room modes, hum, fans: anything stationary (§1 point 3). |
| Floor | Running median of the background-subtracted log-magnitude over ±24 bins; peak must exceed median + 6 dB | Spectral whitening; rejects broadband drum energy. |
| Peaks | Local maxima with parabolic interpolation → (frequency, amplitude) | Sub-bin accuracy for tuning and pitch-class assignment. |
| Tuning | For every peak: cents deviation from the nearest 440-based semitone, in [−50, 50). Amplitude-weighted circular mean, τ ≈ 30 s. | Bands tune to 432, 441, 445 Hz, or drift; ±50 cents of tolerance, reported on screen. |
| Pitch class | `pc = round(12·log2(f / A4')) mod 12`, A4' = 440·2^(tuning/1200), rotated so C = 0 | |
| Weight | `w = ln(1 + k·amplitude)` × band weight (1.0 below 1 kHz, 0.5 above) | Log compression so a loud lead does not dominate; bass defines the key. |
| Frame weight | `1 − flatness` clamped, flatness = geometric mean / arithmetic mean of the power spectrum in range | §1 point 1: drum-burst frames count for little. |
| Integration | `acc = acc·(1−α) + chroma·frame_weight·α`, τ = `key.window_s` (12 s) | Sliding, exponential; reset on Relock and on silence. |
| Matching | Normalise `acc`; Pearson correlation with 24 rotated profiles | Krumhansl–Kessler and Temperley profiles built in, selectable; Sha'ath (KeyFinder) profile to be added and compared in the spike. |
| Confidence | `(r_best − r_second) / 0.15` clamped to 0..1, and 0 if `r_best < 0.5` | Margin, not absolute fit; a flat chroma has low margin. |
| Hysteresis | A new key must win for `key.hold_s` (3 s) of consecutive estimates before it replaces the shown one; shown key is kept but dimmed while confidence < 0.3; cleared after `tempo.stop_after_s` of silence | Same idea as the tempo hysteresis. |
| Alternate | If the relative major/minor (or the fifth) is within 0.05 correlation of the best, report it as `alt` | Major/relative-minor is inherently ambiguous; say so instead of guessing. |

Profiles (pitch class 0 = tonic):

```
Krumhansl–Kessler major: 6.35 2.23 3.48 2.33 4.38 4.09 2.52 5.19 2.39 3.66 2.29 2.88
Krumhansl–Kessler minor: 6.33 2.68 3.52 5.38 2.60 3.53 2.54 4.75 3.98 2.69 3.34 3.17
Temperley major:         5.0  2.0  3.5  2.0  4.5  4.0  2.0  4.5  2.0  3.5  1.5  4.0
Temperley minor:         5.0  2.0  3.5  4.5  2.0  4.0  2.0  4.5  3.5  2.0  1.5  4.0
```

CPU: the FIR is 63 MACs per output sample at 12 kHz, the 8192-point real FFT
runs 6 times a second, the background update is one multiply-add per bin, the
median floor is a few hundred thousand comparisons per frame. Well under 1 %
of one Pi 4 core; measured in phase 2.

---

## 3. Where it lives in the code

New crate **`crates/key`**, same shape as `crates/onset`: pure DSP, `realfft`,
no ALSA, no allocation after construction, feed samples in, get results out.

```rust
pub struct KeyConfig { sample_rate, decimate, fft_size, hop, fmin_hz, fmax_hz,
                       window_s, hold_s, profile: Profile, band_split_hz, ... }
pub enum Mode { Major, Minor }
#[derive(Clone, Copy)]
pub struct KeyEstimate {
    pub tonic: u8,              // 0 = C … 11 = B
    pub mode: Mode,
    pub confidence: f32,        // 0..1
    pub tuning_cents: f32,      // A4 offset from 440 Hz
    pub chroma: [f32; 12],      // the integrated, normalised chroma, for the screen
    pub alt: Option<(u8, Mode)>,// close runner-up (relative / fifth)
    pub at: f64,                // rtio::now() of the frame that produced it
}
pub struct KeyDetector { ... }
impl KeyDetector {
    pub fn new(cfg: KeyConfig) -> Self;
    pub fn process(&mut self, samples: &[f32], on_frame: impl FnMut(&KeyEstimate));
    pub fn reset(&mut self);
    pub fn estimate(&self) -> Option<KeyEstimate>;
}
```

Data flow in `crates/app`:

```
audio thread (SCHED_FIFO)                key thread (normal priority)
  read period ─► mono ─► onset detector    loop:
                   │                         drain ring ─► KeyDetector::process
                   └─► ringbuf<f32> (4 s) ─►   on_frame ─► shared.key.store(Arc::new(est))
                       try_push_slice          park_timeout(50 ms)
                       (overflow counted)
```

- The audio thread only copies `period` samples into a `ringbuf` (already a
  dependency) and unparks the key thread. Zero extra work on the RT path
  beyond that copy. If `audio.key_channel` differs from `mic_channel`, a
  second mono buffer is extracted.
- The key thread is a plain thread; it can be starved for a while with no
  audible consequence. Ring overflows are counted in `Shared` and shown like
  xruns are.
- `Shared` gets `key: ArcSwap<Option<KeyEstimate>>`. The UI status closure
  and the headless status line read it.
- `Action::Relock` also resets the key detector (`live.relock` is already an
  atomic flag; the key thread polls it too). Silence detection reuses the
  tracker's Stopped state via `live.tracker_grid` or a simple RMS gate in the
  key thread; the RMS gate is simpler and keeps the crates decoupled.

Config, `[key]` in `config.toml`, all with defaults:

```toml
[key]
enabled = true
window_s = 12.0        # integration time
hold_s = 3.0           # a new key must win this long before it is shown
profile = "temperley"  # or "krumhansl", "shaath"
fmin_hz = 55.0
fmax_hz = 4000.0
background_s = 60.0    # stationary-noise (room modes, hum) removal time constant
tuning = "auto"        # or a number: A4 in Hz, e.g. 440

[audio]
key_channel = 0        # input channel for key detection, default = mic_channel (the room mic)
```

Headless status line gains `key Am 0.72 (+4c)`; `--` while unknown.

---

## 4. Screen

**TEMPO tab.** The beat-lamp row has free space to the right. Put the key
there:

```
 [TEMPO] [SEQUENCE] [BASS]    RUNNING      IN ████░░░
 [ AUTO ]
 [MANUAL]        120.0 BPM
            TEMPO STRENGTH 0.41       CONF ██████░░
                 ● ○ ○ ○           KEY A MIN ▮▮▮▯  +4c
 [ -5 ] [ -1 ] [    TAP    ] [ +1 ] [ +5 ]
 [   STOP   ] [RELOCK]      MIDI 12k TICKS  LATE MAX 82 US
```

- Key name in TEXT colour when confidence ≥ 0.6, YELLOW between 0.3 and 0.6,
  DIM below; `--` when nothing is known. Small 4-segment confidence bar.
  The tuning offset is shown when |cents| ≥ 5, so an out-of-tune band or a
  432 Hz session is visible.
- If `alt` is set, show it small: `A MIN (C MAJ)`.
- A 12-bar chroma strip is useful while tuning the detector; add it to the
  `--ui-snapshot`/uisim rendering behind a config flag `ui.show_chroma`, off
  by default.

**BASS tab.** Rows whose note is in the detected key get their label drawn in
TEXT instead of DIM, the tonic row a touch brighter. Nothing moves and nothing
is auto-transposed: the highlight is a hint, the sequence is still the user's.
Only applied at confidence ≥ 0.6, with the same hysteresis, so rows do not
flicker.

The top-bar "key" readout could later also appear on SEQUENCE/BASS next to the
tempo; not in the first version.

---

## 5. Offline evaluation first (`tools/eval`)

Nothing goes on the Pi before it scores well on files. Extend the eval tool:

- `eval synth-tonal --key Am --progression i-VI-III-VII --bpm 120 --seconds 30
  --tuning-cents +8 --drums-db -6 --reverb 1.2 --hum-db -30 --noise 0.01
  -o x.wav` — band-limited sawtooth chords plus a root-note bass line, random
  voicings per bar, mixed with the existing drum synthesiser at a chosen
  level, then passed through a simple room: a feedback-delay reverb with the
  given RT60 in seconds, a couple of fixed low-frequency resonances standing
  in for room modes, and 50 Hz hum with harmonics. Writes `x.wav` and `x.key`
  (the key, or a timeline `t key` for modulations). A `--modulate-at 15:F`
  option creates a key change for lag measurement.
- `eval key x.wav [--ann x.key] [--set window_s=8] [--dump-chroma chroma.csv]`
  — runs `KeyDetector` on the file, prints the key timeline, and scores it.
- Scoring, the MIREX convention: exact 1.0, perfect fifth 0.5, relative
  major/minor 0.3, parallel major/minor 0.2, else 0. Plus: **time to first
  correct key** after start (target < 8 s), **spurious changes per minute** on
  steady material (target 0), and **confidence during drum-only sections**
  (target < 0.3, i.e. no key shown).
- A `test-data/real/` directory (gitignored) for recordings with known keys,
  ideally a few minutes recorded with the actual room mic in the actual room
  (the rehearsal recording is the best test data there is); a small script
  runs the whole directory and prints a table. Real material is what decides
  the profile set and the background/flatness settings; synthetic material is
  for regression.

Unit tests in `crates/key`: a pure 440 Hz tone lands on pitch class A; a
tone at 445 Hz reports +20 cents and still lands on A; a synthesised C major
triad plus C bass gives C major with confidence > 0.6 within 20 frames; white
noise gives confidence < 0.2; a constant 50 Hz + 100 Hz hum under a C major
triad vanishes from the chroma after the background has settled; decimator
passes 1 kHz and kills 7 kHz.

---

## 6. Honest limits

- **Relative keys.** A minor and C major share all seven notes; the profiles
  separate them by how often tonic and fifth occur, which works for most songs
  but not all. The `alt` field exists for this reason; the screen shows both
  when they are close.
- **Modal music.** Dorian, Mixolydian, blues with a ♭7 or a ♭3 over major
  chords map to the nearest major/minor. That is usually the musically useful
  answer anyway. Mode-specific profiles can be added later; the matching code
  does not care how many profiles there are.
- **Drum-only passages** produce no key. The readout dims and keeps the last
  key; it does not guess.
- **Modulations** show up with roughly half the window (≈ 6 s) of lag by
  design. Lowering `window_s` trades that against stability.
- **Room balance.** A room mic hears whoever is loudest in the room. If the
  guitar amp points at the mic and the bass does not, the chroma is
  guitar-heavy; still usually the right key, but with less bass to anchor the
  tonic. Mic placement matters more than any setting; phase 4 gives a
  recommendation.
- **Stationary music.** The background subtraction treats anything constant
  for a minute as noise. A pedal note or drone held for minutes gets slowly
  attenuated. Rare in band practice; `background_s` can be raised.
- **Loud single instruments.** A guitar solo on one pedal note pulls the key
  toward that note's fifth for a few seconds. Log compression and the band
  weight reduce it; hysteresis hides most of it.

---

## 7. Phases

| Phase | Work | Done when |
|---|---|---|
| 0. Spike | `crates/key` minimal (decimate, FFT, peaks, chroma, KK/Temperley matching, no hysteresis) + `eval synth-tonal` and `eval key` | Synthetic progressions in all 24 keys, drums at −6 dB, score ≥ 0.9 MIREX; a table of window_s × profile is in the plan's notes. |
| 1. Crate | Tuning estimate, background subtraction, flatness weighting, hysteresis, `alt`, unit tests, `reset()`; HPSS only if phase 0 says it is needed | Unit tests pass; eval scores hold with drums at 0 dB, RT60 1.5 s and −30 dB hum; spurious changes/min = 0 on steady synthetic material. |
| 2. Integration | Ring buffer from the audio thread, key thread, `[key]` config, `Shared.key`, status line, Relock reset, silence reset | Runs on the Pi with 0 xruns and < 2 % of one core for the key thread; a chord played in the room shows on the status line within 8 s (dev machine with the Flow 8 first). |
| 3. Screen | TEMPO-tab readout, BASS-tab in-key rows, snapshot and uisim rendering, `ui.show_chroma` | Renders at 800×480 and 1024×600 snapshots; uisim shows it. |
| 4. Field | Record a rehearsal through the room mic into `test-data/real`, try two or three mic positions, run the table | A short table in the README: position vs time to key and stability; README section written with a mic placement recommendation. |

Order of files touched, roughly: `crates/key/*` → `tools/eval/src/main.rs`
→ `crates/app/src/config.rs`, `crates/app/src/main.rs` → `crates/ui/src/screen.rs`
→ `deploy/config.example.toml`, `README.md`.
