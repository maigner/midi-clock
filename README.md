# midi-clock

Listens to live music through a microphone on a USB audio interface, tracks the
beat, and turns it into a MIDI master clock plus a metronome click, so sequencers
and synths follow the human players. Runs on a Raspberry Pi.

The design is **predictive**: an internal beat grid (period + phase) drives the
click and the MIDI clock, and detected onsets only *correct* the grid. While the
band holds a tempo, click and clock land on the beat with no lag, because they
are emitted before the hit is heard. See [PLAN.md](PLAN.md) for the rationale.

## Layout

| Path | What |
|---|---|
| `crates/rtio` | Raw ALSA duplex PCM with per-period timestamps, MIDI output (rawmidi or an ALSA sequencer port), SCHED_FIFO helpers |
| `crates/onset` | Drum-aware onset detection function (log spectral flux + kick and snare bands) |
| `crates/tracker` | Tempo estimation (comb-filtered autocorrelation), phase-locked beat grid, state machine |
| `crates/click` | Sample-accurate click rendered from the grid |
| `crates/midiclock` | The 24 PPQN clock thread, Start/Stop, lateness statistics |
| `crates/key` | Key detection from the room mic (long FFT, chroma, Krumhansl–Schmuckler key profiles, hysteresis) |
| `crates/sequencer` | 16-step, 8-track pattern sequencer driven by the beat grid, MIDI notes out; keys, scales and the RANDOM / counterpoint generators |
| `crates/ui` | Touch-screen front end: framebuffer drawing, evdev touch, no window system |
| `crates/app` | The `midi-clock` binary: threads, config, mode switching, status line |
| `tools/clockgen` | Phase-1 tool: fixed-tempo clock + click, MIDI jitter and loopback latency measurement |
| `tools/eval` | Offline evaluation: synthesise drum tracks, run the tracker on WAV files, score the emitted beats |
| `tools/uisim` | Desktop simulator for the touch UI: a window, the mouse as finger, a virtual drummer |
| `deploy/` | Example config, systemd unit, install and OS tuning scripts |

## Build

Needs Rust (stable) and the ALSA development package.

```sh
sudo apt install libasound2-dev       # on the Pi / any Debian
cargo build --release
```

Binaries land in `target/release/`: `midi-clock`, `clockgen`, `eval`.
Building natively on a Pi 4/5 takes a few minutes; cross-compiling for
`aarch64-unknown-linux-gnu` works with `cross` or a sysroot that contains
libasound.

## Bring-up on the Pi (do this in order)

1. **OS**: Raspberry Pi OS Lite 64-bit, PREEMPT_RT kernel if available.
   Run `sudo deploy/os-tuning.sh` and reboot.
2. **Devices**: `midi-clock --list-devices` prints the `hw:CARD=...` names and
   the rawmidi ports. Put them in the config.
3. **Prove the real-time path** before anything else:
   ```sh
   clockgen run --bpm 120 --audio hw:CARD=US2x2 --midi hw:CARD=US2x2,0,0 --seconds 600
   ```
   Wants: 0 xruns, tick lateness p99 well under 1 ms. If not, raise
   `period_frames` to 128 or fix the kernel/tuning first.
4. **Measure unreported latency** with a cable from an output to an input:
   ```sh
   clockgen latency --audio hw:CARD=US2x2 --channel 0
   ```
   Put the suggested values into `extra_input_latency_ms` / `extra_output_latency_ms`.
5. **Run it**: `midi-clock --config config.toml`. The status line shows state,
   BPM, confidence, input level, the key heard, onsets, clicks, ticks and xruns.
6. **Install as a service**: `sudo deploy/install.sh`, edit
   `/etc/midi-clock/config.toml`, `systemctl start midi-clock`.
7. **Touch screen** (optional): plug it in, reboot, and the UI appears on the
   console. If touches land in the wrong place set `touch_swap_xy` /
   `touch_invert_*` in `[ui]`; for a portrait mount set `rotation`.

