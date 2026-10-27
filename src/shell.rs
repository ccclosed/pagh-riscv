//! The interactive console: a line editor over the UART and the command set.
//!
//! This is the kernel's first user-visible surface and the one every subsystem is
//! tested through, so it stays deliberately dull: read a byte, echo it, act on
//! Enter, print lines a script can grep. There is no history and no completion.
//! Output goes to `console`, i.e. to the UART *and* the ramfb text screen
//! (see `src/console.rs`).
//!
//! Since M5 the shell is also the filesystem's only client: the `ls`/`cat`/
//! `stat`/`mkdir`/`touch`/`write`/`rm`/`cp`/`mv`/`ln` family speaks the VFS
//! (`src/vfs`), which in turn speaks ext2 (`src/fs/ext2`). Two consequences are
//! visible in this file:
//!
//! * **There is a current working directory.** Paths are resolved against it by
//!   `vfs::resolve` + `vfs::walk`, which understands `.`, `..` and symlinks; the
//!   shell itself stores the cwd as an absolute path string.
//! * **Commands report errors as text, never panic.** A missing `/mnt`, a
//!   corrupt directory or a full disk prints one line and returns to the prompt —
//!   the machine keeps running, which is what makes the serial log a test
//!   surface.
//!
//! `ramdisk` is a deliberate test-only command: it registers a RAM `MemBlock` as
//! the system block device and formats/mounts it, exercising exactly the boot
//! path in `src/main.rs::init_fs` on a machine with no disk attached. It is how
//! the filesystem commands are verified in CI (which has no disk) and it is
//! marked as test-only in the `help` text.

use alloc::string::String;
use alloc::sync::Arc;

use crate::arch::riscv64::csr;
use crate::arch::riscv64::timer;
use crate::drivers::block::{self, MemBlock, SECTOR_SIZE};
use crate::drivers::uart;
use crate::fdt::Fdt;
use crate::vfs::{self, VfsError, VfsNode};

/// Longest accepted input line. Longer lines are refused, not truncated: a
/// silently shortened command is worse than a rejected one.
const LINE_CAPACITY: usize = 128;

/// 512-byte sectors in one MiB, for the human-readable capacity in `disk`.
const SECTORS_PER_MIB: u64 = (1024 * 1024) / SECTOR_SIZE as u64;

/// Cap on `cat`/`cp` bytes, so a huge file cannot lock the console for minutes.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Read/write chunk used by `cat` and `cp`.
const IO_CHUNK: usize = 4096;

/// Shell state that survives from one command to the next.
struct State {
    /// Absolute current working directory (lexically joined, not canonicalised).
    cwd: String,
}

impl State {
    fn new() -> Self {
        State {
            cwd: String::from("/"),
        }
    }
}

