//! The framebuffer console: the same text the UART carries, drawn with the
//! console typeface into a screen QEMU can photograph.
//!
//! This is a port of the x86 kernel's `drivers/framebuffer.rs`, and the *why* of
//! its structure survives the port intact:
//!
//! * **Everything is drawn into a RAM shadow buffer** and pushed to the device by
//!   [`FramebufferWriter::flush`], which copies only the rectangle that changed.
//!   On x86 the reason was cost (every device write was uncached MMIO); here the
//!   device surface is ordinary RAM, so the copy is cheap — and the flush is kept
//!   because it is the *frame boundary*. QEMU samples the ramfb surface whenever
//!   its display refreshes or the monitor takes a `screendump`, and a screenshot
//!   of a half-drawn frame is a lie in exactly the artefact this milestone
//!   produces (`tools/font_check.py` fails on a single wrong pixel, so "usually
//!   complete" is not good enough);
//! * **the font is 8-bit coverage**, not a bitmap: each byte is blended between
//!   the console's background and foreground colour with the rounding in
//!   [`blend_px`], which is what makes an 8x18 Fira Code cell read like the same
//!   face in a host terminal. The rounding is load-bearing: the checker rebuilds
//!   the expected screen from the same formula and fails on one differing pixel;
//! * **the text grid is a model**, retained next to the pixels, because pixels
//!   cannot answer "what character is in this cell" — the shell work (caret, copy
//!   off screen) needs that, and repainting a cell from the model is how
//!   backspace erases without guessing.
//!
//! What the port *adds*, and why:
//!
//! * **no allocator.** The x86 writer owned `Vec`s; here the two surfaces are
//!   `static` arrays and the grid is a fixed array, so the console is independent
//!   of the M2 heap work — the coordination rule for this milestone is that every
//!   driver uses fixed storage;
//! * **a tiny ANSI filter.** The shell's `clear` prints `ESC [ 2 J ESC [ H`; a
//!   console sink that treated those bytes as text would smear `[2J[H` over the
//!   screen. Sequences are swallowed whole (never rendered), and the two the
//!   kernel itself emits are acted on: full clear and home;
//! * **the status bar is drawn by [`init`]** with labels stored in the writer, so
//!   it survives the `clear` command without the shell having to repaint it.
//!
//! Geometry is compile-time (`SCREEN_WIDTH`/`SCREEN_HEIGHT`) because the surfaces
//! are static arrays; ramfb accepts any size, but a fixed mode is what makes the
//! screenshot comparable between runs and the checker's grid arithmetic simple.
//! The cell constants move with `assets/font8x18.bin` and nothing else may
//! re-declare them (`tools/gen_font.py` decides the cell; `tools/font_check.py`
//! cross-checks both against the asset).

use core::fmt;

use crate::drivers::fw_cfg;
use crate::drivers::ramfb;
use crate::sync::Spinlock;

/// Glyph cell width in pixels. **Public on purpose**: the checker and the
/// generator both pin the cell, and a second copy of these numbers anywhere in
/// the kernel is a silent layout bug the moment the font changes.
pub const CHAR_WIDTH: usize = 8;
/// Glyph cell height in pixels (see [`CHAR_WIDTH`]).
pub const CHAR_HEIGHT: usize = 18;
/// Columns of text on one row. The x86 console's grid, kept so the two kernels
/// lay out alike (and so a screenshot of one is readable next to the other's).
const CHARS_PER_LINE: usize = 100;
/// Rows the console may hold. The screen is taller than this (see
/// [`FramebufferWriter::text_rows`]): 37 rows is the x86 console's model size and
/// the grid is a fixed array.
const MAX_LINES: usize = 37;

/// Height in pixels of the reserved status-bar strip at the bottom of the
/// screen. The text console never scrolls into it (`text_rows` reserves it).
pub const STATUS_BAR_HEIGHT: usize = CHAR_HEIGHT;

/// The console's screen, in pixels.
///
/// 1280x800 is the x86 kernel's default mode, and the mode the screenshot
/// tooling and checker are tuned for. These two constants, the static surfaces
/// below and the status bar's right-hand label move together.
pub const SCREEN_WIDTH: usize = 1280;
/// See [`SCREEN_WIDTH`].
pub const SCREEN_HEIGHT: usize = 800;
/// Bytes of one surface: the scanout and the shadow are both exactly this long.
const SURFACE_BYTES: usize = SCREEN_WIDTH * SCREEN_HEIGHT * ramfb::BYTES_PER_PIXEL;

