//! Touch input straight from evdev (`/dev/input/eventN`), no libinput. Handles
//! single-touch (ABS_X/Y + BTN_TOUCH) and multi-touch type B (ABS_MT_*) panels
//! and reports one finger. Event timestamps are switched to CLOCK_MONOTONIC so
//! taps share the audio/MIDI time base.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const INPUT_PROP_DIRECT: usize = 0x01;

const IOC_READ: libc::c_ulong = 2;
const IOC_WRITE: libc::c_ulong = 1;

const fn ioc(dir: libc::c_ulong, nr: libc::c_ulong, size: libc::c_ulong) -> libc::c_ulong {
    (dir << 30) | (size << 16) | ((b'E' as libc::c_ulong) << 8) | nr
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchEvent {
    /// Finger down at physical pixel coordinates, monotonic time.
    Down { x: i32, y: i32, t: f64 },
    Move { x: i32, y: i32, t: f64 },
    Up { t: f64 },
}

#[derive(Clone, Debug)]
pub struct TouchConfig {
    pub swap_xy: bool,
    pub invert_x: bool,
    pub invert_y: bool,
    /// Take the device exclusively (EVIOCGRAB).
    pub grab: bool,
}

impl Default for TouchConfig {
    fn default() -> Self {
        Self { swap_xy: false, invert_x: false, invert_y: false, grab: true }
    }
}

pub struct Touch {
    file: File,
    pub name: String,
    pub path: String,
    cfg: TouchConfig,
    screen_w: i32,
    screen_h: i32,
    x_range: (i32, i32),
    y_range: (i32, i32),
    multitouch: bool,
    // pending state between SYN_REPORTs
    raw_x: i32,
    raw_y: i32,
    touching: bool,
    was_touching: bool,
    slot: i32,
    moved: bool,
    buf: Vec<u8>,
}

fn has_bit(bits: &[u8], i: usize) -> bool {
    bits.get(i / 8).map(|b| b & (1 << (i % 8)) != 0).unwrap_or(false)
}

fn get_bits(fd: i32, ev: u16, out: &mut [u8]) -> bool {
    let req = ioc(IOC_READ, 0x20 + ev as libc::c_ulong, out.len() as libc::c_ulong);
    // SAFETY: EVIOCGBIT writes at most out.len() bytes into out.
    unsafe { libc::ioctl(fd, req as _, out.as_mut_ptr()) >= 0 }
}

fn get_abs(fd: i32, code: u16) -> Option<AbsInfo> {
    let mut info = AbsInfo::default();
    let req = ioc(IOC_READ, 0x40 + code as libc::c_ulong, std::mem::size_of::<AbsInfo>() as libc::c_ulong);
    // SAFETY: EVIOCGABS fills a struct input_absinfo of exactly this layout.
    if unsafe { libc::ioctl(fd, req as _, &mut info) } < 0 {
        None
    } else {
        Some(info)
    }
}

fn get_name(fd: i32) -> String {
    let mut buf = [0u8; 128];
    let req = ioc(IOC_READ, 0x06, buf.len() as libc::c_ulong);
    // SAFETY: EVIOCGNAME writes at most buf.len() bytes.
    let n = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..n as usize]).trim_end_matches('\0').to_string()
}

/// Score a device as a touchscreen: None = not one.
fn probe(path: &str) -> Option<(File, String, u32)> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    let fd = file.as_raw_fd();
    let mut types = [0u8; 4];
    if !get_bits(fd, 0, &mut types) || !has_bit(&types, EV_ABS as usize) {
        return None;
    }
    let mut abs = [0u8; 8];
    get_bits(fd, EV_ABS, &mut abs);
    let mt = has_bit(&abs, ABS_MT_POSITION_X as usize);
    let st = has_bit(&abs, ABS_X as usize) && has_bit(&abs, ABS_Y as usize);
    if !mt && !st {
        return None;
    }
    let mut keys = [0u8; 96];
    get_bits(fd, EV_KEY, &mut keys);
    let touch = has_bit(&keys, BTN_TOUCH as usize);
    if !touch && !mt {
        return None; // joystick, accelerometer, ...
    }
    let mut props = [0u8; 4];
    let req = ioc(IOC_READ, 0x09, props.len() as libc::c_ulong);
    // SAFETY: EVIOCGPROP writes at most props.len() bytes.
    unsafe { libc::ioctl(fd, req as _, props.as_mut_ptr()) };
    let mut score = 1;
    if has_bit(&props, INPUT_PROP_DIRECT) {
        score += 2;
    }
    if touch {
        score += 1;
    }
    Some((file, get_name(fd), score))
}

/// List candidate touch devices as (path, name).
pub fn list() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for i in 0..32 {
        let p = format!("/dev/input/event{i}");
        if let Some((_, name, _)) = probe(&p) {
            v.push((p, name));
        }
    }
    v
}