/// Run the console until `halt` is typed. Never returns: the kernel has nothing
/// else to schedule yet, so leaving the shell would leave the hart idle forever.
pub fn run(tree: Fdt) -> ! {
    crate::kprintln!();
    crate::kprintln!("Type 'help' for the command list.");
    crate::kprintln!();

    let mut line = Line::new();
    let mut state = State::new();
    loop {
        crate::kprint!("pagh:rv64> ");
        loop {
            // Idle wait, not a poll: the UART interrupt wakes this `wfi` and the
            // timer guarantees a periodic wake-up even if a byte is lost to the
            // ring-check/`wfi` window (`uart::wait_byte`).
            let byte = uart::wait_byte();
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
            execute(&command, &tree, &mut state);
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

    /// Copy the line out (the heap exists since M2; a command line is a normal
    /// `String` now, which keeps path handling in one place).
    fn take(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.buf[..self.len]).into_owned();
        self.len = 0;
        text
    }
}

/// Split on spaces, ignoring runs of them (`echo  a   b` prints `a b`).
fn split(line: &str) -> (Option<&str>, impl Iterator<Item = &str>) {
    let mut words = line.split_whitespace();
    let first = words.next();
    (first, words)
}

/// Resolve a command argument against the shell's cwd (lexical join; `vfs::walk`
/// resolves `.`/`..`/symlinks against the real tree).
fn arg_path(state: &State, arg: &str) -> String {
    vfs::resolve(&state.cwd, arg)
}

/// `dir` + `/` + `name`, for building a child path from a resolved directory.
fn child_path(dir: &str, name: &str) -> String {
    let mut out = String::with_capacity(dir.len() + 1 + name.len());
    out.push_str(dir);
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(name);
    out
}

/// One error line, in the shape the other commands use.
fn print_error(op: &str, path: &str, e: VfsError) {
    crate::kprintln!("{}: {}: {}", op, path, e.as_str());
}

fn execute(line: &str, tree: &Fdt, state: &mut State) {
    let (name, mut args) = split(line);
    let Some(name) = name else { return };
    match name {
        "help" => {
            crate::kprintln!("commands:");
            crate::kprintln!("  help          this list");
            crate::kprintln!(
                "  mem           device tree range, frame pool, heap and the page table"
            );
            crate::kprintln!(
                "  disk          block device: name, capacity and a sector-0 read check"
            );
            crate::kprintln!(
                "  selftest      run the in-QEMU memory, block and filesystem self-tests"
            );
            crate::kprintln!("  sleep <secs>  wait for that many seconds of timer ticks");
            crate::kprintln!("  time          the `time` CSR (timebase ticks) and its frequency");
            crate::kprintln!("  hart          the hart this kernel is running on");
            crate::kprintln!("  echo <text>   print the text back");
            crate::kprintln!("  ls [path]     list a directory (directories end with '/')");
            crate::kprintln!("  cat <path>    print a file's contents");
            crate::kprintln!("  stat <path>   file or directory details");
            crate::kprintln!("  cd [path]     change directory (default: /)");
            crate::kprintln!("  pwd           print the current directory");
            crate::kprintln!("  mkdir <path>  create a directory");
            crate::kprintln!("  touch <path>  create an empty file");
            crate::kprintln!("  write <path> <text>    replace a file's contents with <text>");
            crate::kprintln!("  rm [-r] <path>         remove a file, or a directory with -r");
            crate::kprintln!("  cp <src> <dst>         copy a file");
            crate::kprintln!("  mv <src> <dst>         rename or move an entry");
            crate::kprintln!("  ln [-s] <target> <path> create a hard or symbolic link");
            crate::kprintln!("  ramdisk [sectors]      TEST ONLY: format+mount a RAM disk at /mnt");
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
            // The address space is the third number that can disagree with the
            // other two: a frame the PMM calls free is only usable if RAM is
            // mapped, and `satp` says whether the CPU is using this map at all.
            match crate::memory::paging::snapshot() {
                Some(state) => crate::kprintln!(
                    "sv39   {} satp={:#x}, root {:#x}, identity {:#x}..{:#x} (RWX below {:#x}, RW above), {} pages, {} device",
                    if crate::memory::paging::active() {
                        "on "
                    } else {
                        "off"
                    },
                    state.satp,
                    state.root,
                    state.identity.0,
                    state.identity.1,
                    state.executable_below,
                    state.mappings,
                    state.device_pages
                ),
                None => crate::kprintln!("sv39   no address space (paging has not been built)"),
            }
        }
        // The block seam's one visible surface: what device the boot path found,
        // how big it is, and a live read proving the driver still answers. Sector
        // 0 is read (never written) because a device that cannot read it is not a
        // disk, and the read is non-destructive. No device is a normal state on a
        // headless boot, so it is a line, not an error.
        "disk" => match block::get() {
            Some(device) => {
                let sectors = device.sector_count();
                let mut sector = [0u8; SECTOR_SIZE];
                let readable = device.read_block(0, &mut sector).is_ok();
                crate::kprintln!(
                    "disk: {} {} sectors ({} MiB), sector 0 {}",
                    device.name(),
                    sectors,
                    sectors / SECTORS_PER_MIB,
                    if readable { "readable" } else { "NOT readable" }
                );
            }
            None => crate::kprintln!("disk: no block device"),
        },
        // The suites: memory first (frames, heap, Sv39), then the block layer
        // (RAM round-trip + read-only device checks), then the filesystem on its
        // own RAM device. All are written to be non-destructive and idempotent,
        // so running `selftest` twice in one boot is a real check instead of a
        // state mutation — the free-frame ledgers must read the same both times.
        "selftest" => {
            crate::memory::selftest::run();
            crate::drivers::selftest::run();
            crate::fs::selftest::run();
            crate::arch::riscv64::selftest::run();
        }
        "time" => {
            // Two clocks, and printing both is the point: the `time` CSR is the
            // hardware counter (it pauses for nothing), while the tick count is
            // what `sleep` and a future scheduler run on. They advance at
            // different rates by design; a log where the two disagree is how a
            // timer that is not actually firing gets caught.
            let hz = tree.timebase_frequency().unwrap_or(0);
            let now = csr::read_time();
            crate::kprintln!(
                "time {} ticks (~{} s at {} Hz)",
                now,
                now / hz.max(1) as usize,
                hz
            );
            crate::kprintln!(
                "uptime {} ms ({} timer ticks at {} Hz, {} timer interrupts)",
                timer::uptime_ms(),
                timer::ticks(),
                timer::TICK_HZ,
                timer::interrupts()
            );
            // The PLIC counters are the only place a routed-but-silent external
            // line is observable: the handler prints nothing (trap context), so
            // claim/complete counts and the UART's dropped-byte count are read
            // here instead.
            let (claims, completions, unknown) = crate::arch::riscv64::plic::stats();
            if unknown == 0 {
                crate::kprintln!(
                    "plic {} claims, {} completed, uart irq {}, {} uart rx overrun(s)",
                    claims,
                    completions,
                    crate::arch::riscv64::plic::uart_irq(),
                    uart::rx_overruns()
                );
            } else {
                crate::kprintln!(
                    "plic {} claims, {} completed, uart irq {}, {} uart rx overrun(s), {} unknown source(s)",
                    claims,
                    completions,
                    crate::arch::riscv64::plic::uart_irq(),
                    uart::rx_overruns(),
                    unknown
                );
            }
        }
        "sleep" => {
            // Seconds in, ticks out: the command is a user-facing duration, the
            // wait is the kernel's clock. `wfi` inside `sleep_ticks` is what makes
            // this an idle wait rather than a spin.
            let seconds = args.next().and_then(|word| word.parse::<u64>().ok());
            match seconds {
                Some(seconds) => {
                    let wait = seconds.saturating_mul(timer::TICK_HZ as u64);
                    crate::kprintln!("sleep: {} s ({} timer ticks)", seconds, wait);
                    timer::sleep_ticks(wait);
                }
                None => crate::kprintln!("sleep: usage: sleep <seconds>"),
            }
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
        "pwd" => crate::kprintln!("{}", state.cwd),
        "cd" => cmd_cd(args, state),
        "ls" => cmd_ls(args, state),
        "cat" => cmd_cat(args, state),
        "stat" => cmd_stat(args, state),
        "mkdir" => cmd_mkdir(args, state),
        "touch" => cmd_touch(args, state),
        "write" => cmd_write(args, state),
        "rm" => cmd_rm(args, state),
        "cp" => cmd_cp(args, state),
        "mv" => cmd_mv(args, state),
        "ln" => cmd_ln(args, state),
        "fs" => cmd_fs(),
        "ramdisk" => cmd_ramdisk(args, state),
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

// ─── filesystem commands ───────────────────────────────────────────────────

/// `pwd`-relative resolution helper: the absolute path for an argument.
fn resolve_arg(state: &State, arg: &str) -> String {
    arg_path(state, arg)
}

/// `cd [path]`: no argument goes back to `/`; a non-directory is refused and
/// leaves the cwd unchanged.
fn cmd_cd<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        state.cwd = String::from("/");
        return;
    };
    let target = resolve_arg(state, arg);
    match vfs::walk(&target, true) {
        Ok(node) => {
            if node.is_directory() {
                state.cwd = target;
            } else {
                print_error("cd", arg, VfsError::NotADirectory);
            }
        }
        Err(e) => print_error("cd", arg, e),
    }
}

/// `ls [path]`: list directory entries (directories with a trailing `/`), or a
/// file's own name.
fn cmd_ls<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let path = match args.next() {
        Some(arg) => resolve_arg(state, arg),
        None => state.cwd.clone(),
    };
    match vfs::walk(&path, true) {
        Ok(node) => {
            if node.is_directory() {
                match node.readdir() {
                    Ok(children) => {
                        for child in children.iter() {
                            if child.is_directory() {
                                crate::kprintln!("{}/", child.name());
                            } else {
                                crate::kprintln!("{}", child.name());
                            }
                        }
                    }
                    Err(e) => print_error("ls", &path, e),
                }
            } else {
                crate::kprintln!("{}", node.name());
            }
        }
        Err(e) => print_error("ls", &path, e),
    }
}

/// `cat <path>`: print a file's contents (lossily decoded: the console is a text
/// sink, `read` is byte-granular and a binary file is not a console's business).
fn cmd_cat<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        crate::kprintln!("cat: missing operand");
        return;
    };
    let path = resolve_arg(state, arg);
    let node = match vfs::walk(&path, true) {
        Ok(node) => node,
        Err(e) => return print_error("cat", &path, e),
    };
    if node.is_directory() {
        return print_error("cat", &path, VfsError::IsADirectory);
    }
    let mut buf = [0u8; IO_CHUNK];
    let mut offset = 0u64;
    let mut last = b'\n';
    while offset < MAX_FILE_BYTES {
        match node.read(offset, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                crate::kprint!("{}", String::from_utf8_lossy(&buf[..n]));
                last = buf[n - 1];
                offset += n as u64;
            }
            Err(e) => return print_error("cat", &path, e),
        }
    }
    if last != b'\n' {
        crate::kprintln!();
    }
}

