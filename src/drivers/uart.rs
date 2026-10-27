//! NS16550-compatible UART — the `virt` machine's console.
//!
//! QEMU's `virt` machine maps a 16550 at a fixed physical address, byte-wide
//! registers with a 1-byte stride (the "no shift" variant; the older PC layout
//! used a 4-byte stride). Output is polled: the transmitter's holding register
//! empty bit is the only "done" signal a 16550 gives, and there is nothing useful
//! to do while waiting for it. **Input is interrupt-driven**: with the PLIC
//! routing the receive line, the external-interrupt handler drains the FIFO into
//! a single-producer/single-consumer ring and the shell sleeps in `wfi` until a
//! byte arrives. The old polling path is kept and is what a machine without a
//! PLIC uses — the console must work before the tree is parsed and after it
//! disagrees with the hardware.
//!
//! # The lock contract (why output masks interrupts)
//!
//! [`UART`] serialises the transmitter so two contexts cannot interleave halves
//! of two messages. It is a non-reentrant spinlock, and the trap handler can
//! print (the exception path does) — so every critical section masks interrupts
//! first (`csr::without_interrupts`) and restores the caller's state afterwards.
//! Without that, a timer tick taken inside `put_byte` would let a handler that
//! prints spin forever on the lock the interrupted context holds. The receive
//! ring deliberately has **no** lock for the same reason: its producer runs in
//! trap context and could otherwise interrupt its own consumer mid-update.
//!
//! Order of operations matters on a 16550: set the divisor latch *before*
//! enabling the FIFO, or the FIFO takes the divisor byte; and never write the
//! transmit register when the line status says the holding register is full.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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

/// `IER` bit 0: interrupt when a received byte is available.
const IER_RX_AVAILABLE: u8 = 1 << 0;

/// Bytes the 16550's receiver FIFO can hold; the drain loop is bounded by this so
/// the interrupt handler cannot spin on a misbehaving FIFO.
const FIFO_DEPTH: usize = 16;

/// Capacity of the receive ring: a power of two so wrapping is a mask, and 256
/// because that is eight shell lines — a command burst sent in one write survives
/// however slowly the shell echoes it.
const RING_CAPACITY: usize = 256;

/// Guards the transmitter so two harts (or a trap and the shell) cannot
/// interleave halves of two messages on the wire.
static UART: Spinlock<()> = Spinlock::new(());

/// Received bytes, from the interrupt handler to the shell.
static RX: RxRing = RxRing::new();
/// Bytes dropped because [`RX`] was full.
static RX_OVERRUNS: AtomicU64 = AtomicU64::new(0);
/// Whether receive interrupts are enabled (the ring is authoritative for input).
static RX_IRQ_ENABLED: AtomicBool = AtomicBool::new(false);

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

/// A single-producer/single-consumer byte ring.
///
/// One writer (the UART interrupt handler) and one reader (the shell) are a
/// SPSC ring's exact premise, and the indices are the synchronisation: the
/// producer publishes a slot with a `Release` store of `head`, the consumer takes
/// it with an `Acquire` load. No lock — a lock here would be a deadlock waiting to
/// happen, because the producer runs in trap context.
struct RxRing {
    bytes: UnsafeCell<[u8; RING_CAPACITY]>,
    /// Index of the next slot the producer will write.
    head: AtomicUsize,
    /// Index of the next slot the consumer will read.
    tail: AtomicUsize,
}

// SAFETY: writes to `bytes` happen only in `push`, by the one producer, to the
// slot at `head` after checking it is free; reads happen only in `pop`, by the
// one consumer, from the slot at `tail` after `head` published it. The
// acquire/release pairing on the indices is what makes the slot visible in the
// right order; no other context touches the buffer.
unsafe impl Sync for RxRing {}

