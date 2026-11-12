//! MIDI output and input, and a parser that cuts an input's byte stream
//! into messages. The output has two backends behind one type:
//!
//! * ALSA rawmidi (`hw:...`), the shortest path to a USB endpoint. This is
//!   what runs on the Pi.
//! * An ALSA sequencer client with one output port (`virtual` or
//!   `virtual:NAME`). Other programs on the same machine (Bitwig, a2jmidid,
//!   `aseqdump`) see the port and can subscribe to it, so the simulator can
//!   drive real synths without any hardware.

use alsa::rawmidi::Rawmidi;
use alsa::seq::{self, Seq};
use alsa::Direction;
use anyhow::{Context, Result};
use std::ffi::CString;
use std::io::Write;
use std::sync::Mutex;

pub const CLOCK: u8 = 0xF8;
pub const START: u8 = 0xFA;
pub const CONTINUE: u8 = 0xFB;
pub const STOP: u8 = 0xFC;

/// Client name used by `virtual` without an explicit name.
pub const DEFAULT_VIRTUAL_NAME: &str = "midi-clock";

enum Backend {
    Raw(Rawmidi),
    Seq { seq: Seq, port: i32 },
}

/// Shared by the clock thread and the sequencer thread; every message is
/// written under a short lock so bytes of different messages never interleave.
pub struct MidiOut {
    backend: Mutex<Backend>,
    name: String,
}

impl MidiOut {
    /// Open a rawmidi output such as `hw:1,0,0` or `hw:CARD=US2x2,0,0`, or
    /// create an ALSA sequencer port with `virtual` / `virtual:NAME`.
    pub fn open(name: &str) -> Result<Self> {
        if let Some(rest) = name.strip_prefix("virtual") {
            let client = match rest.strip_prefix(':') {
                Some(n) if !n.is_empty() => n,
                _ if rest.is_empty() => DEFAULT_VIRTUAL_NAME,
                _ => anyhow::bail!("MIDI output must be hw:..., virtual or virtual:NAME, got {name}"),
            };
            return Self::open_virtual(client);
        }
        let rm = Rawmidi::new(name, Direction::Playback, false)
            .with_context(|| format!("opening MIDI output {name}"))?;
        Ok(Self { backend: Mutex::new(Backend::Raw(rm)), name: name.to_string() })
    }

