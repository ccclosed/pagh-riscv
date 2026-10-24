//! The kernel's text console: the NS16550 UART, plus the `kprint!` macros.
//!
//! Serial is the only sink in this milestone. A framebuffer console comes later
//! and will sit beside it the way it does on x86 (the same macros fan out to
//! both), which is why the macros are written against a sink rather than against
//! the UART directly.

use core::fmt::{self, Write};

use crate::drivers::uart;

/// Bring the console up. Safe to call once, from the boot hart.
pub fn init() {
    uart::init();
}

/// A `core::fmt::Write` sink that pushes bytes at the UART.
struct UartSink;

impl Write for UartSink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            uart::put_byte(b);
        }
        Ok(())
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
    UartSink
}