impl RxRing {
    const fn new() -> Self {
        RxRing {
            bytes: UnsafeCell::new([0; RING_CAPACITY]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer side: append one byte, or report a full ring.
    ///
    /// Full means the byte is dropped by the caller (counted, never blocked on):
    /// blocking in a trap handler is how an interrupt storm becomes a hang.
    fn push(&self, byte: u8) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= RING_CAPACITY {
            return false;
        }
        // SAFETY: the ring is not full (`head - tail < CAPACITY`), so the slot is
        // one the consumer has released; only this producer writes it, and it is
        // published below before any consumer can observe the new `head`.
        unsafe {
            (*self.bytes.get())[head & (RING_CAPACITY - 1)] = byte;
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side: take one byte, or report an empty ring.
    fn pop(&self) -> Option<u8> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: `head != tail`, so the producer wrote this slot and published
        // it with `Release`; only this consumer reads it and advances `tail`.
        let byte = unsafe { (*self.bytes.get())[tail & (RING_CAPACITY - 1)] };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(byte)
    }

    fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Relaxed)
    }
}

/// Configure the UART for 8N1 at the firmware's baud rate (QEMU ignores it, a
/// real board would not, so the divisor is still programmed).
///
/// Interrupts stay off here: [`enable_rx_interrupt`] is a separate step because it
/// must not run before the PLIC routes the line. The FIFO clear in this function
/// is also why the smoke driver waits for the banner before typing — bytes that
/// arrive before the kernel configures the part are discarded.
pub fn init() {
    write(IER, 0x00); // no interrupts yet: receive polling is the boot path
    write(LCR, 0x80); // DLAB=1 -> the next two writes are the divisor
    write(DLL, 0x01); // divisor 1: the fastest the part allows
    write(DLM, 0x00);
    write(LCR, 0x03); // 8 bits, no parity, one stop bit, DLAB=0
    write(FCR, 0x07); // enable and clear both FIFOs
}

/// Switch the receive path to interrupts. Call only after the PLIC has enabled
/// this UART's source and before the shell starts reading.
///
/// Bytes already in the FIFO are moved into the ring first: input that arrived
/// while the controller was being configured must not be stranded behind a now
/// authoritative ring.
pub fn enable_rx_interrupt() {
    drain_fifo();
    RX_IRQ_ENABLED.store(true, Ordering::Release);
    write(IER, IER_RX_AVAILABLE);
}

/// Drain the receiver FIFO into the ring.
///
/// This is the external-interrupt handler's whole job. It runs in trap context:
/// no lock, no allocation, no output, and a loop bounded by [`FIFO_DEPTH`] so a
/// FIFO that never reports empty cannot pin the hart. A byte that does not fit
/// the ring is counted in [`rx_overruns`] and dropped — the UART's own failure
/// mode, not a second, quieter one.
pub fn handle_interrupt() {
    drain_fifo();
}

/// How many received bytes were dropped because the ring was full (diagnostic).
pub fn rx_overruns() -> u64 {
    RX_OVERRUNS.load(Ordering::Relaxed)
}

/// Whether receive interrupts are routed through the ring.
pub fn rx_interrupt_enabled() -> bool {
    RX_IRQ_ENABLED.load(Ordering::Acquire)
}

fn drain_fifo() {
    let mut drained = 0;
    while drained < FIFO_DEPTH && read(LSR) & LSR_DATA_READY != 0 {
        if !RX.push(read(RBR)) {
            RX_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
        drained += 1;
    }
}

/// Write one byte, waiting for room in the transmit holding register.
///
/// Interrupts are masked for the whole critical section, and that is a
/// correctness requirement, not tidiness: the lock is not reentrant, and once an
/// interrupt source is enabled the trap handler can print to this same console.
/// A trap taken while the interrupted context holds `UART` would spin forever
/// waiting for a lock that cannot be released until the handler returns — the
/// deadlock `docs/architecture.md` §10.1 predicted for M3. Masking is also what
/// lets the transmit wait below be an ordinary spin: it cannot be preempted into
/// a handler that needs the same lock.
///
/// `without_interrupts` restores the *previous* state, so calling this from trap
/// context (where interrupts are already masked) is not just safe, it is the
/// intended path for a handler's diagnostics.
pub fn put_byte(byte: u8) {
    crate::arch::riscv64::csr::without_interrupts(|| {
        let _guard = UART.lock();
        while read(LSR) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        write(RBR, byte);
    });
}

/// Read one byte if one has arrived, without blocking.
///
/// Once receive interrupts are enabled this consumes the ring and nothing else:
/// the handler is the sole reader of the receiver FIFO, and polling it from here
/// as well would race the handler between the line-status and data-register reads
/// (it can drain the byte in between, leaving `RBR` stale). Before that switch —
/// and on a machine where the line is not routed — it polls exactly as M1 did.
pub fn try_get_byte() -> Option<u8> {
    if RX_IRQ_ENABLED.load(Ordering::Acquire) {
        RX.pop()
    } else if read(LSR) & LSR_DATA_READY == 0 {
        None
    } else {
        Some(read(RBR))
    }
}

/// Block until a byte arrives.
///
/// With the interrupt routed this is an *idle* wait: the shell sleeps in `wfi` and
/// the UART interrupt wakes it. The timer is armed too, so even the lost-wakeup
/// window (a byte pushed between the ring check and the `wfi`) costs at most one
/// tick — the loop re-checks after every wake-up. Without the interrupt it is the
/// old spin on the line-status register.
pub fn wait_byte() -> u8 {
    loop {
        if let Some(byte) = try_get_byte() {
            return byte;
        }
        if RX_IRQ_ENABLED.load(Ordering::Acquire) {
            // SAFETY: `wfi` is a hint; it cannot fault and needs no state set up.
            // Every enabled source returns through `trap_entry`/`sret`, so a
            // pending interrupt resumes exactly at the loop's next iteration.
            unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Block until a byte arrives, by polling.
///
/// Kept as the explicit fallback API: a caller that cannot tolerate `wfi` (or a
/// machine whose PLIC line is not routed) still has the M1 behaviour. Prefer
/// [`wait_byte`], which becomes an idle wait when the interrupt exists.
pub fn get_byte() -> u8 {
    loop {
        if let Some(byte) = try_get_byte() {
            return byte;
        }
        core::hint::spin_loop();
    }
}

/// Selftest: run bytes through the real ring and read them back.
///
/// Exercises the producer/consumer pair the interrupt handler and the shell use,
/// including the wrap boundary and the "full ring refuses" case. Non-destructive:
/// bytes already queued (a user may type ahead while the suite runs) are drained
/// first and restored in order afterwards, so the selftest cannot eat input.
pub fn selftest_ring() -> bool {
    /// Pending input this test is willing to save and restore. The shell's line
    /// buffer is 128 bytes; 64 queued bytes is already far more typing ahead than
    /// a human or the smoke driver ever produces.
    const SAVED: usize = 64;

    let mut saved = [0u8; SAVED];
    let mut saved_len = 0;
    while saved_len < SAVED {
        match RX.pop() {
            Some(byte) => {
                saved[saved_len] = byte;
                saved_len += 1;
            }
            None => break,
        }
    }

    let mut ok = true;
    // Fill exactly to capacity without wrapping, then prove a full ring refuses.
    for i in 0..RING_CAPACITY {
        if !RX.push((i & 0xff) as u8) {
            ok = false;
            break;
        }
    }
    if RX.push(0x5a) {
        crate::kprintln!("[selftest] uart ring: accepted a byte into a full ring");
        ok = false;
    }
    for i in 0..RING_CAPACITY {
        match RX.pop() {
            Some(byte) if byte == (i & 0xff) as u8 => {}
            _ => {
                crate::kprintln!("[selftest] uart ring: byte {i} did not read back");
                ok = false;
                break;
            }
        }
    }
    // A second pass with the indices moved off zero: the wrap arithmetic is the
    // part of a ring that is wrong in a way small tests miss.
    for i in 0..8u8 {
        if !RX.push(i) {
            ok = false;
        }
    }
    for i in 0..8u8 {
        if RX.pop() != Some(i) {
            crate::kprintln!("[selftest] uart ring: wrapped byte {i} did not read back");
            ok = false;
            break;
        }
    }
    while RX.pop().is_some() {}
    if !RX.is_empty() {
        crate::kprintln!("[selftest] uart ring: not empty after draining");
        ok = false;
    }
    for byte in saved[..saved_len].iter() {
        if !RX.push(*byte) {
            crate::kprintln!("[selftest] uart ring: could not restore queued input");
            ok = false;
            break;
        }
    }
    ok
}
