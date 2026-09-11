//! Raw ALSA duplex PCM with per-period timestamps.
//!
//! Capture and playback are independent `hw:` streams on the same interface.
//! They are not linked: instead every buffer is timestamped on the shared
//! monotonic clock using `snd_pcm_delay`, so the beat grid, the click and the
//! MIDI clock all agree on when a sample was captured or will be played.

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{anyhow, Context, Result};

use crate::time::now;

#[derive(Clone, Debug)]
pub struct PcmConfig {
    pub capture_device: String,
    pub playback_device: String,
    pub sample_rate: u32,
    pub period_frames: usize,
    pub periods: u32,
    pub capture_channels: usize,
    pub playback_channels: usize,
    /// USB/converter latency not visible to ALSA, added to capture timestamps (seconds).
    pub extra_input_latency: f64,
    /// USB/converter latency not visible to ALSA, added to playback timestamps (seconds).
    pub extra_output_latency: f64,
}

impl Default for PcmConfig {
    fn default() -> Self {
        Self {
            capture_device: "hw:0".into(),
            playback_device: "hw:0".into(),
            sample_rate: 48000,
            period_frames: 64,
            periods: 3,
            capture_channels: 2,
            playback_channels: 2,
            extra_input_latency: 0.0,
            extra_output_latency: 0.0,
        }
    }
}

/// Sample formats we can convert to/from f32 without ALSA plugins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    S32,
    S24In32,
    S24Packed,
    S16,
    F32,
}

impl SampleFormat {
    const PREFERENCE: [SampleFormat; 5] = [
        SampleFormat::S32,
        SampleFormat::S24In32,
        SampleFormat::S24Packed,
        SampleFormat::F32,
        SampleFormat::S16,
    ];

    fn alsa(self) -> Format {
        match self {
            SampleFormat::S32 => Format::S32LE,
            SampleFormat::S24In32 => Format::S24LE,
            SampleFormat::S24Packed => Format::S243LE,
            SampleFormat::S16 => Format::S16LE,
            SampleFormat::F32 => Format::FloatLE,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::S32 | SampleFormat::S24In32 | SampleFormat::F32 => 4,
            SampleFormat::S24Packed => 3,
            SampleFormat::S16 => 2,
        }
    }