    /// Create an ALSA sequencer client called `client` with one output port
    /// called `out`. Shows up in `aconnect -o` and in DAWs as `client:out`.
    pub fn open_virtual(client: &str) -> Result<Self> {
        let seq = Seq::open(None, Some(Direction::Playback), false).context("opening ALSA sequencer")?;
        let cname = CString::new(client).context("client name")?;
        seq.set_client_name(&cname).context("naming ALSA sequencer client")?;
        let port = seq
            .create_simple_port(
                &CString::new("out").unwrap(),
                seq::PortCap::READ | seq::PortCap::SUBS_READ,
                seq::PortType::MIDI_GENERIC | seq::PortType::APPLICATION,
            )
            .context("creating ALSA sequencer port")?;
        let id = seq.client_id().unwrap_or(-1);
        let name = format!("{client}:out ({id}:{port})");
        Ok(Self { backend: Mutex::new(Backend::Seq { seq, port }), name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Subscribe the sequencer port to a destination, like `aconnect`.
    /// `dest` is `CLIENT:PORT` with numbers, or a client or sound card name
    /// (or a case-insensitive part of one) optionally followed by `:PORT`;
    /// the port must accept writes. Only for `virtual` outputs.
    pub fn connect(&self, dest: &str) -> Result<()> {
        let be = self.backend.lock().unwrap_or_else(|e| e.into_inner());
        let Backend::Seq { seq, port } = &*be else {
            anyhow::bail!("--connect needs a virtual MIDI output");
        };
        let (client_spec, port_spec) = match dest.rsplit_once(':') {
            Some((c, p)) if p.parse::<i32>().is_ok() && !c.is_empty() => (c, p.parse::<i32>().unwrap()),
            _ => (dest, 0),
        };
        let want = client_spec.trim().to_ascii_lowercase();
        let client = match client_spec.trim().parse::<i32>() {
            Ok(id) => id,
            Err(_) => seq::ClientIter::new(seq)
                .find(|c| {
                    // Client name ("Virtual Raw MIDI 5-0") or the card's name ("VirMIDI").
                    c.get_name().is_ok_and(|n| n.to_ascii_lowercase().contains(&want))
                        || c.get_card().ok().filter(|&i| i >= 0).is_some_and(|i| {
                            alsa::Card::new(i).get_name().is_ok_and(|n| n.to_ascii_lowercase().contains(&want))
                        })
                })
                .map(|c| c.get_client())
                .with_context(|| format!("no ALSA sequencer client matching {client_spec:?}"))?,
        };
        let target = seq::PortIter::new(seq, client)
            .find(|p| p.get_port() == port_spec)
            .with_context(|| format!("client {client} has no port {port_spec}"))?;
        if !target.get_capability().contains(seq::PortCap::WRITE | seq::PortCap::SUBS_WRITE) {
            anyhow::bail!("{}:{} is not an input port", client, port_spec);
        }
        let me = seq::Addr { client: seq.client_id()?, port: *port };
        let sub = seq::PortSubscribe::empty()?;
        sub.set_sender(me);
        sub.set_dest(target.addr());
        seq.subscribe_port(&sub).with_context(|| format!("connecting to {client}:{port_spec}"))?;
        Ok(())
    }

    /// Write one complete MIDI message immediately. One byte for a clock
    /// tick, so this is called up to ~100 times per second from the clock
    /// thread. The sequencer backend only knows the messages this program
    /// sends (realtime, note on/off, control change).
    #[inline]
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let mut be = self.backend.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *be {
            Backend::Raw(rm) => {
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
            Backend::Seq { seq, port } => {
                let mut ev = seq_event(bytes)?;
                ev.set_source(*port);
                ev.set_subs();
                ev.set_direct();
                seq.event_output_direct(&mut ev).context("sequencer event output")?;
                Ok(())
            }
        }
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

/// Translate one raw MIDI message into an ALSA sequencer event.
fn seq_event(bytes: &[u8]) -> Result<seq::Event<'static>> {
    use seq::EventType as T;
    let ev = match bytes {
        [CLOCK] => seq::Event::new(T::Clock, &seq::EvQueueControl::<()> { queue: 0, value: () }),
        [START] => seq::Event::new(T::Start, &seq::EvQueueControl::<()> { queue: 0, value: () }),
        [CONTINUE] => seq::Event::new(T::Continue, &seq::EvQueueControl::<()> { queue: 0, value: () }),
        [STOP] => seq::Event::new(T::Stop, &seq::EvQueueControl::<()> { queue: 0, value: () }),
        [st, note, vel] if st & 0xF0 == 0x90 || st & 0xF0 == 0x80 => {
            let t = if st & 0xF0 == 0x90 { T::Noteon } else { T::Noteoff };
            seq::Event::new(
                t,
                &seq::EvNote { channel: st & 0x0F, note: *note, velocity: *vel, off_velocity: 0, duration: 0 },
            )
        }
        [st, cc, val] if st & 0xF0 == 0xB0 => seq::Event::new(
            T::Controller,
            &seq::EvCtrl { channel: st & 0x0F, param: *cc as u32, value: *val as i32 },
        ),
        _ => anyhow::bail!("unsupported MIDI message for the sequencer backend: {bytes:02X?}"),
    };
    Ok(ev)
}

/// MIDI input: a rawmidi capture port such as `hw:CARD=US2x2,0,0`. Read
/// with a timeout so the owning thread can notice a stop request.
pub struct MidiIn {
    rm: Rawmidi,
    fds: Vec<libc::pollfd>,
    name: String,
}

impl MidiIn {
    pub fn open(name: &str) -> Result<Self> {
        let rm = Rawmidi::new(name, Direction::Capture, true).with_context(|| format!("opening MIDI input {name}"))?;
        let fds = alsa::poll::Descriptors::get(&rm).context("MIDI input poll descriptors")?;
        Ok(Self { rm, fds, name: name.to_string() })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Wait up to `timeout_ms` for bytes and read what has arrived. Returns
    /// the number of bytes put into `buf`, 0 when the time ran out.
    pub fn read(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        for fd in self.fds.iter_mut() {
            fd.revents = 0;
        }
        let ready = alsa::poll::poll(&mut self.fds, timeout_ms).context("polling MIDI input")?;
        if ready == 0 {
            return Ok(0);
        }
        let mut io = self.rm.io();
        match std::io::Read::read(&mut io, buf) {
            Ok(n) => Ok(n),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(0),
            Err(e) => Err(e).context("rawmidi read"),
        }
    }
}

/// Longest System Exclusive message `Parser` hands on, F0 and F7 included.
/// A longer one would hold up the clock ticks that share the cable.
pub const SYSEX_MAX: usize = 64;

/// Cuts the byte stream of a MIDI input into complete messages, so that
/// each can be sent on between the messages of other senders: running
/// status is written out, an unfinished message is dropped. Realtime bytes
/// (0xF8..) may come in the middle of any message and are not handled here.
pub struct Parser {
    buf: [u8; SYSEX_MAX],
    len: usize,
    /// Length of the message being collected.
    need: usize,
    /// Status for data bytes that come without one (running status), 0 = none.
    running: u8,
    sysex: bool,
    /// The SysEx being collected is longer than `SYSEX_MAX`.
    overflow: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self { buf: [0; SYSEX_MAX], len: 0, need: 0, running: 0, sysex: false, overflow: false }
    }
}

impl Parser {
    /// Take the next byte of the stream. Returns the message it completes.
    pub fn push(&mut self, b: u8) -> Option<&[u8]> {
        match b {
            0xF8..=0xFF => None,
            0xF0 => {
                self.buf[0] = b;
                self.len = 1;
                self.sysex = true;
                self.overflow = false;
                self.running = 0;
                None
            }
            0xF7 => {
                let n = self.len;
                let whole = self.sysex && !self.overflow;
                self.len = 0;
                self.sysex = false;
                if !whole {
                    return None;
                }
                self.buf[n] = b;
                Some(&self.buf[..=n])
            }
            0x80..=0xF6 => {
                // A status byte ends whatever was unfinished, a SysEx too.
                self.sysex = false;
                self.running = if b < 0xF0 { b } else { 0 };
                self.need = match b {
                    0xC0..=0xDF | 0xF1 | 0xF3 => 2,
                    0xF6 => 1,
                    0xF4 | 0xF5 => 0,
                    _ => 3,
                };
                self.buf[0] = b;
                self.len = usize::from(self.need > 1);
                (self.need == 1).then_some(&self.buf[..1])
            }
            _ if self.sysex => {
                if self.len < SYSEX_MAX - 1 {
                    self.buf[self.len] = b;
                    self.len += 1;
                } else {
                    self.overflow = true;
                }
                None
            }
            _ => {
                if self.len == 0 {
                    if self.running == 0 {
                        return None;
                    }
                    self.buf[0] = self.running;
                    self.len = 1;
                }
                self.buf[self.len] = b;
                self.len += 1;
                if self.len < self.need {
                    return None;
                }
                let n = self.len;
                self.len = 0;
                Some(&self.buf[..n])
            }
        }
    }
}

/// Human-readable list of rawmidi ports on all cards in `dir`.
fn list_ports(dir: Direction) -> Vec<String> {
    let mut out = Vec::new();
    for card in alsa::card::Iter::new().flatten() {
        let idx = card.get_index();
        let ctl = match alsa::Ctl::from_card(&card, false) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let card_name = card.get_name().unwrap_or_default();
        for info in alsa::rawmidi::Iter::new(&ctl).flatten() {
            if info.get_stream() != dir {
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

/// Human-readable list of rawmidi output ports on all cards.
pub fn list_outputs() -> Vec<String> {
    list_ports(Direction::Playback)
}

/// Human-readable list of rawmidi input ports on all cards.
pub fn list_inputs() -> Vec<String> {
    list_ports(Direction::Capture)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut p = Parser::default();
        bytes.iter().filter_map(|&b| p.push(b).map(<[u8]>::to_vec)).collect()
    }

    #[test]
    fn running_status_is_written_out() {
        assert_eq!(parse(&[0x93, 60, 100, 62, 90, 60, 0]), [[0x93, 60, 100], [0x93, 62, 90], [0x93, 60, 0]]);
        // program change and channel pressure have one data byte
        assert_eq!(parse(&[0xC1, 5, 6, 0xD0, 7]), [[0xC1, 5], [0xC1, 6], [0xD0, 7]]);
        // data before any status is nobody's
        assert!(parse(&[60, 100, 0x80]).is_empty());
    }

    #[test]
    fn realtime_bytes_inside_a_message_do_not_break_it() {
        assert_eq!(parse(&[0x90, CLOCK, 60, START, 100, 0xFE, 0xB0, 7, CLOCK, 127]), [[0x90, 60, 100], [0xB0, 7, 127]]);
    }

    #[test]
    fn system_common_messages() {
        // song position, song select, tune request; no running status after them
        assert_eq!(parse(&[0xF2, 1, 2, 3, 4, 0xF3, 9, 0xF6, 0x90, 1, 2]), [vec![0xF2, 1, 2], vec![0xF3, 9], vec![0xF6], vec![0x90, 1, 2]]);
    }

    #[test]
    fn short_sysex_passes_and_long_sysex_is_dropped() {
        assert_eq!(parse(&[0xF0, 0x7F, 0x7F, 6, 2, 0xF7]), [[0xF0, 0x7F, 0x7F, 6, 2, 0xF7]]);
        let mut full = vec![0xF0];
        full.extend(std::iter::repeat(1).take(SYSEX_MAX - 2));
        full.push(0xF7);
        assert_eq!(parse(&full), [full.clone()]);
        let mut long = vec![0xF0];
        long.extend(std::iter::repeat(1).take(SYSEX_MAX - 1));
        long.extend([0xF7, 0x90, 60, 100]);
        assert_eq!(parse(&long), [[0x90, 60, 100]]);
        // a SysEx cut off by another message is dropped, the message is not
        assert_eq!(parse(&[0xF0, 1, 2, 0x90, 60, 100, 0xF7]), [[0x90, 60, 100]]);
    }
}