/// Labels the status bar shows until something asks for others.
const DEFAULT_STATUS_LEFT: &str = "pagh OS  RISC-V (RV64)";
const DEFAULT_STATUS_RIGHT: &str = "ramfb 1280x800";
/// Longest status-bar label kept for repainting (a label is a strip, not a line).
const LABEL_BYTES: usize = 48;

/// The console typeface: Fira Code (the host's terminal font), rasterised
/// offline by `tools/gen_font.py` into one **8-bit coverage** glyph per printable
/// ASCII codepoint.
///
/// The kernel deliberately does not parse TrueType: the asset holds 95 glyphs,
/// each `CHAR_HEIGHT` rows of `CHAR_WIDTH` coverage bytes (0 = background,
/// 255 = full ink), covering 0x20 (space) through 0x7E ('~'), so the glyph for
/// codepoint `ch` lives at index `ch - 0x20`. Coverage — not a bitmap — is what
/// lets the console antialias ([`blend_px`]). Regenerate (never hand-edit) with::
///
///     python3 tools/gen_font.py --out assets/font8x18.bin
///
/// The generator derives the pixel size from the cell (`--cell 8 --cell-h 18`)
/// and `--check` re-rasterises the committed asset and fails unless the bytes
/// match; `tools/font_check.py` additionally cross-checks these constants
/// against the asset, because a font with two sources is a font that drifts.
static FONT: &[u8] = include_bytes!("../../assets/font8x18.bin");

/// Console colours: white on black, which is also what `tools/font_check.py`
/// compares a screenshot against by default.
pub const DEFAULT_FG: u32 = 0xFF_FFFF;
/// See [`DEFAULT_FG`].
pub const DEFAULT_BG: u32 = 0x00_0000;

const FONT_FIRST_CP: u8 = 0x20; // first codepoint present in the font
const FONT_LAST_CP: u8 = 0x7E; // last codepoint present in the font
const GLYPH_BYTES: usize = CHAR_HEIGHT * CHAR_WIDTH; // coverage bytes per glyph
const GRID_CELLS: usize = MAX_LINES * CHARS_PER_LINE;

/// Fallback glyph (hollow rectangle) for codepoints outside the font range.
/// Built from the cell rather than written out, so it keeps filling exactly one
/// cell when the cell changes. `write_char` never reaches it (it drops
/// non-printable bytes); `draw_text_px` does, for a label with a stray byte.
const FALLBACK_GLYPH: [u8; GLYPH_BYTES] = {
    let mut glyph = [0u8; GLYPH_BYTES];
    let mut i = 0;
    while i < GLYPH_BYTES {
        let (row, col) = (i / CHAR_WIDTH, i % CHAR_WIDTH); // pixel this byte holds
        let border = row == 0 || row == CHAR_HEIGHT - 1 || col == 0 || col == CHAR_WIDTH - 1;
        glyph[i] = if border { 0xFF } else { 0x00 };
        i += 1;
    }
    glyph
};

/// The scanout surface: the pixels the ramfb device displays.
///
/// A `static` because the device is told a **physical address** and holds it for
/// as long as the surface exists — there is no allocator (M2) and nothing that
/// could hand the device a buffer it might outlive. Written only by
/// [`FramebufferWriter::flush_rect`], one flushed rectangle at a time.
static mut SCANOUT: [u8; SURFACE_BYTES] = [0; SURFACE_BYTES];

/// The shadow surface: what the console actually draws into.
///
/// Two surfaces rather than one because the device reads its surface at moments
/// the kernel does not control; see the module comment. Both are ordinary RAM,
/// so [`FramebufferWriter::flush_rect`] is a row-wise memory copy.
static mut SHADOW: [u8; SURFACE_BYTES] = [0; SURFACE_BYTES];

/// Returns the coverage bitmap for `ch`, sourced from the embedded font.
/// Codepoints outside the font's `0x20..=0x7E` range fall back to a plain box.
fn get_glyph(ch: u8) -> &'static [u8] {
    if ch >= FONT_FIRST_CP && ch <= FONT_LAST_CP {
        let idx = (ch - FONT_FIRST_CP) as usize;
        let start = idx * GLYPH_BYTES;
        &FONT[start..start + GLYPH_BYTES]
    } else {
        &FALLBACK_GLYPH
    }
}

/// Coverage (0 = background, 255 = full ink) of pixel `(dx, dy)` of `glyph`.
///
/// The glyph is `CHAR_HEIGHT` rows of `CHAR_WIDTH` bytes, row-major: one byte per
/// pixel, in reading order. Treating a row as any other length is the mistake
/// that stays silent — it shifts every row of every glyph after the first.
fn glyph_coverage(glyph: &[u8], dx: usize, dy: usize) -> u8 {
    glyph[dy * CHAR_WIDTH + dx]
}