/// `stat <path>`: name, type, size, inode, links and (for links) the target.
fn cmd_stat<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        crate::kprintln!("stat: missing operand");
        return;
    };
    let path = resolve_arg(state, arg);
    // No-follow: `stat link` reports the link itself, with its target printed.
    let node = match vfs::walk(&path, false) {
        Ok(node) => node,
        Err(e) => return print_error("stat", &path, e),
    };
    let kind = if node.is_symlink() {
        "symbolic link"
    } else if node.is_directory() {
        "directory"
    } else {
        "regular file"
    };
    crate::kprintln!("  path:  {}", path);
    crate::kprintln!("  name:  {}", node.name());
    crate::kprintln!("  type:  {}", kind);
    crate::kprintln!("  mode:  {:04o}", node.mode() & 0o7777);
    crate::kprintln!("  size:  {} bytes", node.size());
    crate::kprintln!("  inode: {}", node.fs_ino());
    crate::kprintln!("  links: {}", node.nlink());
    if node.is_directory() {
        if let Ok(children) = node.readdir() {
            crate::kprintln!("  entries: {}", children.len());
        }
    }
    if node.is_symlink() {
        match node.read_link() {
            Some(target) => crate::kprintln!("  target: {}", target),
            None => crate::kprintln!("  target: <unreadable>"),
        }
    }
}

