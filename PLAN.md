# Live Beat → MIDI Master Clock — Project Plan

Goal: a Raspberry Pi with a USB audio interface (Tascam US-2x2 or similar class-compliant
device) listens to live music through a microphone, tracks the beat and tempo, and

1. sends MIDI clock (24 PPQN) + Start/Stop on the interface's MIDI OUT so sequencers and
   synths follow the human players, and
2. plays a click on the interface's audio output that lands exactly on the beat and the
   MIDI clock.

Hard requirement: no perceptible latency. Click and clock must sit *on* the beat, not behind it.

---

## 0. The one design decision that makes "no latency" possible

A beat detector that reacts to a drum hit is always late: the hit has to be captured
(USB + buffer, ≥ 2 ms), analysed (FFT hop, ≥ 5 ms) and only then can anything be sent.
A reactive design therefore lags 10–30 ms behind every beat, forever. That is the failure
mode to avoid.

The system must be **predictive**: it maintains an internal beat grid (period + phase),
runs the click and the MIDI clock *from that grid*, and uses detected onsets only to
*correct* the grid (a phase-locked loop). While the players hold a tempo, the click and
clock are on the beat with zero lag, because they are emitted before the hit is even
heard. Detection latency then only affects how fast the grid follows a tempo change,
not where the click lands.

Consequences that shape everything below:

- The beat tracker outputs `(next_beat_time, period, confidence)` on a monotonic clock,
  never "beat now".
- Click and clock generators are simple, high-priority timers driven from that grid.
- Known constant latencies (audio output buffer, USB MIDI, synth response) are
  compensated by emitting *early* by a measured, configurable offset.
- Honest limit: a sudden tempo change or a stop is noticed one to two beats late. Steady
  playing and gradual push/pull are tracked tightly. This is inherent to any audio-driven
  clock and must be communicated, not hidden.

---

## 1. Hardware and OS

| Item | Choice | Why |
|---|---|---|
| Computer | Raspberry Pi 4 (2 GB+) or Pi 5 | Enough CPU for FFT at 48 kHz with headroom; Pi 5 preferred. Pi Zero 2 W is marginal, avoid. |
| Interface | Tascam US-2x2 / US-2x2HR | USB Audio Class 2 compliant, MIDI I/O built in, works with `snd-usb-audio`. Any class-compliant interface with MIDI is fine. |
| Mic | Dynamic mic near the kit (kick/snare side) | Onset-rich, rejects room; phantom-powered condenser also fine. |
| OS | Raspberry Pi OS Lite 64-bit (Bookworm) | Headless, minimal. |
| Kernel | PREEMPT_RT build (Raspberry Pi ships RT kernel packages; verify name in step 1) | Sub-millisecond scheduling jitter for the clock thread. Stock kernel with `threadirqs` is the fallback. |
| Optional | Footswitch on GPIO or a MIDI pedal | Manual downbeat / tap tempo / start-stop override. |

System tuning checklist (applied by an install script):
- `performance` CPU governor, disable CPU idle states deep sleep.
- `isolcpus=3` (Pi 4/5 have 4 cores): pin audio + clock threads to an isolated core.
- Realtime limits in `/etc/security/limits.d/` (`rtprio 95`, `memlock unlimited`).
- USB: `usbcore.autosuspend=-1`, keep WiFi/Bluetooth off or on a different bus; disable
  HDMI/onboard audio so the Tascam is card 0.
- No desktop, no PipeWire/PulseAudio: talk to ALSA directly.

---

## 2. Software architecture

Language: **Rust** (toolchain already installed here). Reasons: no GC pauses, safe threads,
`alsa` crate exposes raw PCM and rawmidi, easy cross-compile for aarch64, and the project
is small enough that we own the whole real-time path. C++ would be an equal choice; the
decisive point is *raw ALSA*, not the language. No JACK, no PipeWire in the signal path.

```
                +-----------------------------+
 mic ──USB──▶   |  Audio thread (SCHED_FIFO)  |   ──USB──▶ headphones / PA
                |  ALSA PCM duplex, 48 kHz,   |
                |  64-frame periods           |
                |   ├─ onset detection        |
                |   └─ click renderer  ◀──────┼── beat grid
                +--------------┬--------------+
                               │ onset events (time, strength)
                               ▼
                +-----------------------------+
                |  Tracker thread             |
                |  tempo estimate + PLL       |
                |  → beat grid (t0, period,   |
                |    confidence, running)     |
                +--------------┬--------------+
                               │ shared, lock-free (atomic snapshot)
                               ▼
                +-----------------------------+
                |  MIDI clock thread          |   ──USB──▶ MIDI OUT
                |  (SCHED_FIFO, absolute      |
                |   clock_nanosleep)          |
                |  24 PPQN, Start/Stop        |
                +-----------------------------+
                +-----------------------------+
                |  Control thread             |  config file, status line / tiny web
                |  (normal priority)          |  status, footswitch, tap tempo
                +-----------------------------+
```