/// Blend `fg` over `base` with 8-bit coverage: 0 leaves `base`, 255 is exactly
/// `fg`, and everything between walks the ramp with rounding.
///
/// This is the whole reason the asset stores coverage instead of bits: the
/// console gets the antialiased edges the same face has in a host terminal. The
/// rounding matters beyond cosmetics — `tools/font_check.py` rebuilds the
/// expected screen from the asset with the same formula and fails on a single
/// differing pixel, so an off-by-one ramp means the console is not provably
/// showing the font that is committed.
fn blend_px(base: u32, fg: u32, cov: u8) -> u32 {
    match cov {
        0 => base,
        255 => fg,
        _ => {
            let c = cov as u32;
            let ch = |shift: u32| {
                let (a, b) = ((base >> shift) & 0xFF, (fg >> shift) & 0xFF);
                // Coverage is 0..=255, i.e. the blend weight is cov/255 (not
                // /256): the `+127` rounds, so coverage 128 lands halfway.
                (a * (255 - c) + b * c + 127) / 255
            };
            (ch(16) << 16) | (ch(8) << 8) | ch(0)
        }
    }
}

/// What the ANSI filter decided to do with a byte.
enum AnsiAction {
    /// Not part of an escape sequence: draw it.
    Print(u8),
    /// `CSI 2 J` — erase the whole screen.
    Clear,
    /// `CSI H` — move the cursor home.
    Home,
    /// Part of a sequence (or of one this console does not implement): consume
    /// it silently.
    Swallow,
}

/// A minimal CSI parser, in front of the glyph writer.
///
/// The kernel's only escape-sequence producer is the shell's `clear`, and the
/// only sequences worth acting on are "clear" and "home". The parser is still a
/// parser rather than a `s.starts_with("\x1b[2J")` check because the sequence
/// can be split across `write_str` calls by the formatting machinery, and because
/// *swallowing* an unknown sequence is what keeps a stray control byte from
/// becoming a glyph.
#[derive(Clone, Copy)]
struct AnsiFilter {
    state: AnsiState,
    /// First numeric parameter of the sequence being read (`0` when none was
    /// given; only `CSI 2 J` distinguishes a value here).
    parameter: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    /// Ordinary text.
    Ground,
    /// Seen `ESC`; the next byte decides.
    Escape,
    /// Inside a control sequence (`ESC [ ...`).
    Control,
}

impl AnsiFilter {
    const fn new() -> Self {
        AnsiFilter {
            state: AnsiState::Ground,
            parameter: 0,
        }
    }

    fn feed(&mut self, byte: u8) -> AnsiAction {
        match self.state {
            AnsiState::Ground => {
                if byte == 0x1b {
                    self.state = AnsiState::Escape;
                    AnsiAction::Swallow
                } else {
                    AnsiAction::Print(byte)
                }
            }
            AnsiState::Escape => {
                // `ESC [` introduces a control sequence; anything else is a
                // two-byte escape this console does not use, so both bytes go.
                self.state = if byte == b'[' {
                    self.parameter = 0;
                    AnsiState::Control
                } else {
                    AnsiState::Ground
                };
                AnsiAction::Swallow
            }
            AnsiState::Control => match byte {
                // Parameter and intermediate bytes: digits accumulate, `;` and
                // the private-parameter range are accepted and ignored.
                0x30..=0x39 => {
                    self.parameter = self
                        .parameter
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                    AnsiAction::Swallow
                }
                0x20..=0x3f => AnsiAction::Swallow,
                // Final byte: act on the two sequences the kernel emits, swallow
                // the rest so they cannot appear as text.
                0x40..=0x7e => {
                    let action = match (byte, self.parameter) {
                        (b'J', 2) => AnsiAction::Clear,
                        (b'H', _) => AnsiAction::Home,
                        _ => AnsiAction::Swallow,
                    };
                    self.state = AnsiState::Ground;
                    self.parameter = 0;
                    action
                }
                _ => {
                    self.state = AnsiState::Ground;
                    self.parameter = 0;
                    AnsiAction::Swallow
                }
            },
        }
    }
}

