//! The interactive console: a line editor over the UART and a few commands.
//!
//! This is the kernel's first user-visible surface and the one every later
//! subsystem is tested through, so it stays deliberately dull: read a byte, echo
//! it, act on Enter. There is no history and no completion yet — what matters now
//! is that a human (or `tools/run.py`) can talk to the machine and get an answer
//! it can grep. The answer goes to `console`, which since M4 is the UART *and* the
//! ramfb text screen (see `src/console.rs`), so the same line is readable on the
//! serial log and on the screen.

use crate::arch::riscv64::csr;
use crate::drivers::uart;
use crate::fdt::Fdt;

/// Longest accepted input line. Longer lines are refused, not truncated: a
/// silently shortened command is worse than a rejected one.
const LINE_CAPACITY: usize = 128;

/// Run the console until `halt` is typed. Never returns: the kernel has nothing
/// else to schedule yet, so leaving the shell would leave the hart idle forever.
pub fn run(tree: Fdt) -> ! {
    crate::kprintln!();
    crate::kprintln!("Type 'help' for the command list.");
    crate::kprintln!();

    let mut line = Line::new();
    loop {
        crate::kprint!("pagh:rv64> ");
        loop {
            let byte = uart::get_byte();
            match byte {
                b'\r' | b'\n' => {
                    crate::kprintln!();
                    break;
                }
                // Backspace: erase the character on the terminal (the classic
                // destructive triplet) and in the buffer.
                0x7f | 0x08 => {
                    if line.backspace() {
                        crate::kprint!("\x08 \x08");
                    }
                }
                b if (0x20..0x7f).contains(&b) => {
                    if line.push(b) {
                        // Echo what was accepted; a byte that did not fit stays
                        // invisible so the terminal and the buffer agree.
                        crate::kprint!("{}", b as char);
                    }
                }
                // Control characters other than the ones handled above are
                // ignored on purpose: a stray `^C` must not desynchronise the
                // echo from the buffer.
                _ => {}
            }
        }
        let command = line.take();
        if !command.is_empty() {
            execute(command.as_str(), &tree);
        }
    }
}

/// A fixed-capacity input buffer with a destructive-backspace-friendly API.
struct Line {
    buf: [u8; LINE_CAPACITY],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Line {
            buf: [0; LINE_CAPACITY],
            len: 0,
        }
    }

    /// Append `byte`; false when the line is full.
    fn push(&mut self, byte: u8) -> bool {
        if self.len == LINE_CAPACITY {
            return false;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        true
    }

    /// Remove the last byte; false when the line is already empty (so the caller
    /// does not erase the prompt).
    fn backspace(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        self.len -= 1;
        true
    }

    fn take(&mut self) -> alloc_str::Str {
        let s = alloc_str::Str::from_bytes(&self.buf[..self.len]);
        self.len = 0;
        s
    }
}

/// A borrowed, `no_std`-only stand-in for `String`.
///
/// The kernel has no allocator yet; a command line is at most 128 bytes and lives
/// only as long as the dispatch, so the buffer is copied into a small fixed array
/// instead of being heap-allocated. When the heap arrives this can go away.
mod alloc_str {
    pub struct Str {
        buf: [u8; 128],
        len: usize,
    }

    impl Str {
        pub fn from_bytes(bytes: &[u8]) -> Self {
            let mut buf = [0u8; 128];
            let take = bytes.len().min(buf.len());
            buf[..take].copy_from_slice(&bytes[..take]);
            Str { buf, len: take }
        }

        pub fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }
}

/// Split on spaces, ignoring runs of them (`echo  a   b` prints `a b`).
fn split(line: &str) -> (Option<&str>, impl Iterator<Item = &str>) {
    let mut words = line.split_whitespace();
    let first = words.next();
    (first, words)
}

fn execute(line: &str, tree: &Fdt) {
    let (name, args) = split(line);
    let Some(name) = name else { return };
    match name {
        "help" => {
            crate::kprintln!("commands:");
            crate::kprintln!("  help          this list");
            crate::kprintln!("  mem           physical memory reported by the device tree");
            crate::kprintln!("  selftest      run the in-QEMU memory self-tests");
            crate::kprintln!("  time          the `time` CSR (timebase ticks) and its frequency");
            crate::kprintln!("  hart          the hart this kernel is running on");
            crate::kprintln!("  echo <text>   print the text back");
            crate::kprintln!("  clear         clear the screen (ANSI)");
            crate::kprintln!("  halt          power the machine off (SBI SRST)");
        }
        "mem" => {
            // The device tree's claim and the allocator's view are printed
            // together on purpose: when the two disagree, which one is wrong is
            // exactly the question, and the answer is in the pair of lines.
            match tree.memory() {
                Some((base, end)) => crate::kprintln!(
                    "memory {:#x}..{:#x} ({} MiB)",
                    base,
                    end,
                    (end - base) / (1024 * 1024)
                ),
                None => crate::kprintln!("mem: the device tree has no /memory node"),
            }
            let (free, total) = crate::memory::stats();
            let (pool_start, pool_end) = crate::memory::pool_range();
            crate::kprintln!(
                "pmm    {} free of {} frames ({} MiB), pool {:#x}..{:#x}",
                free,
                total,
                total * crate::memory::FRAME_SIZE / (1024 * 1024),
                pool_start,
                pool_end
            );
            let (used, mapped, chunks) = crate::heap_stats();
            crate::kprintln!(
                "heap   {} B used, {} B mapped from the pool in {} chunk(s) of {} KiB",
                used,
                mapped,
                chunks,
                crate::memory::heap::CHUNK_SIZE / 1024
            );
        }
        // The memory suite: frames, frame contents, the heap. It is written to be
        // non-destructive and idempotent on purpose, so running it twice in one
        // boot is a real check instead of a state mutation — the free-frame ledger
        // printed at the end of each run must read the same both times
        // (`src/memory/selftest.rs`).
        "selftest" => {
            crate::memory::selftest::run();
        }
        "time" => {
            let hz = tree.timebase_frequency().unwrap_or(0);
            let ticks = csr::read_time();
            crate::kprintln!(
                "time {} ticks (~{} s at {} Hz)",
                ticks,
                ticks / hz.max(1) as usize,
                hz
            );
        }
        "hart" => crate::kprintln!("hart {}", crate::arch::riscv64::hart_id()),
        "echo" => {
            // Joined with single spaces, like the x86 shell: `echo  a   b` prints
            // "a b", which makes the command useful for checking argument
            // splitting rather than reproducing the user's whitespace.
            let mut first = true;
            for word in args {
                if !first {
                    crate::kprint!(" ");
                }
                crate::kprint!("{}", word);
                first = false;
            }
            crate::kprintln!();
        }
        "clear" => crate::kprint!("\x1b[2J\x1b[H"),
        "halt" => {
            // Ask firmware to power the machine off (SBI SRST). Parking the hart
            // instead would leave QEMU alive: a `wfi`-ing machine is still a
            // machine, and the help text promised otherwise.
            crate::kprintln!("halt: powering off");
            crate::arch::riscv64::sbi::shutdown()
        }
        other => {
            crate::kprintln!("{}: unknown command (try 'help')", other);
        }
    }
}