## Behaviour

- **Idle → Locking → Running**: after the first onsets it needs two agreeing
  tempo estimates (about 2 s), then places the grid, sends MIDI Start just
  before the first predicted beat and starts clocking and clicking.
- **Running**: each onset within ±20 % of a beat nudges phase and period.
  Tempo is re-estimated every 250 ms from the last 4 s. Corrections stretch or
  compress tick spacing but never change the tick count, so slaves stay on the
  right beat.
- **Coasting**: after 3 missed beats (a break, a fill with no hits on the beat)
  the clock keeps running at the last tempo. Tempo re-estimation continues, so
  music that comes back at a clearly different tempo triggers a hot relock.
- **Stopped**: after `stop_after_s` seconds without onsets, MIDI Stop is sent.
  New onsets restart the lock and send Start again.
- **Downbeat**: once 8 beats are in, the beat with the most kick energy every
  `bar_len` beats becomes beat 1 and gets the accented click.

Honest limits: a sudden tempo jump is followed within one or two beats, not
instantly; a drummer who stops dead gets 3 more beats of click. Narrow
`bpm_min`/`bpm_max` per song when octave errors are a risk.

## Touch screen

A cheap HDMI, DSI or SPI touch screen on the Pi gives a second, simpler mode
and a step sequencer. There is no X, Wayland or browser involved: the screen
is drawn into a buffer and written to `/dev/fb0`, touches are read from
`/dev/input/event*`, all in one ordinary-priority thread that only redraws
when something visible changes (measured: under 1 % of a core, even on a 4K
framebuffer; the real-time threads do not see it).

Six tabs share the top bar. The clock state, the tempo and the input level
meter are shown on the TEMPO tab only (the tempo is set there too); the
other tabs show nothing in the top bar but xruns:

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]   RUNNING   IN ████░░░
 [ AUTO ]
 [MANUAL]        120.0 BPM
            TEMPO STRENGTH 0.41       CONF ██████░░
                 ● ○ ○ ○
 [ -5 ] [ -1 ] [    TAP    ] [ +1 ] [ +5 ]     only in MANUAL
 [   STOP   ]                        MIDI 12k TICKS
```

- **AUTO** (default): the microphone drives the clock as described above. The
  screen shows the tracker state, tempo strength, confidence bar, beat lamps
  (beat 1 in colour), input level, xruns and the MIDI tick count. The tap
  and tempo buttons are not on screen: the band sets the tempo.
- **MANUAL**: the clock is yours, and the buttons appear. **TAP** the beat:
  every tap puts a beat on the tap, two or more taps set the tempo. When
  stopped, tap "1 2 3 4" and the clock starts on the following "1" (the
  click already sounds during the count-in, MIDI Start goes out on the one).
  **+1/-1/+5/-5** change the tempo without moving the current beat (hold for
  auto-repeat), **START/STOP** send MIDI Start/Stop. The screen still shows
  what the mic hears, so you can see whether the band is with you.
- **Switching** is seamless in both directions: MANUAL takes over the running
  grid unchanged; AUTO continues from the manual grid and lets onsets correct
  it from there, which is also a quick way to get the tracker onto a known
  tempo before the drummer starts.
- Touch latency is subtracted from tap times (`tap_offset_ms`); if your taps
  consistently land the click late, raise it.

### Key (KEY tab)

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]
 [ AUTO ]
 [MANUAL]          A MINOR
            TUNING +7c   ALSO C MAJOR       CONF ██████░░
  C  C# D  D# E  F  F# G  G# A  A# B        the chroma heard, in-key notes bright
  ▂  ▁  ▃  ▁  ▅  ▂  ▁  ▃  ▁  █  ▁  ▂
```