/// The console's state: text position and model, colours, and the two surfaces.
///
/// The surface fields are **addresses** (`usize`), not pointers or slices. Two
/// reasons, both structural: the surfaces are `static`s whose lifetime is the
/// machine's, so no Rust reference type describes them; and a raw pointer would
/// make this struct `!Send`, while a `static` console has to be `Sync`. The
/// addresses always come from the statics above, and every access is bounded by
/// the geometry checked in [`FramebufferWriter::new`].
struct FramebufferWriter {
    scanout: usize,
    back: usize,
    width: usize,
    height: usize,
    pitch: usize,
    bpp: usize,
    col: usize,
    row: usize,
    fg_color: u32,
    bg_color: u32,
    /// Status-bar strip colours; the bar is the one place the console paints a
    /// background on purpose (its labels must stay readable).
    status_bg: u32,
    status_fg: u32,
    /// A retained copy of what is on the text grid, `MAX_LINES` rows of
    /// `CHARS_PER_LINE` cells. The pixels are still the display; this is the
    /// model the shell needs to place things *at* a cell and to answer "what
    /// character is here" (a selection copied off the screen). Kept in sync by
    /// `write_char`, `set_cell`, `scroll` and `clear`.
    grid: [u8; GRID_CELLS],
    /// Status-bar labels, kept so `clear` can repaint the bar without the shell.
    status_left: [u8; LABEL_BYTES],
    status_left_len: usize,
    status_right: [u8; LABEL_BYTES],
    status_right_len: usize,
    /// Bounding box of cells changed since the last flush, as `(x0, y0, x1, y1)`
    /// inclusive. `None` when nothing has been drawn.
    dirty: Option<(usize, usize, usize, usize)>,
    /// Escape-sequence state, across `write_str` calls (see [`AnsiFilter`]).
    ansi: AnsiFilter,
}

impl FramebufferWriter {
    /// Build the writer for `screen`, drawing the first frame into the shadow
    /// surface and flushing it to the scanout surface.
    ///
    /// `back` and `scanout` are the addresses of the static surfaces; the
    /// geometry they must have is this module's `SURFACE_BYTES`.
    ///
    /// The geometry check is an `assert!` — it kills the machine, because
    /// `panic = "abort"` — and that is deliberate: the alternative is a writer
    /// whose every pixel access is out of bounds for a surface nobody can
    /// resize, i.e. memory corruption instead of a message. It cannot fire as
    /// the code stands (`init` passes these constants to `ramfb::configure` and
    /// gets them back unchanged); it is here so that a future caller cannot
    /// hand `new` a different mode and have `clear` walk off the array.
    fn new(screen: ramfb::Screen, back: usize, scanout: usize) -> Self {
        assert!(
            screen.width == SCREEN_WIDTH
                && screen.height == SCREEN_HEIGHT
                && screen.bpp == ramfb::BYTES_PER_PIXEL
                && screen.stride == SCREEN_WIDTH * ramfb::BYTES_PER_PIXEL
                && screen.surface_bytes() == SURFACE_BYTES,
            "framebuffer geometry does not match the static surfaces"
        );
        let mut writer = FramebufferWriter {
            scanout,
            back,
            width: screen.width,
            height: screen.height,
            pitch: screen.stride,
            bpp: screen.bpp,
            col: 0,
            row: 0,
            fg_color: DEFAULT_FG,
            bg_color: DEFAULT_BG,
            status_bg: 0x1B_3A5B, // slate blue, the x86 console's bar
            status_fg: 0xE6_F0FF,
            grid: [b' '; GRID_CELLS],
            status_left: [0; LABEL_BYTES],
            status_left_len: 0,
            status_right: [0; LABEL_BYTES],
            status_right_len: 0,
            dirty: None,
            ansi: AnsiFilter::new(),
        };
        writer.set_status_labels(DEFAULT_STATUS_LEFT, DEFAULT_STATUS_RIGHT);
        // The first frame is a full clear: it paints the background, resets the
        // model and repaints the status bar, and marks the whole surface dirty,
        // so the flush that follows puts a complete, consistent frame on the
        // device even though the surface's contents were arbitrary until now.
        writer.clear();
        writer.flush();
        writer
    }

    /// Blank the screen, reset the text model, and put the status bar back.
    ///
    /// The bar is part of the frame, not an overlay: it lives outside the text
    /// grid (`text_rows` reserves it) and is wiped by the same fill, so anything
    /// that clears the screen has to repaint it — otherwise the shell's `clear`
    /// command would silently delete the bar for the rest of the session.
    fn clear(&mut self) {
        self.col = 0;
        self.row = 0;
        for cell in self.grid.iter_mut() {
            *cell = b' ';
        }
        let bytes = [
            (self.bg_color & 0xFF) as u8,
            ((self.bg_color >> 8) & 0xFF) as u8,
            ((self.bg_color >> 16) & 0xFF) as u8,
        ];
        for row in 0..self.height {
            let base = row * self.pitch;
            for px in 0..self.width {
                let offset = base + px * self.bpp;
                // SAFETY: `offset + 2 < pitch * height` for every pixel of the
                // geometry asserted in `new`, and `back` is the address of the
                // `SURFACE_BYTES`-long shadow static.
                unsafe {
                    let pixel = (self.back as *mut u8).add(offset);
                    pixel.write(bytes[0]);
                    pixel.add(1).write(bytes[1]);
                    pixel.add(2).write(bytes[2]);
                }
            }
        }
        self.dirty = Some((
            0,
            0,
            self.width.saturating_sub(1),
            self.height.saturating_sub(1),
        ));
        self.paint_status_bar();
    }