/// `mkdir <path>`: create one directory (the parent must exist).
fn cmd_mkdir<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        crate::kprintln!("mkdir: missing operand");
        return;
    };
    let path = resolve_arg(state, arg);
    let (parent_path, leaf) = match vfs::parent_and_leaf(&path) {
        Ok(parts) => parts,
        Err(e) => return print_error("mkdir", &path, e),
    };
    match vfs::walk(&parent_path, true) {
        Ok(parent) => match parent.create_dir(&leaf) {
            Ok(_) => {}
            Err(e) => print_error("mkdir", &path, e),
        },
        Err(e) => print_error("mkdir", &parent_path, e),
    }
}

/// `touch <path>`: create an empty file; an existing entry is a no-op.
fn cmd_touch<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        crate::kprintln!("touch: missing operand");
        return;
    };
    let path = resolve_arg(state, arg);
    let (parent_path, leaf) = match vfs::parent_and_leaf(&path) {
        Ok(parts) => parts,
        Err(e) => return print_error("touch", &path, e),
    };
    match vfs::walk(&parent_path, true) {
        Ok(parent) => match parent.create_file(&leaf) {
            Ok(_) | Err(VfsError::AlreadyExists) => {}
            Err(e) => print_error("touch", &path, e),
        },
        Err(e) => print_error("touch", &parent_path, e),
    }
}