Crates: `alsa` (PCM + rawmidi), `realfft`/`rustfft`, `libc` (sched_setscheduler,
clock_nanosleep, mlockall), `crossbeam` or `ringbuf` (SPSC queues), `serde` + `toml`
(config), `hound` (WAV for offline tests), `clap`.

### 2.1 Audio thread
- ALSA `hw:` device, duplex, 48 kHz, S32/S24, period 64 frames, 2–3 periods.
  Target input→output round trip ≤ 6 ms (measured in step 1).
- Every period: push input into the onset detector, pull click samples from the click
  renderer, write output. No allocation, no locks, no logging in this thread.
- Timestamps: use `snd_pcm_status` htstamp/delay to map "sample n of this buffer" to
  `CLOCK_MONOTONIC`. This is the single time base for onset events, the beat grid, and
  the MIDI thread.

### 2.2 Onset detection (in the audio thread)
- Hop 64–128 samples, STFT 512 or 1024 with Hann window.
- Onset detection function: spectral flux (half-wave rectified, log-compressed) plus two
  band-limited energy detectors tuned for a drum kit: kick (40–120 Hz) and snare/hat
  (1–6 kHz). Weighted sum, adaptive-threshold peak picking.
- Output: `(time, strength)` events into an SPSC queue. Also ship the continuous ODF at
  hop rate to the tracker (needed for tempo autocorrelation).