    /// Record `ch` at grid cell `(col, row)`; out-of-range writes are dropped.
    fn set_cell(&mut self, col: usize, row: usize, ch: u8) {
        if col < CHARS_PER_LINE && row < MAX_LINES {
            self.grid[row * CHARS_PER_LINE + col] = ch;
        }
    }

    /// The character recorded at grid cell `(col, row)`, or a space outside the
    /// grid. This is what lets a cell be repainted *from the model* instead of
    /// from an assumption about what is on the screen.
    fn cell_char(&self, col: usize, row: usize) -> u8 {
        if col < CHARS_PER_LINE && row < MAX_LINES {
            self.grid[row * CHARS_PER_LINE + col]
        } else {
            b' '
        }
    }

    /// Repaint cell `(col, row)` from the model, **without moving the text
    /// cursor**.
    ///
    /// `write_char` cannot touch a cell that is not the current one — it advances
    /// `col`/`row`, so using it to erase would walk the console forward and
    /// eventually scroll. This paints in place; backspace uses it so the erased
    /// cell shows whatever the model says (a space) rather than a glyph the
    /// caller guessed.
    fn repaint_cell(&mut self, col: usize, row: usize) {
        let ch = self.cell_char(col, row);
        self.draw_char(ch, col, row);
    }

    /// Append one byte of console output, interpreting the escape sequences the
    /// kernel emits (see [`AnsiFilter`]).
    fn write_byte(&mut self, byte: u8) {
        match self.ansi.feed(byte) {
            AnsiAction::Print(byte) => self.write_char(byte),
            AnsiAction::Clear => self.clear(),
            AnsiAction::Home => {
                self.col = 0;
                self.row = 0;
            }
            AnsiAction::Swallow => {}
        }
    }