- **AUTO** (default): the box listens to the room mic and works out which
  key the band plays in: every 170 ms a long FFT is folded onto the twelve
  pitch classes, the sum over the last `key.window_s` seconds is matched
  against the 24 major and minor key profiles, and the best match is the
  key. The name is shown in colour when the confidence is 0.6 or more, yellow
  above 0.3, dim below (the key is then kept on screen but not used). A new
  key must win for `key.hold_s` seconds before it replaces the shown one, so
  the readout does not flicker between a key and its relative; when the
  relative key or the fifth is nearly as good it is named (`ALSO C MAJOR`).
  The band's tuning is estimated and shown when it is 5 cents or more off
  A = 440 Hz. Room modes, hum and fans are learned as a slow background and
  subtracted; drum hits count for little. A drum-only passage shows no key;
  after `key.silence_reset_s` seconds of silence the key is forgotten. The
  strip at the bottom is the chroma: the notes of the key are bright, the
  tonic in colour. Honest limits: relative major/minor is ambiguous by
  nature (both are shown), modal music maps to the nearest major or minor,
  and a modulation shows up about half a window late.
- **MANUAL**: set the key by hand. Tap a root in the row of twelve and a
  scale below it: the seven modes (MAJOR, DORIAN, PHRYGIAN, LYDIAN,
  MIXOLYDIAN, MINOR, LOCRIAN), harmonic and melodic minor, major and minor
  pentatonic, and blues. The notes of the key are bright in the root row.
  Tapping a root or a scale while on AUTO switches to MANUAL. The setting is
  saved with the patterns.
- Once a key is known (heard with confidence, or set), the **RANDOM** and
  **CNTRPT** buttons on the LEAD and BASS tabs come alive (see below); the
  MIDI clock and the click do not care about the key.
- `[key]` in the config: `enabled`, `window_s`, `hold_s`, `profile`
  (`temperley` or `krumhansl`), `fmin_hz`/`fmax_hz`, `background_s`,
  `tuning_hz` (0 = estimate), `gate_dbfs`, `silence_reset_s`;
  `audio.key_channel` listens to another input than the beat tracker's mic
  (e.g. a mixer aux feed), -1 = the mic. The detector runs in its own
  ordinary-priority thread and costs well under a percent of a core; the
  audio thread only copies samples into a ring buffer.

### Lead sequencer (LEAD tab)

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]
 LEN [1] [2] [4] [8]   BAR [1] [2] [3] [4] [5] [6] [7] [8] [🔒]
  C1  ■ □ □ □ ■ □ □ □ ■ □ □ □ ■ □ □ □      8 tracks x 16 steps of the shown bar
  D1  □ □ □ □ ■ □ □ □ □ □ □ □ ■ □ □ □      playhead column highlighted
  ...
 [-] NOTE C1 [+]   [-] VEL 100 [+]   [RANDOM] [CNTRPT]