/// `write <path> <text>`: replace a file's contents, creating the file when it
/// does not exist.
fn cmd_write<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let Some(arg) = args.next() else {
        crate::kprintln!("write: usage: write <path> <text>");
        return;
    };
    let text: alloc::vec::Vec<&'a str> = args.collect();
    if text.is_empty() {
        crate::kprintln!("write: usage: write <path> <text>");
        return;
    }
    let mut joined = String::new();
    for (i, word) in text.iter().enumerate() {
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(word);
    }
    let path = resolve_arg(state, arg);
    // Follow a final symlink so `write link txt` writes the target, like open(2).
    let node = match vfs::walk(&path, true) {
        Ok(node) => node,
        Err(VfsError::NotFound) => {
            // Create in the parent directory.
            let (parent_path, leaf) = match vfs::parent_and_leaf(&path) {
                Ok(parts) => parts,
                Err(e) => return print_error("write", &path, e),
            };
            match vfs::walk(&parent_path, true) {
                Ok(parent) => match parent.create_file(&leaf) {
                    Ok(node) => node,
                    Err(e) => return print_error("write", &path, e),
                },
                Err(e) => return print_error("write", &parent_path, e),
            }
        }
        Err(e) => return print_error("write", &path, e),
    };
    if node.is_directory() {
        return print_error("write", &path, VfsError::IsADirectory);
    }
    match vfs::replace_contents(&node, joined.as_bytes()) {
        Ok(n) => {
            // Flush once, at the boundary of the command: the writer is
            // synchronous, but a device cache deserves the hint.
            node.sync();
            crate::kprintln!("write: wrote {} bytes to {}", n, path);
        }
        Err(e) => print_error("write", &path, e),
    }
}

/// `rm [-r] <path>...`: remove entries post-order; a directory needs `-r`.
fn cmd_rm<'a>(args: impl Iterator<Item = &'a str>, state: &mut State) {
    let mut recursive = false;
    let mut targets: alloc::vec::Vec<&'a str> = alloc::vec::Vec::new();
    for arg in args {
        match arg {
            "-r" | "-R" | "-rf" | "-fr" | "--recursive" => recursive = true,
            // `-f` and other flags are no-ops here: everything below fails loudly
            // rather than asking for confirmation.
            x if x.starts_with('-') && x.len() > 1 => {}
            x => targets.push(x),
        }
    }
    if targets.is_empty() {
        crate::kprintln!("rm: missing operand");
        return;
    }
    for target in targets {
        let path = resolve_arg(state, target);
        if let Err(e) = remove_path(&path, recursive, 0) {
            print_error("rm", &path, e);
        }
    }
}

/// Post-order removal (`rm -r`); depth-capped so a corrupt tree cannot make the
/// shell recurse forever.
fn remove_path(path: &str, recursive: bool, depth: u8) -> Result<(), VfsError> {
    if depth > 16 {
        return Err(VfsError::InvalidArgument);
    }
    let node = vfs::lookup_path(path)?;
    if node.is_directory() {
        if !recursive {
            return Err(VfsError::InvalidArgument);
        }
        for child in node.readdir()? {
            let child_path = child_path(path, child.name());
            remove_path(&child_path, recursive, depth + 1)?;
        }
    }
    let (parent_path, leaf) = vfs::parent_and_leaf(path)?;
    let parent = vfs::walk(&parent_path, true)?;
    parent.remove(&leaf)
}