    /// Append a string of console output (escape sequences included).
    fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }

    fn write_char(&mut self, ch: u8) {
        let max_rows = self.text_rows();
        match ch {
            b'\n' => {
                self.col = 0;
                self.row += 1;
                if self.row >= max_rows {
                    self.scroll();
                }
            }
            b'\r' => {
                self.col = 0;
            }
            0x08 => {
                // Backspace: erase the cell from the model, then put the cursor
                // back on it. The order matters — painting first would paint the
                // character the model still holds.
                if self.col > 0 {
                    self.col -= 1;
                    self.set_cell(self.col, self.row, b' ');
                    self.repaint_cell(self.col, self.row);
                }
            }
            32..=126 => {
                if self.col >= CHARS_PER_LINE {
                    self.col = 0;
                    self.row += 1;
                    if self.row >= max_rows {
                        self.scroll();
                    }
                }
                self.draw_char(ch, self.col, self.row);
                self.set_cell(self.col, self.row, ch);
                self.col += 1;
            }
            // Anything else has no glyph and no effect: a UTF-8 byte from the
            // banner's em dash must not become a box or a stray control action.
            _ => {}
        }
    }

    /// Number of text rows the console may use, leaving room for the bottom
    /// status bar.
    ///
    /// Derived from the actual screen height, so a shorter mode reserves a row
    /// for the bar instead of overdrawing or clipping it, and clamped to
    /// `MAX_LINES` because the grid model is a fixed array.
    fn text_rows(&self) -> usize {
        text_rows_for(self.height)
    }

    fn draw_char(&mut self, ch: u8, col: usize, row: usize) {
        let x = col * CHAR_WIDTH;
        let y = row * CHAR_HEIGHT;
        let glyph = get_glyph(ch);
        let (fg, bg) = (self.fg_color, self.bg_color);
        for dy in 0..CHAR_HEIGHT {
            if y + dy >= self.height {
                break;
            }
            for dx in 0..CHAR_WIDTH {
                if x + dx >= self.width {
                    break;
                }
                let cov = glyph_coverage(glyph, dx, dy);
                self.put_pixel(x + dx, y + dy, blend_px(bg, fg, cov));
            }
        }
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        if self.bpp < 3 {
            return;
        }
        let offset = y * self.pitch + x * self.bpp;
        // SAFETY: `x < width` and `y < height`, so `offset + 2` is inside
        // `pitch * height == SURFACE_BYTES` (the pitch is the screen's tight
        // stride, asserted in `new`), and `back` is that static's address.
        unsafe {
            let pixel = (self.back as *mut u8).add(offset);
            pixel.write((color & 0xFF) as u8);
            pixel.add(1).write(((color >> 8) & 0xFF) as u8);
            pixel.add(2).write(((color >> 16) & 0xFF) as u8);
        }
        self.mark_dirty(x, y);
    }

    /// Extend the pending-flush rectangle to include `(x, y)`.
    fn mark_dirty(&mut self, x: usize, y: usize) {
        self.dirty = Some(match self.dirty {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }

    /// Push everything drawn since the last flush to the scanout surface.
    ///
    /// Copies only the tracked bounding box rather than the whole surface: a
    /// typed character touches one text row, and copying four megabytes for that
    /// would be work QEMU under TCG charges the guest for.
    ///
    /// The box is trustworthy because **every** write to the shadow surface goes
    /// through [`Self::put_pixel`] (which widens it) and the two bulk fills widen
    /// it explicitly: [`Self::clear`] marks the whole surface, [`Self::scroll`]
    /// the whole text area. A new drawing primitive that bypasses `put_pixel`
    /// would show as a stale rectangle.
    fn flush(&mut self) -> bool {
        let Some((x0, y0, x1, y1)) = self.dirty.take() else {
            return false;
        };
        if self.bpp < 3 || self.width == 0 || self.height == 0 {
            return false;
        }
        // `mark_dirty` only ever records in-bounds pixels, but the box is clipped
        // here anyway: a future caller that sets `dirty` by hand must not be able
        // to index outside the surface.
        let x1 = x1.min(self.width - 1);
        let y1 = y1.min(self.height - 1);
        if x0 >= self.width || y0 >= self.height || x0 > x1 || y0 > y1 {
            return false;
        }
        self.flush_rect(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
        true
    }

    /// Copy one rectangle of the shadow surface to the scanout surface.
    ///
    /// Row by row, because the two surfaces have the same stride and a rectangle
    /// is not contiguous: a single linear copy would drag the rest of each row
    /// along. The rectangles are clipped to the surface; an empty intersection is
    /// a no-op.
    fn flush_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.bpp < 3 || w == 0 || h == 0 || x >= self.width || y >= self.height {
            return;
        }
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        let row_bytes = (x1 - x) * self.bpp;
        let base = y * self.pitch + x * self.bpp;
        for row in 0..(y1 - y) {
            let offset = base + row * self.pitch;
            // SAFETY: the last byte copied is at `offset + row_bytes - 1` with
            // `row < y1 - y <= height - y` and `x1 <= width`, so it stays inside
            // `pitch * height`; both surfaces are `SURFACE_BYTES` long and do not
            // overlap (two distinct statics), so the row spans cannot alias.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (self.back as *const u8).add(offset),
                    (self.scanout as *mut u8).add(offset),
                    row_bytes,
                );
            }
        }
    }

    /// Scroll the text area up by one row, leaving the status bar alone.
    fn scroll(&mut self) {
        let rows = self.text_rows();
        let line_bytes = CHAR_HEIGHT * self.pitch;
        let text_px = rows * CHAR_HEIGHT;
        let moved = (text_px - CHAR_HEIGHT) * self.pitch;
        // Row by row, top-down: destination row `i` receives source row `i + 1`,
        // which is below it, so each pair of spans is disjoint and the copy
        // cannot overwrite a row it has not read yet.
        for offset in (0..moved).step_by(self.pitch) {
            // SAFETY: `offset + pitch <= moved + pitch == line_bytes + moved`,
            // i.e. inside the shadow surface for the geometry asserted in `new`
            // (`text_px <= height - STATUS_BAR_HEIGHT`); source and destination
            // are one line apart and therefore disjoint.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (self.back as *const u8).add(offset + line_bytes),
                    (self.back as *mut u8).add(offset),
                    self.pitch,
                );
            }
        }
        // The row that scrolled in from below is empty. It is filled with the
        // console background rather than zeroed: with a themed background a
        // black line would be visible until text reaches it (the x86 console
        // zeroes it, and only looks right because its background is black).
        let bytes = [
            (self.bg_color & 0xFF) as u8,
            ((self.bg_color >> 8) & 0xFF) as u8,
            ((self.bg_color >> 16) & 0xFF) as u8,
        ];
        let last = (text_px - CHAR_HEIGHT) * self.pitch;
        for row in 0..CHAR_HEIGHT {
            let base = last + row * self.pitch;
            for px in 0..self.width {
                let offset = base + px * self.bpp;
                // SAFETY: the same bounds as the copies above — `last` starts the
                // final text row, and the text area ends above the status bar.
                unsafe {
                    let pixel = (self.back as *mut u8).add(offset);
                    pixel.write(bytes[0]);
                    pixel.add(1).write(bytes[1]);
                    pixel.add(2).write(bytes[2]);
                }
            }
        }
        self.dirty = Some((
            0,
            0,
            self.width.saturating_sub(1),
            text_px.saturating_sub(1),
        ));
        // The model must move with the pixels: a cell read back after a scroll
        // would otherwise describe the row that used to be there.
        for row in 0..rows.saturating_sub(1) {
            let src = (row + 1) * CHARS_PER_LINE;
            let dst = row * CHARS_PER_LINE;
            for col in 0..CHARS_PER_LINE {
                self.grid[dst + col] = self.grid[src + col];
            }
        }
        let last = (rows - 1) * CHARS_PER_LINE;
        for col in 0..CHARS_PER_LINE {
            self.grid[last + col] = b' ';
        }
        self.row = rows - 1;
    }

    /// Fill an axis-aligned rectangle with a solid colour.
    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for py in y..y1 {
            for px in x..x1 {
                self.put_pixel(px, py, color);
            }
        }
    }

    /// Draw one glyph at an arbitrary pixel position with explicit fg/bg (bg is
    /// painted for the unlit pixels). Used by the status bar, which needs
    /// pixel-precise text outside the text grid.
    fn draw_glyph_px(&mut self, ch: u8, px: usize, py: usize, fg: u32, bg: u32) {
        let glyph = get_glyph(ch);
        for dy in 0..CHAR_HEIGHT {
            for dx in 0..CHAR_WIDTH {
                let cov = glyph_coverage(glyph, dx, dy);
                self.put_pixel(px + dx, py + dy, blend_px(bg, fg, cov));
            }
        }
    }

    /// Draw an ASCII string at a pixel position (left to right). Non-printable
    /// bytes render as the font's fallback box.
    fn draw_text_px(&mut self, px: usize, py: usize, s: &str, fg: u32, bg: u32) {
        let mut x = px;
        for byte in s.bytes() {
            self.draw_glyph_px(byte, x, py, fg, bg);
            x += CHAR_WIDTH;
        }
    }

    /// Keep the bar's labels for the next repaint (see [`Self::clear`]).
    fn set_status_labels(&mut self, left: &str, right: &str) {
        let take = |label: &str, dst: &mut [u8; LABEL_BYTES]| -> usize {
            let len = label.len().min(LABEL_BYTES);
            dst[..len].copy_from_slice(&label.as_bytes()[..len]);
            len
        };
        self.status_left_len = take(left, &mut self.status_left);
        self.status_right_len = take(right, &mut self.status_right);
    }

    /// Repaint the reserved strip at the bottom of the screen: the console's
    /// own chrome, with left- and right-aligned labels.
    fn paint_status_bar(&mut self) {
        let top = self.height.saturating_sub(STATUS_BAR_HEIGHT);
        if top + STATUS_BAR_HEIGHT > self.height {
            return;
        }
        let (bg, fg) = (self.status_bg, self.status_fg);
        // The labels are copied out first: `draw_text_px` needs `&mut self`, and
        // the labels live in `self`.
        let mut left = [0u8; LABEL_BYTES];
        let mut right = [0u8; LABEL_BYTES];
        left.copy_from_slice(&self.status_left);
        right.copy_from_slice(&self.status_right);
        let left_len = self.status_left_len;
        let right_len = self.status_right_len;
        let left = core::str::from_utf8(&left[..left_len]).unwrap_or("");
        let right = core::str::from_utf8(&right[..right_len]).unwrap_or("");

        self.fill_rect(0, top, self.width, STATUS_BAR_HEIGHT, bg);
        // Separator highlight on top of the bar, one shade lighter, so the bar
        // reads as chrome rather than as another line of output.
        let edge = mix_rgb(bg, fg, 60);
        for px in 0..self.width {
            self.put_pixel(px, top, edge);
        }
        let ty = top + (STATUS_BAR_HEIGHT - CHAR_HEIGHT) / 2;
        self.draw_text_px(6, ty, left, fg, bg);
        let right_px = right.len() * CHAR_WIDTH;
        if right_px + 6 < self.width {
            self.draw_text_px(self.width - right_px - 6, ty, right, fg, bg);
        }
    }
}

