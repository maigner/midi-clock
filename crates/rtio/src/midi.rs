//! Raw MIDI output through ALSA rawmidi. Rawmidi is used instead of the ALSA
//! sequencer because it is the shortest path to the USB endpoint.

use alsa::rawmidi::Rawmidi;
use alsa::Direction;
use anyhow::{Context, Result};
use std::io::Write;
use std::sync::Mutex;

pub const CLOCK: u8 = 0xF8;
pub const START: u8 = 0xFA;
pub const CONTINUE: u8 = 0xFB;
pub const STOP: u8 = 0xFC;

/// Shared by the clock thread and the sequencer thread; every message is
/// written under a short lock so bytes of different messages never interleave.
pub struct MidiOut {
    rm: Mutex<Rawmidi>,
    name: String,
}

impl MidiOut {
    /// Open a rawmidi output such as `hw:1,0,0` or `hw:CARD=US2x2,0,0`.
    pub fn open(name: &str) -> Result<Self> {
        let rm = Rawmidi::new(name, Direction::Playback, false)
            .with_context(|| format!("opening MIDI output {name}"))?;
        Ok(Self { rm: Mutex::new(rm), name: name.to_string() })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Write bytes immediately. One byte for a clock tick, so this is called
    /// up to ~100 times per second from the clock thread.
    #[inline]
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let rm = self.rm.lock().unwrap_or_else(|e| e.into_inner());
        let mut io = rm.io();
        let mut off = 0;
        while off < bytes.len() {
            let n = io.write(&bytes[off..]).context("rawmidi write")?;
            if n == 0 {
                anyhow::bail!("rawmidi write returned 0 bytes");
            }
            off += n;
        }
        Ok(())
    }

    pub fn clock(&self) -> Result<()> {
        self.send(&[CLOCK])
    }
    pub fn start(&self) -> Result<()> {
        self.send(&[START])
    }
    pub fn stop(&self) -> Result<()> {
        self.send(&[STOP])
    }
    /// Note On, `channel` 0-based.
    pub fn note_on(&self, channel: u8, note: u8, velocity: u8) -> Result<()> {
        self.send(&[0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F])
    }
    pub fn note_off(&self, channel: u8, note: u8) -> Result<()> {
        self.send(&[0x80 | (channel & 0x0F), note & 0x7F, 0])
    }
}

/// Human-readable list of rawmidi output ports on all cards.
pub fn list_outputs() -> Vec<String> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new().flatten() {
        let idx = card.get_index();
        let ctl = match alsa::Ctl::from_card(&card, false) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let card_name = card.get_name().unwrap_or_default();
        for info in alsa::rawmidi::Iter::new(&ctl).flatten() {
            if info.get_stream() != Direction::Playback {
                continue;
            }
            out.push(format!(
                "hw:{},{},{}  {} / {}",
                idx,
                info.get_device(),
                info.get_subdevice(),
                card_name,
                info.get_id().unwrap_or_default()
            ));
        }
    }
    out
}