/// `cp <src> <dst>`: copy a file; `dst` may be a directory (the source's name is
/// kept) or a file path (an existing file is replaced).
fn cmd_cp<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let (Some(src_arg), Some(dst_arg)) = (args.next(), args.next()) else {
        crate::kprintln!("cp: usage: cp <src> <dst>");
        return;
    };
    let src_path = resolve_arg(state, src_arg);
    let dst_path = resolve_arg(state, dst_arg);
    let src = match vfs::walk(&src_path, true) {
        Ok(node) => node,
        Err(e) => return print_error("cp", &src_path, e),
    };
    if src.is_directory() {
        return print_error("cp", &src_path, VfsError::IsADirectory);
    }
    let dst = match vfs::walk(&dst_path, true) {
        Ok(node) if node.is_directory() => {
            // Copy into the directory under the source's own name.
            let leaf = match vfs::parent_and_leaf(&src_path) {
                Ok((_, leaf)) => leaf,
                Err(e) => return print_error("cp", &src_path, e),
            };
            match node.create_file(&leaf) {
                Ok(node) => node,
                Err(VfsError::AlreadyExists) => match node.lookup(&leaf) {
                    Ok(node) => node,
                    Err(e) => return print_error("cp", &dst_path, e),
                },
                Err(e) => return print_error("cp", &dst_path, e),
            }
        }
        Ok(node) => node,
        Err(VfsError::NotFound) => {
            let (parent_path, leaf) = match vfs::parent_and_leaf(&dst_path) {
                Ok(parts) => parts,
                Err(e) => return print_error("cp", &dst_path, e),
            };
            match vfs::walk(&parent_path, true) {
                Ok(parent) => match parent.create_file(&leaf) {
                    Ok(node) => node,
                    Err(e) => return print_error("cp", &dst_path, e),
                },
                Err(e) => return print_error("cp", &parent_path, e),
            }
        }
        Err(e) => return print_error("cp", &dst_path, e),
    };
    if dst.is_directory() {
        return print_error("cp", &dst_path, VfsError::IsADirectory);
    }
    match copy_file(&src, &dst) {
        Ok(n) => {
            dst.sync();
            crate::kprintln!("cp: copied {} bytes", n);
        }
        Err(e) => print_error("cp", &dst_path, e),
    }
}

/// Byte-copy `src` into `dst` (truncating `dst` first) in [`IO_CHUNK`] pieces.
fn copy_file(src: &Arc<dyn VfsNode>, dst: &Arc<dyn VfsNode>) -> Result<u64, VfsError> {
    dst.truncate(0)?;
    let mut buf = [0u8; IO_CHUNK];
    let mut offset = 0u64;
    while offset < MAX_FILE_BYTES {
        let n = src.read(offset, &mut buf)?;
        if n == 0 {
            break;
        }
        let written = dst.write(offset, &buf[..n])?;
        if written != n {
            return Err(VfsError::IoError);
        }
        offset += n as u64;
    }
    Ok(offset)
}

/// `mv <src> <dst>`: one filesystem rename. If `dst` names an existing
/// directory, the source moves into it under its own name.
fn cmd_mv<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let (Some(src_arg), Some(dst_arg)) = (args.next(), args.next()) else {
        crate::kprintln!("mv: usage: mv <src> <dst>");
        return;
    };
    let src_path = resolve_arg(state, src_arg);
    let dst_path = resolve_arg(state, dst_arg);
    let new_path = match vfs::lookup_path(&dst_path) {
        Ok(node) if node.is_directory() => match vfs::parent_and_leaf(&src_path) {
            Ok((_, leaf)) => child_path(&dst_path, &leaf),
            Err(e) => return print_error("mv", &src_path, e),
        },
        _ => dst_path.clone(),
    };
    match vfs::rename(&src_path, &new_path) {
        Ok(()) => crate::kprintln!("mv: {} -> {}", src_path, new_path),
        Err(e) => print_error("mv", &src_path, e),
    }
}