/// Blend `b` into `a` by `t/256`, channel-wise. Used for the status bar's edge
/// shade so the bar's two colours stay the only ones that have to be stated.
fn mix_rgb(a: u32, b: u32, t: u32) -> u32 {
    let channel = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        (x * (256 - t) + y * t) >> 8
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

impl fmt::Write for FramebufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

/// The console handle: the writer behind one lock, plus the frame boundary.
struct FbWriter {
    inner: Spinlock<Option<FramebufferWriter>>,
}

impl FbWriter {
    const fn new() -> Self {
        FbWriter {
            inner: Spinlock::new(None),
        }
    }

    /// Whether a writer has been installed (i.e. whether the device answered).
    fn is_live(&self) -> bool {
        self.inner.lock().is_some()
    }

    /// Append text to the shadow surface. Does **not** push the frame: a bare
    /// `write_str` is a fragment of a formatted call, and the frame boundary is
    /// [`Self::write_fmt_now`].
    fn write_str(&self, s: &str) {
        if let Some(writer) = self.inner.lock().as_mut() {
            writer.write_string(s);
        }
    }

    /// Write formatted arguments and push the result to the device.
    ///
    /// This is the `_print` path. Without the flush the text only lands in the
    /// shadow surface and the screen stays a frame behind: a prompt printed with
    /// `kprint!` never reaches the device until the *next* newline, which is
    /// exactly how a boot log ends up looking truncated.
    fn write_fmt_now(&self, args: fmt::Arguments) {
        use core::fmt::Write;
        if let Some(writer) = self.inner.lock().as_mut() {
            let _ = writer.write_fmt(args);
            writer.flush();
        }
    }
}

static FB_WRITER: FbWriter = FbWriter::new();

/// Bring up the console's screen: probe fw_cfg, configure the ramfb device, and
/// install the writer with a first frame drawn.
///
/// Called from [`crate::console::init`] — the console has two sinks and one
/// bring-up, so a machine without the device needs no separate boot phase: the
/// probe simply fails, the reason goes to the serial line, and every framebuffer
/// call below becomes a no-op. Returns whether the screen is live.
///
/// The probe is a read of a fixed MMIO address (0x1010_0000, the `virt`
/// machine's fw_cfg; see [`fw_cfg::BASE`]). On a machine that has no device there
/// the load faults rather than returning a value — this milestone's target is
/// QEMU `virt`, where it is always present, and the serial console never depends
/// on it.
pub fn init() -> bool {
    // Idempotent: a second call finds the writer already installed and does
    // nothing, so the boot path cannot reconfigure (and briefly blank) a live
    // screen.
    if FB_WRITER.is_live() {
        return true;
    }
    let base = fw_cfg::BASE;
    let Some(device) = fw_cfg::FwCfg::probe_at(base) else {
        crate::kprintln!("framebuffer  no fw_cfg at {base:#x} — serial console only");
        return false;
    };
    let scanout = core::ptr::addr_of_mut!(SCANOUT) as usize;
    let back = core::ptr::addr_of_mut!(SHADOW) as usize;
    let screen = match ramfb::configure(&device, scanout, SCREEN_WIDTH, SCREEN_HEIGHT) {
        Ok(screen) => screen,
        Err(error) => {
            crate::kprintln!("framebuffer  ramfb: {error} — serial console only");
            return false;
        }
    };
    let writer = FramebufferWriter::new(screen, back, scanout);
    {
        let mut guard = FB_WRITER.inner.lock();
        if guard.is_some() {
            return true;
        }
        *guard = Some(writer);
    }
    // Logged with no lock held (the fan-out re-enters this module).
    crate::kprintln!(
        "framebuffer  ramfb {}x{} stride {} cells {}x{}",
        screen.width,
        screen.height,
        screen.stride,
        CHARS_PER_LINE,
        text_rows_for(screen.height),
    );
    true
}

/// Rows the console may use on a screen `height` pixels tall.
///
/// One function rather than two: the boot log prints this number and the blitter
/// lays out with it, and a log that disagrees with the screen is worse than no
/// log at all.
fn text_rows_for(height: usize) -> usize {
    let available = height.saturating_sub(STATUS_BAR_HEIGHT) / CHAR_HEIGHT;
    available.min(MAX_LINES).max(1)
}

/// Append text to the framebuffer sink without pushing the frame.
///
/// The console fan-out calls this for every chunk it also sends to the UART; the
/// frame is pushed by [`_print`] once per formatted call.
pub fn write_str(s: &str) {
    FB_WRITER.write_str(s);
}

/// Print formatted text into the framebuffer and push the frame.
///
/// The `kprintln!`/`kprint!` fan-out in [`crate::console`] routes here, so one
/// formatted call is one frame: text appears when the call returns, not at the
/// next newline.
pub fn _print(args: fmt::Arguments) {
    FB_WRITER.write_fmt_now(args);
}
