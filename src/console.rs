//! The kernel's console: one text stream, two sinks.
//!
//! The NS16550 UART and, when the machine has one, the ramfb screen carry the
//! *same* text: the serial line is the log of record (it is there before the
//! display, and it survives a screen that never came up), while the screen is
//! what a human — or `tools/run.py`, through a `screendump` — actually looks at.
//! The fan-out lives here, in the sink the `kprint!` macros are written against,
//! so nothing above this file has to know that a screen exists:
//!
//! * `kprint!`/`kprintln!` write to the UART and then to the framebuffer;
//! * the framebuffer is pushed to the device once per formatted call, because a
//!   `kprint!` is one logical print — the shell's prompt must appear when the
//!   prompt is printed, not at the next newline;
//! * ANSI escape sequences reach the UART unchanged (a terminal is entitled to
//!   them) and are interpreted by the framebuffer sink, which is why the shell's
//!   `clear` command clears both.
//!
//! [`init`] brings up both sinks. Keeping the display probe inside the console's
//! bring-up rather than in the boot sequence means there is no second phase to
//! order, and a machine without the device is not an error path: the probe
//! fails, [`set_framebuffer`]'s flag stays meaningful (writes become no-ops
//! rather than being routed anywhere else), and the kernel runs on the serial
//! line exactly as it did before this milestone.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::{framebuffer, uart};

/// Whether the framebuffer sink is part of the fan-out.
///
/// A separate switch from "the framebuffer is initialized" on purpose: a caller
/// that wants a serial-only transcript (a long diagnostic dump, a debug session
/// over a pipe) can exclude the screen without tearing the device down.
static FRAMEBUFFER: AtomicBool = AtomicBool::new(false);

/// Bring the console up. Safe to call once, from the boot hart.
///
/// Order is the design: the UART first, so it can carry the reason if the screen
/// does not appear; then the fan-out flag; then the screen. The flag goes up
/// before [`framebuffer::init`] so the driver's own success/failure line reaches
/// the screen too — the framebuffer sink is a no-op until the writer exists, so
/// nothing can be written into a console that is half-built.
pub fn init() {
    uart::init();
    set_framebuffer(true);
    framebuffer::init();
}

/// Include (`true`) or exclude (`false`) the framebuffer sink from the fan-out.
///
/// The UART is never switched off: it is the log of record, and a console that
/// can go silent is not a console.
pub fn set_framebuffer(on: bool) {
    FRAMEBUFFER.store(on, Ordering::Relaxed);
}

/// Send everything from here on to the serial line only.
///
/// For the trap handler's exception path, which must not deadlock: the screen
/// keeps its writer behind a non-reentrant spinlock, and an exception taken while
/// that lock is held (a fault inside the framebuffer code itself) would spin
/// forever trying to draw its own diagnostics. The screen is deliberately not
/// restored — the exception path prints and parks the hart; a machine that is
/// still running after it has no business being reachable anyway.
pub fn serial_only() {
    set_framebuffer(false);
}

/// Whether the framebuffer sink is currently part of the fan-out.
pub fn framebuffer_enabled() -> bool {
    FRAMEBUFFER.load(Ordering::Relaxed)
}

/// A `core::fmt::Write` sink that pushes bytes at the UART.
struct UartSink;

impl Write for UartSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            uart::put_byte(byte);
        }
        Ok(())
    }
}

/// The fan-out sink: the UART, then the screen.
struct ConsoleSink;

impl Write for ConsoleSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            uart::put_byte(byte);
        }
        if framebuffer_enabled() {
            // No flush here: a bare `write_str` is a fragment of a formatted
            // call, and the frame boundary belongs to that call (see `write_fmt`).
            framebuffer::write_str(s);
        }
        Ok(())
    }

    /// One `kprint!`/`kprintln!` is one frame.
    ///
    /// `Arguments` is `Copy`, so the same formatted text goes to both sinks
    /// without building an intermediate string (there is no allocator) and
    /// without formatting twice (`core::fmt` walks the arguments once per sink,
    /// which is what a fan-out costs).
    fn write_fmt(&mut self, args: fmt::Arguments) -> fmt::Result {
        let mut uart = UartSink;
        let result = uart.write_fmt(args);
        if framebuffer_enabled() {
            framebuffer::_print(args);
        }
        result
    }
}

/// Print formatted text to the console.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::console::sink(), $($arg)*);
    }};
}

/// Print formatted text to the console, with a newline.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::console::sink(), $($arg)*);
    }};
}

/// The console sink, for callers that want `write!` directly.
pub fn sink() -> impl Write {
    ConsoleSink
}

/// Write raw bytes to both sinks.
///
/// This is the `write(2)` path: a user buffer is not a Rust `str` and may hold
/// any byte. The UART gets the bytes verbatim; the screen runs them through the
/// same ANSI filter as kernel output, then flushes once so one syscall is one
/// frame.
pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        uart::put_byte(byte);
    }
    if framebuffer_enabled() {
        framebuffer::write_bytes(bytes);
    }
}