```

- Steps are 16ths of the bar; step 1 is the downbeat of the live grid, so the
  pattern is on the beat in both modes and follows every correction, like the
  click and the clock. It plays whenever the clock runs.
- **LEN** picks the phrase length, 1, 2, 4 or 8 bars; bars count from the
  downbeat, so the phrase always starts on a "1". Making the phrase longer
  fills the new bars with copies of the ones already there, so the pattern
  sounds the same until you edit them; making it shorter drops the trailing
  bars. The screen follows the clock: as the phrase moves on to bar 2, 3,
  ... the grid switches to that bar, and the playhead column shows. Tap a
  **BAR** button to pick a bar and stay on it (the padlock closes); the bar
  being played is then outlined green while you edit another. The padlock
  button toggles between following the clock and holding the shown bar.
- Tap a cell to set or clear a step with the track's note and velocity. Tap
  the note name on the left to select a track; `-`/`+` (hold to repeat)
  change its note and the velocity new steps get. The eight tracks are
  polyphonic voices of one instrument: they all play on the lead's MIDI
  channel, which is set on the SETUP tab.
- **Velocity**: put the finger on a step and drag up or down; the step's
  velocity follows (the full range is about half the screen height of
  travel). The shade of the cell shows it: full colour at 127, a third as
  bright at 1, the tail of a long note darker still. Dragging up or down
  never clears the step, only a tap does.
- **Longer notes**: put the finger down on the step and drag to the right;
  the note lasts up to the step you let go on. Its steps are drawn joined,
  the head in the track colour, the rest darker. Hold the finger at the
  right edge of the grid for a moment and the next bar slides in (the
  screen locks to it) with the note running into it; drag back to where it
  should end, or hold at the edge again for one more bar. Holding at the
  left edge goes back. A note may cross as many bars as the phrase has and
  wrap around its end. A note that goes on into the next bar has a flat
  right edge; in that bar it starts with a flat left edge, and dragging any
  of its steps there sets its end again. Tapping inside a note starts a new
  note on that step and cuts the old one; a drag stops short of the next
  note on the track.
- **RANDOM** and **CNTRPT** write a new phrase in the key (they are grey
  until a key is known; see the KEY tab). Both first make the eight tracks
  the notes of the key upwards from the tonic at middle C, so the grid is a
  diatonic piano roll, and clear the phrase. **RANDOM** then throws notes of
  the key at the grid: a loose rhythm from 16ths to half notes with rests,
  now and then a third or a fifth on top, velocities scattered around the
  track's. **CNTRPT** composes a line by the rules of species counterpoint:
  quarters and halves with a few eighths, ending on a long note; starts and
  ends on the tonic, the last step a leading tone or supertonic; mostly
  stepwise, leaps rare and answered by a step back, no sevenths, tritones
  or repeated notes, one climax; and, when the bass line is in the key, only
  consonances against the bass note sounding on each step, imperfect ones
  preferred, no parallel fifths or octaves, contrary motion preferred, no
  crossing. Velocities follow the beat (downbeat loudest). Every press gives
  a different line; nothing is undone but the phrase it replaces.
- The sequencer state also has play/mute, per-step editing and four patterns
  (`A`-`D`), but the screen has no buttons for them yet; pattern A is used.
- Default tracks are General MIDI drums on channel 10 (kick, snare, hats,
  clap, toms, crash). Patterns are saved to `sequencer.file` a second after
  the last edit and loaded at start (a file from when every track had its
  own channel loads with the first track's channel as the lead's).
- Notes go out on the same rawmidi port as the clock, from their own
  SCHED_FIFO thread (`sequencer_priority`), scheduled from the grid with
  `midi.lead_ms` (+ `sequencer.lead_ms`) lead. Note-offs sit on the grid at
  the end of the note, so a long note follows tempo corrections; a
  retriggered note is released first. Stop, or an unplugged screen,
  never leaves notes hanging.
- No screen? The UI just logs "continuing without the screen". Headless
  `--mode manual` runs a fixed clock at `manual_bpm`. `--no-ui` disables it.
  No interface? `--no-audio` (or `audio.enabled = false`) runs UI and MIDI only.
- `midi-clock --ui-snapshot shot.ppm --ui-size 800x480` renders the layout
  off-screen for any display size (writes the tempo, manual, lead,
  bass, drums, setup and key pages).

### Bass line (BASS tab)

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]
 LEN [1] [2] [4] [8]   BAR [1] [2] [3] [4] [5] [6] [7] [8] [🔒]
 [▲] G1  □ □ □ □ □ □ □ □ □ □ □ □ □ □ □ □   eight semitones, 16 steps of the shown bar
     ...                                    lowest note at the bottom
 [▼] C1  ■ □ □ ■ □ □ □ □ □ □ ■ □ □ □ □ □
 [-] VEL 100 [+]            [RANDOM] [CNTRPT]
```

- A single voice: each step holds one note or nothing, and a new note always
  ends the one before it. Tap a cell to put that note on the step; tap it
  again to clear the step; tapping another row moves the note. Drag to the
  right for a longer note, exactly as on the LEAD tab, bar edges
  included; a long note rings until the next note at the latest.