impl Touch {
    /// Open the given device, or autodetect when `path` is empty.
    pub fn open(path: &str, cfg: TouchConfig, screen_w: usize, screen_h: usize) -> io::Result<Self> {
        let (file, name, path) = if path.is_empty() {
            let mut best: Option<(File, String, u32, String)> = None;
            for i in 0..32 {
                let p = format!("/dev/input/event{i}");
                if let Some((f, n, s)) = probe(&p) {
                    if best.as_ref().map(|b| s > b.2).unwrap_or(true) {
                        best = Some((f, n, s, p));
                    }
                }
            }
            let (f, n, _, p) = best.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no touchscreen found under /dev/input"))?;
            (f, n, p)
        } else {
            let (f, n, _) = probe(path).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path} is not a touch device")))?;
            (f, n, path.to_string())
        };
        let fd = file.as_raw_fd();
        let mt = get_abs(fd, ABS_MT_POSITION_X);
        let (xi, yi, multitouch) = match (mt, get_abs(fd, ABS_MT_POSITION_Y)) {
            (Some(x), Some(y)) => (x, y, true),
            _ => (get_abs(fd, ABS_X).unwrap_or_default(), get_abs(fd, ABS_Y).unwrap_or_default(), false),
        };
        // Monotonic timestamps, exclusive access.
        let clk: libc::c_int = libc::CLOCK_MONOTONIC;
        // SAFETY: EVIOCSCLOCKID reads one int.
        unsafe { libc::ioctl(fd, ioc(IOC_WRITE, 0xa0, 4) as _, &clk) };
        if cfg.grab {
            // SAFETY: EVIOCGRAB takes an int by value.
            unsafe { libc::ioctl(fd, ioc(IOC_WRITE, 0x90, 4) as _, 1 as libc::c_ulong) };
        }
        let fix = |i: AbsInfo| if i.maximum > i.minimum { (i.minimum, i.maximum) } else { (0, 4095) };
        Ok(Self {
            file,
            name,
            path,
            cfg,
            screen_w: screen_w as i32,
            screen_h: screen_h as i32,
            x_range: fix(xi),
            y_range: fix(yi),
            multitouch,
            raw_x: 0,
            raw_y: 0,
            touching: false,
            was_touching: false,
            slot: 0,
            moved: false,
            buf: vec![0; 64 * std::mem::size_of::<libc::input_event>()],
        })
    }

    fn to_screen(&self, raw_x: i32, raw_y: i32) -> (i32, i32) {
        let scale = |v: i32, r: (i32, i32), n: i32| -> i32 {
            let f = (v - r.0) as f64 / (r.1 - r.0).max(1) as f64;
            ((f * n as f64) as i32).clamp(0, n - 1)
        };
        let (mut x, mut y) = if self.cfg.swap_xy {
            (scale(raw_y, self.y_range, self.screen_w), scale(raw_x, self.x_range, self.screen_h))
        } else {
            (scale(raw_x, self.x_range, self.screen_w), scale(raw_y, self.y_range, self.screen_h))
        };
        if self.cfg.invert_x {
            x = self.screen_w - 1 - x;
        }
        if self.cfg.invert_y {
            y = self.screen_h - 1 - y;
        }
        (x, y)
    }

    /// Drain pending events without blocking.
    pub fn poll(&mut self, out: &mut Vec<TouchEvent>) -> io::Result<()> {
        let sz = std::mem::size_of::<libc::input_event>();
        loop {
            let n = match self.file.read(&mut self.buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            for chunk in self.buf[..n].chunks_exact(sz) {
                // SAFETY: the kernel writes whole input_event structs.
                let ev: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const _) };
                let t = ev.time.tv_sec as f64 + ev.time.tv_usec as f64 * 1e-6;
                match (ev.type_, ev.code) {
                    (EV_ABS, ABS_MT_SLOT) => self.slot = ev.value,
                    (EV_ABS, ABS_MT_TRACKING_ID) if self.slot == 0 => self.touching = ev.value >= 0,
                    (EV_ABS, ABS_MT_POSITION_X) if self.multitouch && self.slot == 0 => {
                        self.raw_x = ev.value;
                        self.moved = true;
                    }
                    (EV_ABS, ABS_MT_POSITION_Y) if self.multitouch && self.slot == 0 => {
                        self.raw_y = ev.value;
                        self.moved = true;
                    }
                    (EV_ABS, ABS_X) if !self.multitouch => {
                        self.raw_x = ev.value;
                        self.moved = true;
                    }
                    (EV_ABS, ABS_Y) if !self.multitouch => {
                        self.raw_y = ev.value;
                        self.moved = true;
                    }
                    (EV_KEY, BTN_TOUCH) if !self.multitouch => self.touching = ev.value != 0,
                    (EV_SYN, SYN_REPORT) => {
                        let (x, y) = self.to_screen(self.raw_x, self.raw_y);
                        if self.touching && !self.was_touching {
                            out.push(TouchEvent::Down { x, y, t });
                        } else if self.touching && self.moved {
                            out.push(TouchEvent::Move { x, y, t });
                        } else if !self.touching && self.was_touching {
                            out.push(TouchEvent::Up { t });
                        }
                        self.was_touching = self.touching;
                        self.moved = false;
                    }
                    _ => {}
                }
            }
        }
    }
}