/// `ln [-s] <target> <path>`: a symbolic link with `-s`, a hard link without.
fn cmd_ln<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    let mut symbolic = false;
    let mut rest: alloc::vec::Vec<&'a str> = alloc::vec::Vec::new();
    for arg in args {
        if arg == "-s" {
            symbolic = true;
        } else {
            rest.push(arg);
        }
    }
    let (Some(target), Some(path_arg)) = (rest.first(), rest.get(1)) else {
        crate::kprintln!("ln: usage: ln [-s] <target> <path>");
        return;
    };
    let path = resolve_arg(state, path_arg);
    let (parent_path, leaf) = match vfs::parent_and_leaf(&path) {
        Ok(parts) => parts,
        Err(e) => return print_error("ln", &path, e),
    };
    let parent = match vfs::walk(&parent_path, true) {
        Ok(parent) => parent,
        Err(e) => return print_error("ln", &parent_path, e),
    };
    if symbolic {
        match parent.create_symlink(&leaf, target.as_bytes()) {
            Ok(_) => {}
            Err(e) => print_error("ln", &path, e),
        }
    } else {
        // A hard link needs the target node; relative targets are resolved
        // against the shell's cwd, like every other path.
        let target_path = resolve_arg(state, target);
        match vfs::walk(&target_path, true) {
            Ok(node) => match parent.link(&leaf, &node) {
                Ok(_) => {}
                Err(e) => print_error("ln", &path, e),
            },
            Err(e) => print_error("ln", &target_path, e),
        }
    }
}

/// `fs`: what is mounted at `/mnt`, and how full it is.
fn cmd_fs() {
    match vfs::lookup_path("/mnt") {
        Ok(node) => match node.fs_stat() {
            Some(stat) => crate::kprintln!(
                "fs     ext2 at /mnt, {} B blocks, {}/{} blocks free, {}/{} inodes free",
                stat.block_size,
                stat.blocks_free,
                stat.blocks_total,
                stat.inodes_free,
                stat.inodes_total
            ),
            None => crate::kprintln!("fs     /mnt is not an ext2 filesystem"),
        },
        Err(_) => crate::kprintln!("fs     /mnt is not mounted"),
    }
}

/// `ramdisk [sectors]`: TEST ONLY — register a RAM block device and put a fresh
/// ext2 filesystem on it at `/mnt`.
///
/// This is the same path the boot uses for a real disk (`format-if-blank`, then
/// mount), with a `MemBlock` instead of a driver. It exists so the filesystem
/// commands can be exercised on a machine without a disk — CI's configuration —
/// and it replaces both the system block device and the `/mnt` mount.
fn cmd_ramdisk<'a>(mut args: impl Iterator<Item = &'a str>, state: &mut State) {
    // The heap refuses a single allocation above one 64 KiB chunk, so a MemBlock
    // is at most 127 sectors; 120 is 60 KiB, which format turns into 15 blocks of
    // 4096 bytes.
    const DEFAULT_SECTORS: u64 = 120;
    const MAX_SECTORS: u64 = 127;
    let sectors = match args.next() {
        Some(text) => match text.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                crate::kprintln!("ramdisk: usage: ramdisk [sectors] (16..=127)");
                return;
            }
        },
        None => DEFAULT_SECTORS,
    };
    if sectors < 16 || sectors > MAX_SECTORS {
        crate::kprintln!("ramdisk: sectors must be in 16..={}", MAX_SECTORS);
        return;
    }
    let dev: Arc<dyn block::BlockDevice> = Arc::new(MemBlock::new("ramdisk", sectors));
    block::register(Arc::clone(&dev));
    let _ = vfs::unmount_at("/mnt");
    let root = match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&dev)) {
        Ok(root) => root,
        Err(_) => match crate::fs::ext2::Ext2Fs::format(Arc::clone(&dev)) {
            Ok(()) => match crate::fs::ext2::Ext2Fs::mount(Arc::clone(&dev)) {
                Ok(root) => root,
                Err(e) => {
                    crate::kprintln!("ramdisk: mount after format failed: {}", e.as_str());
                    return;
                }
            },
            Err(e) => {
                crate::kprintln!("ramdisk: format failed: {}", e.as_str());
                return;
            }
        },
    };
    match vfs::mount_at("/mnt", root) {
        Ok(()) => {
            state.cwd = String::from("/mnt");
            crate::kprintln!(
                "ramdisk: {} KiB RAM device formatted ext2, mounted at /mnt",
                sectors * 512 / 1024
            );
        }
        Err(e) => {
            let _ = e;
            crate::kprintln!("ramdisk: mounting at /mnt failed");
        }
    }
}