- **LEN**, **BAR** and the padlock work as on the LEAD tab; the bass
  phrase has its own length and its own lock, so a 2-bar drum pattern can run
  under an 8-bar bass line.
- The grid shows eight semitones. Drag up or down on an empty cell (or on
  the note names) to scroll the window with the finger; the **▲ ▼**
  buttons left of the note names step it one semitone at a time (hold to
  repeat). Notes stay where they are; a note outside the shown range is
  marked by a thin bar at the top or bottom edge of its step.
- **Velocity**: every note has its own. Drag up or down on a note (its head
  or its tail) and its velocity follows, the shade of the note shows it, as
  on the LEAD tab. **VEL** is the velocity new notes get; the MIDI channel
  is set on the SETUP tab.
- **RANDOM** and **CNTRPT** write a new bass line in the key, as on the LEAD
  tab: notes of the key in the octave up from the tonic (the tonic between
  G0 and F#1; the window scrolls so it is the bottom row), and CNTRPT keeps
  below and consonant with the lead where the lead's notes are in the key.
  A line written with CNTRPT under a lead written with CNTRPT is two-part
  counterpoint; the other way round works too.
- The bass line is saved in `sequencer.file` with the drum patterns.

### Drum kit (DRUMS tab)

A second drum sequencer, worked the way a DrumBrute is: you pick an
instrument on its pad and the 16 step buttons underneath are that
instrument's steps; the pads play when you hit them; steps can be
accented (the drums have no key and no per-step velocity: a hit is plain or
accented, as on the machine); the kit has swing.

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]
 LEN [1] [2] [4] [8]   BAR [1] [2] [3] [4] [5] [6] [7] [8] [🔒]
 [  KICK  ] [ SNARE ] [  RIM  ] [ CLAP  ] [ CHAT  ]   ten pads, each with a row of
 [·········] [·········] ...                          dots: its steps in the shown bar
 [  OHAT  ] [ LTOM  ] [ HTOM  ] [ CRASH ] [ RIDE  ]
 [·········] ...
  [1] [2] [3] [4] [5] [6] [7] [8] [9] [10] [11] [12] [13] [14] [15] [16]   the selected pad's steps
 [-] NOTE C1 [+]   [-] VEL 100 [+]   [-] SWING 50 [+]
```

- **Pads**: tap a pad to select the instrument (outlined in its colour) and
  hear it at once, clock running or not. The pad shows the General MIDI
  name of its note (KICK, SNARE, CHAT, ...; the note name for anything
  else) and lights while its step plays. The kit sits on one MIDI channel
  (set on the SETUP tab); **NOTE** and **VEL** are the selected
  instrument's note and velocity, so the kit can drive any drum machine or
  sampler.
- **Steps**: the row of 16 is the selected instrument in the shown bar.
  A tap cycles a step off → hit (dim) → accented hit (bright, velocity
  127) → off, like the LEDs on the machine. The playhead step is white.
  The row of dots on every pad is the same information for all
  instruments at once, so the whole pattern stays in view.
- The kit's state also has per-instrument mute and solo, clearing an
  instrument, and live recording of pad taps onto the nearest 16th, but the
  screen has no buttons for them.
- **SWING** delays every second 16th of the kit: 50 is straight, 67 the
  triplet feel, 75 half a step late (the DrumBrute range). It applies to
  the drum kit only, not to the LEAD or BASS phrases, and the swung
  hits follow tempo corrections like everything else.
- **LEN**, **BAR** and the padlock work as on the other tabs, with the
  kit's own phrase length and lock. Hits are one 16th long (no note
  lengths, as on the machine); the kit is saved in `sequencer.file` with
  the patterns and the bass line.

### MIDI channels (SETUP tab)

```
 [TEMPO] [KEY] [LEAD] [BASS] [DRUMS] [SETUP]
 LEAD    [-]      CH 10      [+]
 BASS    [-]      CH 1       [+]
 DRUMS   [-]      CH 10      [+]
