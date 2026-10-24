//! NS16550-compatible UART — the `virt` machine's console.
//!
//! QEMU's `virt` machine maps a 16550 at a fixed physical address, byte-wide
//! registers with a 1-byte stride (the "no shift" variant; the older PC layout
//! used a 4-byte stride). It is polled, not interrupt-driven: the kernel spends
//! its life in a shell that waits for input, and a polled console cannot drop a
//! character because an interrupt handler was slow.
//!
//! Order of operations matters on a 16550: set the divisor latch *before*
//! enabling the FIFO, or the FIFO takes the divisor byte; and never write the
//! transmit register when the line status says the holding register is full.

use crate::sync::Spinlock;

/// Base address of the console UART on QEMU `virt`.
///
/// The device tree says the same thing (`/soc/serial@10000000`); the constant is
/// kept because the console is needed *before* the tree is parsed, and a wrong
/// address here fails loudly (no output) rather than silently.
pub const BASE: usize = 0x1000_0000;

const RBR: usize = 0; // receive buffer (read) / transmit holding (write)
const IER: usize = 1; // interrupt enable
const FCR: usize = 2; // FIFO control
const LCR: usize = 3; // line control
const LSR: usize = 5; // line status
const DLL: usize = 0; // divisor latch low (when DLAB=1)
const DLM: usize = 1; // divisor latch high

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Guards the transmitter so two harts (or a trap and the shell) cannot
/// interleave halves of two messages on the wire.
static UART: Spinlock<()> = Spinlock::new(());

#[inline]
fn reg(offset: usize) -> *mut u8 {
    (BASE + offset) as *mut u8
}

#[inline]
fn read(offset: usize) -> u8 {
    // SAFETY: the UART's registers are memory-mapped at `BASE`, an address the
    // platform guarantees to be device memory; each read is a single byte.
    unsafe { core::ptr::read_volatile(reg(offset)) }
}

#[inline]
fn write(offset: usize, value: u8) {
    // SAFETY: as above — one byte, device memory, no aliasing with kernel RAM.
    unsafe { core::ptr::write_volatile(reg(offset), value) }
}

/// Configure the UART for 8N1 at the firmware's baud rate (QEMU ignores it, a
/// real board would not, so the divisor is still programmed).
pub fn init() {
    write(IER, 0x00); // no interrupts: this driver polls
    write(LCR, 0x80); // DLAB=1 -> the next two writes are the divisor
    write(DLL, 0x01); // divisor 1: the fastest the part allows
    write(DLM, 0x00);
    write(LCR, 0x03); // 8 bits, no parity, one stop bit, DLAB=0
    write(FCR, 0x07); // enable and clear both FIFOs
}

/// Write one byte, waiting for room in the transmit holding register.
pub fn put_byte(byte: u8) {
    let _guard = UART.lock();
    while read(LSR) & LSR_THR_EMPTY == 0 {
        core::hint::spin_loop();
    }
    write(RBR, byte);
}

/// Read one byte if one has arrived, without blocking.
pub fn try_get_byte() -> Option<u8> {
    if read(LSR) & LSR_DATA_READY == 0 {
        None
    } else {
        Some(read(RBR))
    }
}

/// Block until a byte arrives.
pub fn get_byte() -> u8 {
    loop {
        if let Some(b) = try_get_byte() {
            return b;
        }
        core::hint::spin_loop();
    }
}