    #[inline]
    fn decode(self, b: &[u8]) -> f32 {
        match self {
            SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0,
            SampleFormat::S24In32 => {
                let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                ((v << 8) >> 8) as f32 / 8388608.0
            }
            SampleFormat::S24Packed => {
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                v as f32 / 8388608.0
            }
            SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }

    #[inline]
    fn encode(self, v: f32, out: &mut [u8]) {
        let v = v.clamp(-1.0, 1.0);
        match self {
            SampleFormat::S32 => {
                let i = (v as f64 * 2147483647.0) as i32;
                out[..4].copy_from_slice(&i.to_le_bytes());
            }
            SampleFormat::S24In32 => {
                let i = (v * 8388607.0) as i32;
                out[..4].copy_from_slice(&i.to_le_bytes());
            }
            SampleFormat::S24Packed => {
                let i = (v * 8388607.0) as i32;
                let b = i.to_le_bytes();
                out[..3].copy_from_slice(&b[..3]);
            }
            SampleFormat::S16 => {
                let i = (v * 32767.0) as i16;
                out[..2].copy_from_slice(&i.to_le_bytes());
            }
            SampleFormat::F32 => out[..4].copy_from_slice(&v.to_le_bytes()),
        }
    }
}

pub struct Stream {
    pcm: PCM,
    dir: Direction,
    format: SampleFormat,
    channels: usize,
    period: usize,
    periods: u32,
    rate: f64,
    buf: Vec<u8>,
    xruns: u64,
    extra_latency: f64,
}

impl Stream {
    fn open(name: &str, dir: Direction, cfg: &PcmConfig, channels: usize, extra_latency: f64) -> Result<Self> {
        let pcm = PCM::new(name, dir, false).with_context(|| format!("opening {name} for {dir:?}"))?;
        let mut chosen = None;
        let mut last_err = None;
        for &fmt in &SampleFormat::PREFERENCE {
            match Self::configure(&pcm, dir, fmt, cfg, channels) {
                Ok(v) => {
                    chosen = Some(v);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let (format, channels, period, periods) =
            chosen.ok_or_else(|| anyhow!("no usable sample format on {name}: {:?}", last_err))?;
        let buf = vec![0u8; period * channels * format.bytes()];
        Ok(Self {
            pcm,
            dir,
            format,
            channels,
            period,
            periods,
            rate: cfg.sample_rate as f64,
            buf,
            xruns: 0,
            extra_latency,
        })
    }

    fn configure(pcm: &PCM, dir: Direction, fmt: SampleFormat, cfg: &PcmConfig, channels: usize) -> Result<(SampleFormat, usize, usize, u32)> {
        let hwp = HwParams::any(pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(fmt.alsa())?;
        let ch = hwp.set_channels_near(channels as u32)?;
        hwp.set_rate(cfg.sample_rate, ValueOr::Nearest)?;
        let rate = hwp.get_rate()?;
        if rate != cfg.sample_rate {
            anyhow::bail!("device does not support {} Hz (offers {rate})", cfg.sample_rate);
        }
        let period = hwp.set_period_size_near(cfg.period_frames as i64, ValueOr::Nearest)?;
        let periods = hwp.set_periods_near(cfg.periods, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
        let swp = pcm.sw_params_current()?;
        swp.set_avail_min(period)?;
        // Capture starts on the first read; playback starts once the prefill is in.
        let start = match dir {
            Direction::Capture => 1,
            Direction::Playback => period * (periods as i64 - 1).max(1),
        };
        swp.set_start_threshold(start)?;
        pcm.sw_params(&swp)?;
        Ok((fmt, ch as usize, period as usize, periods))
    }

    pub fn format(&self) -> SampleFormat {
        self.format
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn period(&self) -> usize {
        self.period
    }
    pub fn periods(&self) -> u32 {
        self.periods
    }
    pub fn xruns(&self) -> u64 {
        self.xruns
    }

    fn recover(&mut self, e: alsa::Error) -> Result<()> {
        self.xruns += 1;
        self.pcm.try_recover(e, true).context("pcm recover")
    }

    /// Read one period into `out` (interleaved f32, `period * channels` long).
    /// Returns the capture time of the last sample in the period.
    pub fn read(&mut self, out: &mut [f32]) -> Result<f64> {
        debug_assert_eq!(out.len(), self.period * self.channels);
        let frame_bytes = self.channels * self.format.bytes();
        let want = self.period;
        let mut got = 0usize;
        while got < want {
            let r = {
                let io = self.pcm.io_bytes();
                io.readi(&mut self.buf[got * frame_bytes..want * frame_bytes])
            };
            match r {
                Ok(n) => got += n,
                Err(e) => self.recover(e)?,
            }
        }
        let t = now();
        let delay = self.pcm.delay().unwrap_or(0).max(0) as f64;
        let last_sample_time = t - delay / self.rate - self.extra_latency;
        let fmt = self.format;
        let bytes = fmt.bytes();
        for (i, v) in out.iter_mut().enumerate() {
            *v = fmt.decode(&self.buf[i * bytes..i * bytes + bytes]);
        }
        Ok(last_sample_time)
    }

    /// Write one period of interleaved f32. Returns the playback time of the
    /// first sample in the period.
    pub fn write(&mut self, data: &[f32]) -> Result<f64> {
        debug_assert_eq!(data.len(), self.period * self.channels);
        let fmt = self.format;
        let bytes = fmt.bytes();
        for (i, &v) in data.iter().enumerate() {
            fmt.encode(v, &mut self.buf[i * bytes..i * bytes + bytes]);
        }
        let frame_bytes = self.channels * bytes;
        loop {
            let delay = match self.pcm.state() {
                State::Running | State::Prepared => self.pcm.delay().unwrap_or(0).max(0) as f64,
                _ => 0.0,
            };
            let t = now() + delay / self.rate + self.extra_latency;
            let r = {
                let io = self.pcm.io_bytes();
                io.writei(&self.buf[..self.period * frame_bytes])
            };
            match r {
                Ok(_) => return Ok(t),
                Err(e) => self.recover(e)?,
            }
        }
    }

    /// Estimated playback time of the next sample written (playback only).
    #[inline]
    pub fn next_write_time(&self) -> f64 {
        let delay = match self.pcm.state() {
            State::Running | State::Prepared => self.pcm.delay().unwrap_or(0).max(0) as f64,
            _ => 0.0,
        };
        now() + delay / self.rate + self.extra_latency
    }

    /// Queue `n` periods of silence (playback only).
    pub fn prefill_silence(&mut self, n: usize) -> Result<()> {
        let z = vec![0f32; self.period * self.channels];
        for _ in 0..n {
            self.write(&z)?;
        }
        Ok(())
    }

    pub fn start(&mut self) -> Result<()> {
        if self.pcm.state() != State::Running {
            self.pcm.start().context("pcm start")?;
        }
        Ok(())
    }

    pub fn state(&self) -> State {
        self.pcm.state()
    }

    pub fn direction(&self) -> Direction {
        self.dir
    }
}

pub struct Duplex {
    pub capture: Stream,
    pub playback: Stream,
    rate: u32,
}

impl Duplex {
    pub fn open(cfg: &PcmConfig) -> Result<Self> {
        let capture = Stream::open(&cfg.capture_device, Direction::Capture, cfg, cfg.capture_channels, cfg.extra_input_latency)?;
        let playback = Stream::open(&cfg.playback_device, Direction::Playback, cfg, cfg.playback_channels, cfg.extra_output_latency)?;
        if capture.period != playback.period {
            anyhow::bail!(
                "capture period {} != playback period {}; pick a period size both directions support",
                capture.period,
                playback.period
            );
        }
        Ok(Self { capture, playback, rate: cfg.sample_rate })
    }

    /// Prefill playback with `periods - 1` silent periods (which also starts
    /// it through the start threshold) and start capture.
    pub fn start(&mut self) -> Result<()> {
        let n = (self.playback.periods as usize).saturating_sub(1).max(1);
        self.playback.prefill_silence(n)?;
        self.playback.start()?;
        self.capture.start()?;
        Ok(())
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
    pub fn period(&self) -> usize {
        self.capture.period
    }
    pub fn xruns(&self) -> u64 {
        self.capture.xruns + self.playback.xruns
    }

    pub fn describe(&self) -> String {
        format!(
            "capture {}ch {:?} / playback {}ch {:?}, {} Hz, period {} frames x {} ({:.2} ms per period)",
            self.capture.channels,
            self.capture.format,
            self.playback.channels,
            self.playback.format,
            self.rate,
            self.period(),
            self.playback.periods,
            self.period() as f64 / self.rate as f64 * 1000.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip() {
        for fmt in SampleFormat::PREFERENCE {
            let mut buf = [0u8; 4];
            for &v in &[0.0f32, 0.5, -0.5, 0.999, -0.999] {
                fmt.encode(v, &mut buf);
                let back = fmt.decode(&buf);
                assert!((back - v).abs() < 1e-3, "{fmt:?}: {v} -> {back}");
            }
        }
    }
}

/// Human-readable list of sound cards with the `hw:CARD=...` name to use.
pub fn list_cards() -> Vec<String> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new().flatten() {
        let idx = card.get_index();
        let Ok(ctl) = alsa::Ctl::from_card(&card, false) else { continue };
        let Ok(info) = ctl.card_info() else { continue };
        out.push(format!(
            "card {idx}: hw:CARD={}  ({})",
            info.get_id().unwrap_or(""),
            info.get_longname().unwrap_or("")
        ));
    }
    out
}
