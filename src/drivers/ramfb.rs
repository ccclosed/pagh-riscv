//! QEMU's `ramfb` device: a linear framebuffer that lives in guest RAM.
//!
//! This is the RISC-V port of what Limine handed the x86 kernel in its
//! framebuffer response — with one structural difference that shapes everything
//! downstream: **the surface is not MMIO**. QEMU maps the guest's own RAM as the
//! display surface, so a pixel write is a normal store to RAM, not an uncached
//! trip to a device. There is no "write to the device" step at all; what the
//! device needs instead is to be *told* where the surface is.
//!
//! The instruction comes through fw_cfg: QEMU registers the writable file
//! `etc/ramfb` ([`FwCfg::write_file`](crate::drivers::fw_cfg::FwCfg::write_file))
//! and its write callback ([`configure`] below) parses a 28-byte, fully
//! big-endian, packed structure:
//!
//! ```text
//! offset  size  field
//!      0     8  addr    physical address of the surface (guest RAM)
//!      8     4  fourcc  pixel format; 'XR24' (XRGB8888) is the one that works
//!     12     4  flags   unused, zero
//!     16     4  width   pixels
//!     20     4  height  pixels
//!     24     4  stride  bytes per row
//! ```
//!
//! Three details decide whether this works at all, and each one fails as a black
//! screen rather than an error:
//!
//! * the fourcc must be DRM's `XRGB8888` (`'X' 'R' '2' '4'`). Anything else and
//!   QEMU's `qemu_drm_format_to_pixman` yields "unknown format" and the device
//!   quietly keeps its previous (empty) surface;
//! * `addr` must be **physical** and the whole `stride * (height - 1) + width*4`
//!   range must map contiguously: QEMU maps it with `cpu_physical_memory_map`
//!   and refuses the surface if the mapping comes up short. A buffer that spans
//!   two memory regions would therefore be rejected, which is why the caller
//!   hands us one static array and not a heap object;
//! * geometry is validated by QEMU against the VBE limits — at least 16x16, at
//!   most 2560x1600 — and an out-of-range size is again a silent no-op.
//!
//! Only XRGB8888 is implemented: it is the format the x86 console's blitter
//! already writes (blue, green, red bytes in order, the fourth byte ignored), so
//! the ported blitter needs no pixel-format branch, and `tools/font_check.py`
//! compares the RGB channels of a screendump, where the ignored byte cannot
//! matter.

use core::fmt;

use crate::drivers::fw_cfg::{self, FwCfg};

/// DRM fourcc `XR24` — DRM_FORMAT_XRGB8888 as the u32 the DRM headers define.
///
/// Spelled as the constant rather than as the four bytes so a reader can check
/// it against `drm_fourcc.h` (`fourcc_code('X', 'R', '2', '4')`) without
/// decoding a magic number.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;

/// Bytes per pixel of [`FOURCC_XRGB8888`] — three channels plus the ignored one.
pub const BYTES_PER_PIXEL: usize = 4;

/// The configuration item's name in the fw_cfg directory.
const ITEM_NAME: &str = "etc/ramfb";

/// Size of the packed configuration structure (see the module comment).
const CONFIG_BYTES: usize = 28;

/// Smallest surface ramfb accepts (QEMU validates against the VBE limits).
pub const MIN_SIDE: u32 = 16;
/// Largest surface ramfb accepts.
pub const MAX_WIDTH: u32 = 2560;
/// Largest surface ramfb accepts.
pub const MAX_HEIGHT: u32 = 1600;

/// Why a ramfb configuration attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The fw_cfg side refused (no item, transfer error, wrong size).
    FwCfg(fw_cfg::Error),
    /// Geometry outside the range QEMU's ramfb implements.
    Geometry {
        /// Requested width.
        width: usize,
        /// Requested height.
        height: usize,
    },
    /// The device did not report the configuration back byte for byte.
    NotAccepted,
}

impl From<fw_cfg::Error> for Error {
    fn from(error: fw_cfg::Error) -> Self {
        Error::FwCfg(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FwCfg(error) => write!(f, "{error}"),
            Error::Geometry { width, height } => write!(
                f,
                "{width}x{height} is outside ramfb's {MIN_SIDE}..{MAX_WIDTH}x{MIN_SIDE}..{MAX_HEIGHT}"
            ),
            Error::NotAccepted => write!(f, "the device did not keep the configuration it was given"),
        }
    }
}

/// What the device was told to display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// Visible width in pixels.
    pub width: usize,
    /// Visible height in pixels.
    pub height: usize,
    /// Bytes per row (the framebuffer's own stride, not `width * 4` by
    /// accident).
    pub stride: usize,
    /// Bytes per pixel.
    pub bpp: usize,
}

impl Screen {
    /// Bytes the surface occupies, with QEMU's formula: the last row needs only
    /// its visible pixels, not a full stride.
    pub fn surface_bytes(&self) -> usize {
        self.stride * (self.height - 1) + self.width * self.bpp
    }
}

/// Tell the ramfb device to display `addr` as a `width`×`height` XRGB8888
/// surface.
///
/// `addr` is a physical address (identity-mapped kernel: the pointer itself),
/// and it must stay valid for as long as the device may display it — the caller
/// passes a `static`, because there is no allocator to outlive it and because
/// QEMU's mapping holds on to the range.
///
/// The configuration is read back and compared before success is reported. That
/// read-back is not ceremony: the whole write path is "a struct in RAM plus a
/// DMA command", and the failure mode of that path (a wrong endianness, a
/// dropped write) is a device that accepted nothing and shows nothing. The
/// comparison turns it into a boot-log line.
pub fn configure(
    fw_cfg: &FwCfg,
    addr: usize,
    width: usize,
    height: usize,
) -> Result<Screen, Error> {
    if !(MIN_SIDE as usize..=MAX_WIDTH as usize).contains(&width)
        || !(MIN_SIDE as usize..=MAX_HEIGHT as usize).contains(&height)
    {
        return Err(Error::Geometry { width, height });
    }
    let stride = width * BYTES_PER_PIXEL;

    let mut config = [0u8; CONFIG_BYTES];
    config[0..8].copy_from_slice(&(addr as u64).to_be_bytes());
    config[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes());
    config[12..16].copy_from_slice(&0u32.to_be_bytes()); // flags: unused by QEMU
    config[16..20].copy_from_slice(&(width as u32).to_be_bytes());
    config[20..24].copy_from_slice(&(height as u32).to_be_bytes());
    config[24..28].copy_from_slice(&(stride as u32).to_be_bytes());

    fw_cfg.write_file(ITEM_NAME, &config)?;

    let mut readback = [0u8; CONFIG_BYTES];
    fw_cfg.read_file(ITEM_NAME, &mut readback)?;
    if readback != config {
        return Err(Error::NotAccepted);
    }

    Ok(Screen {
        width,
        height,
        stride,
        bpp: BYTES_PER_PIXEL,
    })
}
