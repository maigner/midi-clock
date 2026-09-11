//! Linux framebuffer output (`/dev/fb0`). Frames are pushed with one
//! `pwrite` of the whole buffer, which also marks the damage for DRM fbdev
//! emulation (the KMS driver on the Pi), so no page-fault tracking is needed.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;

use crate::canvas::{Canvas, Channel, PixelFormat, Rotation};

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

pub struct Framebuffer {
    file: File,
    tty: Option<File>,
    pub width: usize,
    pub height: usize,
    pub line_length: usize,
    pub format: PixelFormat,
    pub id: String,
}

impl Framebuffer {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut var = FbVarScreeninfo::default();
        // SAFETY: plain ioctl reads into a repr(C) struct of the kernel's layout.
        if unsafe { libc::ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO as _, &mut var) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; the struct is fully overwritten by the kernel.
        let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO as _, &mut fix) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if var.bits_per_pixel != 16 && var.bits_per_pixel != 32 && var.bits_per_pixel != 24 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("framebuffer depth {} bpp not supported (16/24/32 only)", var.bits_per_pixel),
            ));
        }
        let ch = |b: FbBitfield| Channel { offset: b.offset, length: b.length };
        let format = PixelFormat {
            bytes_per_pixel: (var.bits_per_pixel / 8) as usize,
            red: ch(var.red),
            green: ch(var.green),
            blue: ch(var.blue),
        };
        let id = String::from_utf8_lossy(&fix.id).trim_end_matches('\0').to_string();
        Ok(Self {
            file,
            tty: None,
            width: var.xres as usize,
            height: var.yres as usize,
            line_length: fix.line_length as usize,
            format,
            id,
        })
    }

    /// Switch a virtual console to graphics mode so the text console stops
    /// drawing over us (cursor blink, kernel messages). Restored on drop.
    pub fn take_console(&mut self, tty_path: &str) -> io::Result<()> {
        let tty = OpenOptions::new().read(true).write(true).open(tty_path)?;
        // SAFETY: KDSETMODE takes an int argument.
        if unsafe { libc::ioctl(tty.as_raw_fd(), KDSETMODE as _, KD_GRAPHICS as libc::c_ulong) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.tty = Some(tty);
        Ok(())
    }

    pub fn canvas(&self, rotation: Rotation) -> Canvas {
        Canvas::new(self.width, self.height, self.line_length, self.format, rotation)
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}x{} {} bpp (R{}:{} G{}:{} B{}:{})",
            self.id,
            self.width,
            self.height,
            self.format.bytes_per_pixel * 8,
            self.format.red.offset,
            self.format.red.length,
            self.format.green.offset,
            self.format.green.length,
            self.format.blue.offset,
            self.format.blue.length
        )
    }

    /// Push a whole frame.
    pub fn present(&self, canvas: &Canvas) -> io::Result<()> {
        self.file.write_all_at(canvas.bytes(), 0)
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        if let Some(tty) = &self.tty {
            // SAFETY: as in take_console.
            unsafe {
                libc::ioctl(tty.as_raw_fd(), KDSETMODE as _, KD_TEXT as libc::c_ulong);
            }
        }
    }
}