### 2.3 Tracker thread (the heart)
Reference algorithms: BTrack (Stark/Davies/Plumbley 2009, "Real-time beat-synchronous
analysis") and Ellis 2007 dynamic programming; we implement our own so it can be
predictive and drum-aware.

- **Tempo estimation**: autocorrelation of the last ~6 s of the ODF, weighted by a prior
  over a configurable BPM range (default 60–200, user can narrow, e.g. 100–140 for a
  gig). Comb-filter scoring to resolve harmonics. Hysteresis: prefer the current tempo
  unless a competitor wins clearly for N beats. This is what prevents half/double-tempo
  flips, the classic failure.
- **Phase**: cross-correlate the ODF with a pulse train at the estimated period; pick
  the phase that best explains recent strong onsets.
- **PLL**: grid state `(t0, period)`. Each strong onset within ±25 % of a predicted beat
  yields a phase error. Apply gains: phase correction `k_p` (~0.2–0.4 per beat), period
  correction `k_i` (small). Ignore onsets far from the grid (fills, ghost notes) unless
  they persist. Bounded slew so the MIDI clock never jumps abruptly — sequencers hate
  that more than a few ms of drift.
- **Confidence**: agreement between recent onsets and grid. Low confidence → freeze
  period (coast), do not chase noise.
- **State machine**: `Idle → Locking → Running → Coasting → Stopped`.
  - Locking: needs ~4 beats of consistent onsets before it declares a tempo.
  - Running: normal.
  - Coasting: no onsets for > 2 beats (break, silence): keep clocking at the last tempo
    for a configurable time, then Stop.
- Optional downbeat: assume 4/4 and align bar phase to the strongest recurring kick
  onset or to a footswitch press. Stretch goal; beats first.

### 2.4 MIDI clock thread
- Reads the grid snapshot, computes absolute times for ticks `t0 + k·period/24`, sleeps
  with `clock_nanosleep(TIMER_ABSTIME)` and writes `0xF8` via ALSA rawmidi (not the
  sequencer API; rawmidi is the lowest-jitter path).
- Emits `0xFA` Start on entering Running (just before the first tick of the beat, so
  slaves begin on beat 1), `0xFC` Stop on leaving.
- Re-reads the grid every tick, so tempo changes are followed without discontinuity.
- Configurable `midi_lead_ms` (default ~2 ms) to absorb USB + slave latency.
- Target: tick jitter < 0.5 ms at the thread; USB MIDI framing adds ≤ 1 ms.

### 2.5 Click renderer
- Synthesised click (short sine burst, e.g. 1 kHz, 5 ms, exponential decay; accented
  1.5 kHz on downbeat when available). No sample files, no disk I/O.
- Rendered sample-accurately at `beat_time − output_latency`, where output latency is
  measured in step 1 and stored in config.
- Level and output channel selectable; optionally mix the mic through for monitoring.

### 2.6 Control / config
- `config.toml`: device names, sample rate, period, BPM range, PLL gains, offsets, click
  settings, silence timeout, behaviour on stop.
- Status: one refreshing terminal line (BPM, confidence, state, phase error ms) and a
  tiny HTTP/JSON status endpoint for a phone. No GUI.
- Inputs: footswitch (GPIO) or MIDI IN note: tap tempo, force downbeat, start/stop.

---

## 3. Work phases (each ends with a measurable result)

### Phase 1 — Prove the real-time path first (no beat tracking yet)
Build `clockgen`: fixed-BPM MIDI clock + click from the Pi through the Tascam.
- ALSA duplex at 64-frame periods running for hours without xruns on the RT kernel.
- MIDI tick timing logged and measured: jitter histogram (goal < 1 ms p99).
- Round-trip audio latency measured with a loopback cable (output → input), stored as
  `output_latency` in config.
- Cross-check the clock with a hardware sequencer / DAW as slave.
Exit criterion: a synth slaved to the Pi and the Pi's click sound like one machine.
If this phase fails, nothing else matters, so it comes first.

### Phase 2 — Beat tracker offline
Build `tracker` as a library plus a CLI that reads WAV files and writes beat times.
- Test corpus: own recordings of a drummer/band through the intended mic (steady, with
  push/pull, with fills, with a break and re-entry, with count-in), plus public sets
  with beat annotations (e.g. Ballroom, SMC) for regression.
- Metrics: F-measure at ±70 ms, continuity (CMLc/AMLt), median absolute phase error
  in ms, time-to-lock, and — most important for us — *predicted* beat error
  (beat time as predicted one beat ahead vs. annotation).
- Tune ODF weights, BPM prior, PLL gains on the corpus. Keep a `make eval` target so
  every change is scored.
Exit criterion: predicted-beat median error < 15 ms on steady drum recordings, no
tempo-octave flips over full takes, lock within 4 beats.

### Phase 3 — Integrate live
Wire the tracker into the real-time app: audio thread → tracker → grid → MIDI + click.
- Lock-free grid snapshot, RT-safe queues, priorities, CPU pinning.
- Hardware-in-the-loop test: play a drum machine at known tempo into the mic; record
  the Pi's click and MIDI clock alongside; measure phase offset and jitter in the
  recording. Then automate tempo ramps and sudden changes on the drum machine to
  characterise following behaviour.
Exit criterion: with a drum machine as "drummer", clock and click within ±10 ms of the
source beat while steady; recovers a 5 % tempo step within 2 beats.

### Phase 4 — Robustness and musician-facing behaviour
- Silence/break handling, Start/Stop policy, coasting, re-lock on re-entry.
- BPM range lock per song, tap-tempo / downbeat footswitch, tempo hold button.
- Watchdog: automatic restart of the service on xrun storms or device disconnect;
  hot-plug of the interface.
- Rehearsal with a real drummer: log everything, review the failures, tune.

### Phase 5 — Deployment
- `systemd` service, starts on boot in < 15 s, no login needed.
- Install script that applies the OS tuning from section 1.
- Read-only root filesystem (overlay) so power cuts do not corrupt the SD card.
- Cross-compilation from this workstation (`aarch64-unknown-linux-gnu`) and `scp`
  deploy; optional `cargo-deb` package.

---

## 4. Repository layout

```
midi-clock/
  Cargo.toml                 workspace
  crates/
    rtio/                    ALSA PCM duplex + rawmidi, RT thread helpers, timing
    onset/                   STFT, ODF, peak picking (no_std-friendly, allocation-free hot path)
    tracker/                 tempo estimation, PLL, state machine (pure, testable)
    click/                   click synthesis
    app/                     the binary: threads, config, status, footswitch
  tools/
    clockgen/                Phase 1 fixed-tempo generator + jitter logger
    eval/                    offline WAV evaluation, metrics, plots
  test-data/                 short annotated recordings (git-lfs or external)
  deploy/                    systemd unit, install.sh, config.example.toml, os-tuning.sh
  PLAN.md
```

---

## 5. Risks and how the plan handles them

| Risk | Mitigation |
|---|---|
| Xruns / timing jitter on the Pi | RT kernel, isolated core, raw ALSA, Phase 1 measures before anything else is built. |
| Half/double tempo errors | BPM range lock, tempo hysteresis, comb-filter harmonic scoring, evaluated on corpus. |
| Fills, breaks, ghost notes throw the phase | PLL only accepts onsets near predicted beats; confidence gating; coasting. |
| Bleed from the synths (which we are clocking) feeding back into the mic | Drum-band-weighted ODF; option to subtract our own click; mic placement; optional gate. |
| Tascam quirks on Linux | Class compliant, but verify sample-rate/format support and MIDI port in Phase 1 on the actual unit. |
| Sequencer behaviour on tempo changes | Smooth slew; never jump the clock; Start sent exactly before beat-1 tick. |
| Sudden tempo change lags 1–2 beats | Inherent; documented; footswitch tap/hold gives the musician manual override. |

---

## 6. First concrete steps

1. Set up the Pi: OS, RT kernel, tuning script, confirm the Tascam enumerates
   (`aplay -l`, `amidi -l`) at 48 kHz with 64-frame periods.
2. Create the Cargo workspace and the `rtio` crate; write `clockgen`.
3. Measure: xruns over 1 h, MIDI tick jitter, loopback latency. Record numbers in
   `deploy/config.example.toml`.
4. Record the test corpus with the intended mic and drummer.
5. Start `onset` + `tracker` with the offline `eval` tool.
