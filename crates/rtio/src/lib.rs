//! Real-time I/O layer: monotonic time, SCHED_FIFO helpers, raw ALSA duplex
//! PCM and raw MIDI input and output. Everything here is deliberately thin; the hot
//! paths do no allocation and no locking.

pub mod midi;
pub mod pcm;
pub mod rt;
pub mod time;

pub use midi::{MidiIn, MidiOut};
pub use pcm::{Duplex, PcmConfig};
pub use time::{now, sleep_until};
