//! TOML configuration. Every field has a default so a partial file works.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub audio: Audio,
    pub onset: Onset,
    pub tempo: Tempo,
    pub midi: Midi,
    pub click: Click,
    pub realtime: Realtime,
    pub ui: Ui,
    pub sequencer: Sequencer,
    pub key: KeyDet,
}

/// Key detection from the microphone (`crates/key`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyDet {
    pub enabled: bool,
    /// Integration time of the chroma (seconds).
    pub window_s: f32,
    /// A new key must win this long before it is shown (seconds).
    pub hold_s: f32,
    /// "temperley" or "krumhansl".
    pub profile: String,
    pub fmin_hz: f32,
    pub fmax_hz: f32,
    /// Stationary-noise (room modes, hum) removal time constant (seconds).
    pub background_s: f32,
    /// A4 in Hz; 0 = estimate the band's tuning from the music.
    pub tuning_hz: f32,
    /// Below this level (dBFS) the input counts as silence.
    pub gate_dbfs: f32,
    /// After this much silence the key is forgotten (seconds).
    pub silence_reset_s: f32,
}

impl Default for KeyDet {
    fn default() -> Self {
        let d = key::KeyConfig::default();
        Self {
            enabled: true,
            window_s: d.window_s,
            hold_s: d.hold_s,
            profile: "temperley".into(),
            fmin_hz: d.fmin_hz,
            fmax_hz: d.fmax_hz,
            background_s: d.background_s,
            tuning_hz: 0.0,
            gate_dbfs: d.gate_dbfs,
            silence_reset_s: d.silence_reset_s,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Sequencer {
    pub enabled: bool,
    /// Patterns are saved here after every edit.
    pub file: String,
    /// Extra lead for sequencer notes on top of `midi.lead_ms` (ms).
    pub lead_ms: f64,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self { enabled: true, file: "patterns.toml".into(), lead_ms: 0.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// false = run without any audio interface (UI + MIDI only).
    pub enabled: bool,
    pub capture_device: String,
    pub playback_device: String,
    pub sample_rate: u32,
    pub period_frames: usize,
    pub periods: u32,
    pub capture_channels: usize,
    pub playback_channels: usize,
    /// Which capture channel carries the microphone (0-based).
    pub mic_channel: usize,
    /// Capture channel for key detection (0-based); negative = the mic.
    pub key_channel: i64,
    /// Playback channels that receive the click.
    pub click_channels: Vec<usize>,
    /// Mix the microphone into the playback channels at this gain (0 = off).
    pub monitor_gain: f32,
    /// Latency not reported by ALSA (USB, converters), measured with `clockgen latency`.
    pub extra_input_latency_ms: f64,
    pub extra_output_latency_ms: f64,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            enabled: true,
            capture_device: "hw:CARD=US2x2".into(),
            playback_device: "hw:CARD=US2x2".into(),
            sample_rate: 48000,
            period_frames: 64,
            periods: 3,
            capture_channels: 2,
            playback_channels: 2,
            mic_channel: 0,
            key_channel: -1,
            click_channels: vec![0, 1],
            monitor_gain: 0.0,
            extra_input_latency_ms: 1.0,
            extra_output_latency_ms: 1.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Onset {
    pub frame_size: usize,
    pub hop: usize,
    pub kick_band_hz: (f32, f32),
    pub hi_band_hz: (f32, f32),
    pub w_flux: f32,
    pub w_kick: f32,
    pub w_hi: f32,
    pub agc_tau_s: f32,
    pub threshold_ratio: f32,
    pub min_onset_gap_ms: f32,
    pub gate_dbfs: f32,
}

impl Default for Onset {
    fn default() -> Self {
        let d = onset::OnsetConfig::default();
        Self {
            frame_size: d.frame_size,
            hop: d.hop,
            kick_band_hz: d.kick_band_hz,
            hi_band_hz: d.hi_band_hz,
            w_flux: d.w_flux,
            w_kick: d.w_kick,
            w_hi: d.w_hi,
            agc_tau_s: d.agc_tau_s,
            threshold_ratio: d.threshold_ratio,
            min_onset_gap_ms: d.min_onset_gap_s * 1000.0,
            gate_dbfs: d.gate_dbfs,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Tempo {
    pub bpm_min: f64,
    pub bpm_max: f64,
    pub bpm_center: f64,
    pub bpm_width_octaves: f64,
    pub history_s: f64,
    pub estimate_interval_s: f64,
    pub min_tempo_strength: f64,
    pub tempo_hysteresis: f64,
    pub kp: f64,
    pub ki: f64,
    pub period_blend: f64,
    pub accept_window: f64,
    pub max_phase_slew: f64,
    pub lock_estimates: usize,
    pub coast_beats: u32,
    pub stop_after_s: f64,
    /// Delay between a real onset and its ODF peak; measured with `eval run --ann`.
    pub onset_offset_ms: f64,
    pub bar_len: u32,
    pub full_weight_strength: f64,
}

impl Default for Tempo {
    fn default() -> Self {
        let d = tracker::TrackerConfig::default();
        Self {
            bpm_min: d.bpm_min,
            bpm_max: d.bpm_max,
            bpm_center: d.bpm_center,
            bpm_width_octaves: d.bpm_width_octaves,
            history_s: d.history_s,
            estimate_interval_s: d.estimate_interval_s,
            min_tempo_strength: d.min_tempo_strength,
            tempo_hysteresis: d.tempo_hysteresis,
            kp: d.kp,
            ki: d.ki,
            period_blend: d.period_blend,
            accept_window: d.accept_window,
            max_phase_slew: d.max_phase_slew,
            lock_estimates: d.lock_estimates,
            coast_beats: d.coast_beats,
            stop_after_s: d.stop_after_s,
            onset_offset_ms: 7.0,
            bar_len: d.bar_len,
            full_weight_strength: d.full_weight_strength,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Midi {
    pub enabled: bool,
    /// Rawmidi port such as `hw:CARD=US2x2,0,0`, or `virtual` / `virtual:NAME`
    /// for an ALSA sequencer port that other programs on the machine can subscribe to.
    pub device: String,
    /// Send every tick this much early.
    pub lead_ms: f64,
    pub send_start_stop: bool,
}

impl Default for Midi {
    fn default() -> Self {
        Self { enabled: true, device: "hw:CARD=US2x2,0,0".into(), lead_ms: 1.5, send_start_stop: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Click {
    pub enabled: bool,
    pub freq_hz: f32,
    pub accent_freq_hz: f32,
    pub duration_ms: f32,
    pub gain: f32,
    pub accent_gain: f32,
    pub lead_ms: f64,
}

impl Default for Click {
    fn default() -> Self {
        let d = click::ClickConfig::default();
        Self {
            enabled: true,
            freq_hz: d.freq_hz,
            accent_freq_hz: d.accent_freq_hz,
            duration_ms: d.duration_s * 1000.0,
            gain: d.gain,
            accent_gain: d.accent_gain,
            lead_ms: 0.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Realtime {
    pub audio_priority: i32,
    pub midi_priority: i32,
    pub sequencer_priority: i32,
    pub tracker_priority: i32,
    /// CPU to pin the audio and MIDI threads to; negative = no pinning.
    pub cpu: i32,
    pub lock_memory: bool,
}

impl Default for Realtime {
    fn default() -> Self {
        Self { audio_priority: 80, midi_priority: 85, sequencer_priority: 84, tracker_priority: 60, cpu: -1, lock_memory: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    /// Touch-screen front end on the Linux framebuffer (no window system needed).
    pub enabled: bool,
    pub framebuffer: String,
    /// evdev device of the touch panel; empty = autodetect.
    pub touch_device: String,
    pub touch_swap_xy: bool,
    pub touch_invert_x: bool,
    pub touch_invert_y: bool,
    /// Take the touch device exclusively.
    pub touch_grab: bool,
    /// Screen rotation in degrees (0, 90, 180, 270).
    pub rotation: u32,
    pub fps: f64,
    /// Virtual console switched to graphics mode while running; empty = none.
    pub tty: String,
    /// Touch-panel latency subtracted from tap times.
    pub tap_offset_ms: f64,
    /// "auto" or "manual".
    pub start_mode: String,
    /// Tempo the manual clock starts with.
    pub manual_bpm: f64,
    pub tap_timeout_s: f64,
}

impl Default for Ui {
    fn default() -> Self {
        let d = ui::UiConfig::default();
        Self {
            enabled: true,
            framebuffer: d.framebuffer,
            touch_device: d.touch_device,
            touch_swap_xy: false,
            touch_invert_x: false,
            touch_invert_y: false,
            touch_grab: true,
            rotation: 0,
            fps: d.fps,
            tty: d.tty,
            tap_offset_ms: d.tap_offset_s * 1000.0,
            start_mode: "auto".into(),
            manual_bpm: 120.0,
            tap_timeout_s: 2.0,
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn pcm(&self) -> rtio::PcmConfig {
        let a = &self.audio;
        rtio::PcmConfig {
            capture_device: a.capture_device.clone(),
            playback_device: a.playback_device.clone(),
            sample_rate: a.sample_rate,
            period_frames: a.period_frames,
            periods: a.periods,
            capture_channels: a.capture_channels,
            playback_channels: a.playback_channels,
            extra_input_latency: a.extra_input_latency_ms / 1000.0,
            extra_output_latency: a.extra_output_latency_ms / 1000.0,
        }
    }

    pub fn onset(&self) -> onset::OnsetConfig {
        let o = &self.onset;
        onset::OnsetConfig {
            sample_rate: self.audio.sample_rate as f32,
            frame_size: o.frame_size,
            hop: o.hop,
            kick_band_hz: o.kick_band_hz,
            hi_band_hz: o.hi_band_hz,
            w_flux: o.w_flux,
            w_kick: o.w_kick,
            w_hi: o.w_hi,
            agc_tau_s: o.agc_tau_s,
            threshold_ratio: o.threshold_ratio,
            min_onset_gap_s: o.min_onset_gap_ms / 1000.0,
            gate_dbfs: o.gate_dbfs,
        }
    }

    pub fn tracker(&self) -> tracker::TrackerConfig {
        let t = &self.tempo;
        tracker::TrackerConfig {
            hop_dt: self.onset.hop as f64 / self.audio.sample_rate as f64,
            bpm_min: t.bpm_min,
            bpm_max: t.bpm_max,
            bpm_center: t.bpm_center,
            bpm_width_octaves: t.bpm_width_octaves,
            history_s: t.history_s,
            estimate_interval_s: t.estimate_interval_s,
            min_tempo_strength: t.min_tempo_strength,
            tempo_hysteresis: t.tempo_hysteresis,
            kp: t.kp,
            ki: t.ki,
            period_blend: t.period_blend,
            accept_window: t.accept_window,
            max_phase_slew: t.max_phase_slew,
            lock_estimates: t.lock_estimates,
            coast_beats: t.coast_beats,
            stop_after_s: t.stop_after_s,
            onset_offset_s: t.onset_offset_ms / 1000.0,
            bar_len: t.bar_len,
            full_weight_strength: t.full_weight_strength,
        }
    }

    pub fn key(&self) -> key::KeyConfig {
        let k = &self.key;
        key::KeyConfig {
            sample_rate: self.audio.sample_rate as f32,
            window_s: k.window_s,
            hold_s: k.hold_s,
            profile: if k.profile.eq_ignore_ascii_case("krumhansl") { key::Profile::Krumhansl } else { key::Profile::Temperley },
            fmin_hz: k.fmin_hz,
            fmax_hz: k.fmax_hz,
            background_s: k.background_s,
            tuning_hz: (k.tuning_hz > 0.0).then_some(k.tuning_hz),
            gate_dbfs: k.gate_dbfs,
            silence_reset_s: k.silence_reset_s,
            ..key::KeyConfig::default()
        }
    }

    pub fn click(&self) -> click::ClickConfig {
        let c = &self.click;
        click::ClickConfig {
            sample_rate: self.audio.sample_rate as f32,
            freq_hz: c.freq_hz,
            accent_freq_hz: c.accent_freq_hz,
            duration_s: c.duration_ms / 1000.0,
            gain: c.gain,
            accent_gain: c.accent_gain,
            lead_s: c.lead_ms / 1000.0,
        }
    }

    pub fn midiclock(&self) -> midiclock::ClockConfig {
        midiclock::ClockConfig { lead_s: self.midi.lead_ms / 1000.0, send_start_stop: self.midi.send_start_stop }
    }

    pub fn ui(&self) -> ui::UiConfig {
        let u = &self.ui;
        ui::UiConfig {
            framebuffer: u.framebuffer.clone(),
            touch_device: u.touch_device.clone(),
            touch: ui::TouchConfig {
                swap_xy: u.touch_swap_xy,
                invert_x: u.touch_invert_x,
                invert_y: u.touch_invert_y,
                grab: u.touch_grab,
            },
            rotation: u.rotation,
            fps: u.fps,
            tty: u.tty.clone(),
            tap_offset_s: u.tap_offset_ms / 1000.0,
        }
    }

    pub fn manual(&self) -> tracker::ManualConfig {
        tracker::ManualConfig {
            bpm_min: 30.0,
            bpm_max: 300.0,
            tap_timeout_s: self.ui.tap_timeout_s,
            max_taps: 8,
            start_delay_s: 0.15,
            bar_len: self.tempo.bar_len,
        }
    }
}