```

- Every instrument the sequencer plays with its MIDI channel: the lead
  (all eight tracks together), the bass line and the drum kit. `-`/`+`
  (hold to repeat) move the channel through 1..16; the change is heard at
  once, a note already sounding is still released on the channel it
  started on, and it is saved with the patterns.
- Every channel is only set here. The lead's channel belongs to the
  pattern, so each of the four patterns can have its own.

### Working on the UI without the Pi

```sh
cargo run --release -p uisim -- --drummer          # 1280x800 window, virtual drummer at 118 BPM
cargo run --release -p uisim -- --size 800x480 --mode manual
```

`uisim` opens a desktop window (X11 or Wayland) running the exact screen code
of the Pi: the mouse is the finger, and a virtual drummer feeds hops to the
real tracker so AUTO mode locks, coasts and stops as on stage. The real
sequencer thread runs too; `--print-notes` logs its notes to stderr and
`--patterns FILE` picks the pattern file (default `patterns.toml` in the
current directory). Keys: `D` drummer on/off, `[` / `]` drummer tempo, `J`
timing jitter, `K` cycle the key the imaginary band plays in (none, a sure
C major, A minor, a doubtful G, a hopeless F# minor) so the KEY tab and the
generators can be tried, `Q` quit.

To drive real synths and drum machines on the same machine, send the MIDI
for real:

```sh
cargo run --release -p uisim -- --drummer --midi virtual
```

This runs the clock thread from the application and creates an ALSA
sequencer port `midi-clock:out` that carries clock, Start/Stop and the
sequencer's notes. `aseqdump -p midi-clock:0` prints what goes out,
`aconnect midi-clock:0 <client:port>` wires it to any sequencer client, and
`--connect NAME` does that at start (`NAME` is a client name or part of it,
or `CLIENT:PORT` numbers). `--midi virtual:NAME` names the client
differently, `--midi hw:1,0,0` uses a rawmidi port instead. The screen's
tick counter and lateness then show the real clock thread.

Bitwig on Linux only lists rawmidi devices, not sequencer ports. The
`snd-virmidi` kernel module provides devices that are both, so wire the
simulator to one of them:

```sh
sudo modprobe snd-virmidi midi_devs=1        # once per boot; a file in
                                              # /etc/modules-load.d makes it permanent
cargo run --release -p uisim -- --drummer --midi virtual --connect virmidi
```

Bitwig then shows a *VirMIDI* device: add a *Generic MIDI Keyboard* (notes)
or a *Generic Controller* set up as a clock slave and pick it as the input.
The screen lives in `crates/ui/src/screen.rs`
(layout, colours, `View`), the touch/redraw logic in `Session` in
`crates/ui/src/lib.rs`, the sequencer model in `crates/sequencer`; edit,
rerun, then `rsync` to the Pi and rebuild there.

The service unit takes over `tty1` (no login prompt there; use tty2) so the
text console cannot draw over the screen; `os-tuning.sh` disables console
blanking.

### Trying the screen without the audio interface

Nothing else is needed for this: no interface, no tuning script, no service.

1. Flash **Raspberry Pi OS Lite (64-bit)** with the Pi Imager, enable SSH and
   set a user in its settings. Connect the screen and boot.
   - The official DSI display and most HDMI panels with a USB touch controller
     work out of the box. If an HDMI panel shows the wrong resolution, add
     `video=HDMI-A-1:800x480@60D` (your panel's numbers) to the end of the line
     in `/boot/firmware/cmdline.txt`.
   - **Joy-it RB-LCD-10-2** (10.1", 1280x800): the Pi 4 rejects the panel's own
     timing (odd horizontal total) and falls back to 1080p, which the panel
     scales. Native resolution needs a patched EDID: see
     `deploy/edid/README.md`.
   - Small SPI panels need their vendor overlay in `config.txt` first; they
     then show up as `/dev/fb0` or `/dev/fb1` (set `framebuffer` accordingly).
2. On the Pi:
   ```sh
   sudo apt install -y git build-essential pkg-config libasound2-dev
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && . ~/.cargo/env
   sudo usermod -aG video,input,tty $USER      # then log out and in again
   ```
   Copy the repository over (`rsync -a --exclude target midi-clock/ pi:midi-clock/`)
   and build: `cd midi-clock && cargo build --release` (5–10 min on a Pi 4).
3. Check what the program sees:
   ```sh
   ./target/release/midi-clock --list-devices
   ```
   It lists `/dev/fb0` with resolution and depth, and the touch panel with
   its name. No touch device: check the `input` group and `ls /dev/input`.
4. Run it from the SSH session:
   ```sh
   sudo ./target/release/midi-clock --no-audio --no-midi --mode manual
   ```
   `sudo` lets it switch tty1 to graphics mode; without it the login prompt's
   cursor keeps blinking over the UI, which is harmless for a test. The screen
   should show the MANUAL page. Every touch draws a yellow marker where it was
   registered: if the marker is mirrored or rotated relative to your finger,
   put `touch_swap_xy` / `touch_invert_x` / `touch_invert_y` (or `rotation`)
   into a `config.toml` and pass `--config config.toml`.
5. Tap TAP four times: the state goes COUNT-IN, then RUNNING, the beat lamps
   walk, the big number shows your tempo, and the SSH status line agrees. Try
   +5/-5 (hold for repeat), STOP, START, and the AUTO tab (it will say
   LISTENING, there is no microphone). Ctrl-C ends it and clears the screen.

With the interface attached later, drop `--no-audio --no-midi` and follow the
bring-up list above.

## Offline evaluation

```sh
eval synth --bpm 120 --seconds 40 --out test-data/generated/steady.wav
eval synth --bpm 100 --ramp-to 118 --ramp-start 12 --ramp-len 12 --out test-data/generated/ramp.wav
eval run test-data/generated/ramp.wav --ann test-data/generated/ramp.beats
eval run recording.wav --ann recording.beats --set kp=0.5 --set history_s=3
```

`eval run` simulates the clock thread and scores the *emitted* beat times
(the ones the click and MIDI clock would have used) against the annotations.
It also prints the measured `onset_offset_ms` for your material. Current
numbers on synthetic kits (median error of emitted beats vs truth):

| Material | Median | p90 | Lock |
|---|---|---|---|
| Steady 120 BPM rock pattern | 3.8 ms | 8.5 ms | 2.0 s |
| 92 BPM with 8 ms human jitter | 5.8 ms | 24 ms | 2.0 s |
| Ramp 100 → 118 BPM over 12 s | 6.6 ms | 22 ms | 2.4 s |
| 128 BPM with a 4 s break | 3.7 ms | 8.8 ms | 1.9 s |

Record real drummers through the intended mic, annotate beats, and tune with
`--set` until the numbers hold; then copy the values into the config.

## Development notes

- All timing is CLOCK_MONOTONIC in f64 seconds. Capture buffers are timestamped
  from `snd_pcm_delay`, playback buffers from the delay before the write.
- The audio thread never allocates or locks. Hops go to the tracker through a
  lock-free ring; the grid comes back through an `ArcSwap`.
- On a machine without `libasound2-dev` you can still build by pointing
  `PKG_CONFIG_PATH` at a directory with an `alsa.pc` that links the runtime
  `libasound.so.2`.
- `cargo test --workspace` covers format conversion, sleep accuracy, onset
  detection on a click train, tempo/phase estimation, PLL lock, ramp following,
  stop after silence, click placement, clock statistics, tap-tempo rules,
  framebuffer pixel packing / rotation, the screen layout at several sizes,
  sequencer editing / persistence and the sequencer thread's note timing.
- The UI uses `embedded-graphics` for primitives and the bitmap font (scaled
  in software for the big tempo readout); no other graphics dependency.
