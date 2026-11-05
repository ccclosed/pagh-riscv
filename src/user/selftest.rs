//! The user-mode (M6b) self-tests, run by the shell's `selftest` after the task
//! suite.
//!
//! The contract is the same as the M6a/M5 suites: non-destructive, deterministic
//! and idempotent. The property this suite exists to prove is the *ledger*
//! property — a started program, a refused image and a killed process all leave
//! the PMM exactly as they found it — plus the two acceptance runs the milestone
//! names: `hello` exits 42 through `write`/`sched_yield`/`exit`, and `fault` dies
//! from a U-mode page fault without taking the machine with it. The ABI fixture
//! also carries the native-thread section (`clone`/gettid/futex/`exit`/
//! `exit_group`); the kernel side requires its counters to move and the futex
//! queue to be empty after the run.
//!
//! The reject matrix is built by mutating a copy of the very ELF the accept case
//! loads. The bytes under test therefore stay the shipped fixture, each mutation
//! changes one header field, and the expected [`ElfError`] says which rule the
//! loader enforced. Every case also compares the free-frame count, because "an
//! error costs the PMM nothing" is a claim about frames, not about return values.
//!
//! The last check is the streaming claim: `/mnt/selftest-large.elf` is the hello
//! fixture padded with zeros to 100 KiB — more than one 64 KiB heap chunk — and
//! `run_file` must execute it (`Exit::Code(42)`) without a whole-image buffer
//! and without leaving frames or heap bytes behind. It prints `SKIP` when no
//! filesystem is mounted, so the suite still runs on the serial-only boot.
//!
//! The scratch buffer is allocated once, before the ledger baseline, and reused
//! for every case: the heap chunk is part of the harness, not of what the tests
//! measure, and reusing it keeps `selftest` twice in one boot idempotent.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::riscv64::traps::CAUSE_LOAD_PAGE_FAULT;
use crate::memory::pmm;
use crate::user::elf::{self, ElfError, MAX_IMAGE_BYTES};
use crate::user::syscall;
use crate::user::{
    self, Exit, UserError, ABI_ELF, ARGV_ELF, EXEC_CHILD_ELF, FAULT_ELF, HELLO_ELF, SPIN_ELF,
};
use crate::vfs::{self, VfsError, VfsNode};

/// The fixture's `e_entry`: `user.ld` places `_start` at the bottom of the first
/// mapped page, 0x1_0000.
const HELLO_ENTRY: usize = 0x0001_0000;

/// Pages the `hello` fixture occupies: text, rodata, and data+bss (the last
/// segment's 0x1008 bytes span two pages).
const HELLO_PAGES: usize = 4;

/// Byte offset of `hello`'s first program header (`e_phoff`, which both fixtures
/// share) and the fields the reject matrix mutates.
const PHDR0: usize = 64;

const E_TYPE: usize = 16;
const E_MACHINE: usize = 18;
const E_ENTRY: usize = 24;
const E_PHOFF: usize = 32;
const E_EHSIZE: usize = 52;
const E_PHNUM: usize = 56;
const PHDR_VADDR: usize = PHDR0 + 16;
const PHDR_FILESZ: usize = PHDR0 + 32;
const PHDR_MEMSZ: usize = PHDR0 + 40;

/// Free frames in the PMM: the number every case has to leave unchanged.
fn frames() -> usize {
    pmm::stats().0
}

/// True when a captured console transcript contains `needle` as a contiguous
/// byte run. The transcript is bounded; a `truncated` capture is still searched,
/// and the caller's marker check fails if the bytes were not there.
fn captured_contains(captured: &user::Captured, needle: &[u8]) -> bool {
    captured.bytes[..captured.len]
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Little-endian stores that never panic on an out-of-range offset: a fixture
/// change shows up as a failing case, not as a kernel abort.
fn put_u16(buf: &mut [u8], offset: usize, value: u16) -> bool {
    match buf.get_mut(offset..offset + 2) {
        Some(slot) => {
            slot.copy_from_slice(&value.to_le_bytes());
            true
        }
        None => false,
    }
}

fn put_u64(buf: &mut [u8], offset: usize, value: u64) -> bool {
    match buf.get_mut(offset..offset + 8) {
        Some(slot) => {
            slot.copy_from_slice(&value.to_le_bytes());
            true
        }
        None => false,
    }
}

/// Restore the known-good fixture.
fn reset(buf: &mut Vec<u8>) {
    buf.clear();
    buf.extend_from_slice(HELLO_ELF);
}

/// One reject case: reset, mutate, load, and require exactly `expected` with the
/// frame ledger unchanged.
fn reject(buf: &mut Vec<u8>, expected: ElfError, mutate: fn(&mut [u8])) -> bool {
    reset(buf);
    mutate(buf.as_mut_slice());
    let before = frames();
    let result = elf::load(buf.as_slice(), 0);
    let after = frames();
    match result {
        Err(error) if error == expected && before == after => true,
        // A copy was loaded despite the mutation: release it so one failed case
        // cannot leak into the next case's ledger.
        Ok(mut loaded) => {
            loaded.release();
            crate::kprintln!(
                "[selftest] user: expected {:?}, the loader accepted the image",
                expected
            );
            false
        }
        Err(error) => {
            crate::kprintln!(
                "[selftest] user: expected {:?}, got {:?} (frames {} -> {})",
                expected,
                error,
                before,
                after
            );
            false
        }
    }
}

// ── the mutations (one field each; see the ELF64 header layout) ──────────────

fn bad_magic(buf: &mut [u8]) {
    let _ = buf.first_mut().map(|byte| *byte = 0);
}

fn bad_class(buf: &mut [u8]) {
    if let Some(byte) = buf.get_mut(4) {
        *byte = 1;
    }
}

fn bad_data(buf: &mut [u8]) {
    if let Some(byte) = buf.get_mut(5) {
        *byte = 2;
    }
}

fn bad_ident_version(buf: &mut [u8]) {
    if let Some(byte) = buf.get_mut(6) {
        *byte = 0;
    }
}

fn bad_type(buf: &mut [u8]) {
    let _ = put_u16(buf, E_TYPE, 1); // ET_REL
}

fn bad_machine(buf: &mut [u8]) {
    let _ = put_u16(buf, E_MACHINE, 0);
}

fn bad_header(buf: &mut [u8]) {
    let _ = put_u16(buf, E_EHSIZE, 32);
}

fn program_headers_out_of_bounds(buf: &mut [u8]) {
    let _ = put_u64(buf, E_PHOFF, buf.len() as u64);
}

fn no_loadable_segment(buf: &mut [u8]) {
    let _ = put_u16(buf, E_PHNUM, 0);
}

fn filesz_bigger_than_memsz(buf: &mut [u8]) {
    let _ = put_u64(buf, PHDR_FILESZ, 0x2000);
}

fn segment_outside_window(buf: &mut [u8]) {
    let _ = put_u64(buf, PHDR_VADDR, 0x5000_0000);
}

fn entry_outside_text(buf: &mut [u8]) {
    let _ = put_u64(buf, E_ENTRY, 0x2000_0000);
}

fn image_too_large(buf: &mut [u8]) {
    let _ = put_u64(buf, PHDR_MEMSZ, (MAX_IMAGE_BYTES + 1) as u64);
}

/// The happy path: the shipped fixture loads, its pages are exactly the three
/// expected addresses, a second concurrent load is refused, and the release puts
/// every frame back.
fn elf_accept(buf: &mut Vec<u8>) -> bool {
    let before = frames();
    reset(buf);
    let mut loaded = match elf::load(buf.as_slice(), 0) {
        Ok(loaded) => loaded,
        Err(error) => {
            crate::kprintln!("[selftest] user: the hello fixture was refused: {error}");
            return false;
        }
    };

    let mut ok = loaded.entry == HELLO_ENTRY && loaded.pages().len() == HELLO_PAGES;
    if !ok {
        crate::kprintln!(
            "[selftest] user: hello loaded entry {:#x} with {} page(s), expected {:#x}/{HELLO_PAGES}",
            loaded.entry,
            loaded.pages().len(),
            HELLO_ENTRY
        );
    }

    // A second load of the same image must be refused because those pages are
    // live, and the refusal must not have allocated anything.
    let during = frames();
    match elf::load(buf.as_slice(), 0) {
        Err(ElfError::AddressInUse(_)) => {
            if frames() != during {
                crate::kprintln!(
                    "[selftest] user: a refused overlapping load changed the ledger ({} -> {})",
                    during,
                    frames()
                );
                ok = false;
            }
        }
        Ok(mut loaded) => {
            loaded.release();
            crate::kprintln!("[selftest] user: the overlapping load was accepted");
            ok = false;
        }
        Err(error) => {
            crate::kprintln!(
                "[selftest] user: the overlapping load failed for the wrong reason: {error:?}"
            );
            ok = false;
        }
    }

    loaded.release();
    if frames() != before {
        crate::kprintln!(
            "[selftest] user: releasing the image left the ledger at {} (was {before})",
            frames()
        );
        ok = false;
    }
    ok
}

/// The reject matrix: every case must be refused by name and cost no frame.
fn elf_rejects(buf: &mut Vec<u8>) -> bool {
    let mut ok = true;

    // Shorter than one ELF64 header; the only case that does not start from the
    // fixture bytes.
    let before = frames();
    let short = elf::load(&HELLO_ELF[..32], 0);
    ok &= matches!(short, Err(ElfError::Short)) && frames() == before;

    ok &= reject(buf, ElfError::BadMagic, bad_magic);
    ok &= reject(buf, ElfError::BadClass, bad_class);
    ok &= reject(buf, ElfError::BadData, bad_data);
    ok &= reject(buf, ElfError::BadIdentVersion, bad_ident_version);
    ok &= reject(buf, ElfError::BadType, bad_type);
    ok &= reject(buf, ElfError::BadMachine, bad_machine);
    ok &= reject(buf, ElfError::BadHeader, bad_header);
    ok &= reject(
        buf,
        ElfError::ProgramHeadersOutOfBounds,
        program_headers_out_of_bounds,
    );
    ok &= reject(buf, ElfError::NoLoadableSegment, no_loadable_segment);
    ok &= reject(buf, ElfError::BadSegment, filesz_bigger_than_memsz);
    ok &= reject(
        buf,
        ElfError::SegmentOutsideUserWindow,
        segment_outside_window,
    );
    ok &= reject(buf, ElfError::EntryOutsideText, entry_outside_text);
    ok &= reject(buf, ElfError::ImageTooLarge, image_too_large);

    // `ET_EXEC` may only be loaded at bias 0; the loader must say so instead of
    // placing the image somewhere the linker did not intend.
    reset(buf);
    let biased = elf::load(buf.as_slice(), crate::memory::FRAME_SIZE);
    ok &= matches!(biased, Err(ElfError::BadBias)) && frames() == before;

    ok
}

/// The entry-stack proof: run the argv fixture with three arguments and a
/// `PWD=/` environment and require the exact line back, byte for byte.
///
/// Capturing the *console bytes* rather than trusting the exit code is what makes
/// this a test of the layout: the fixture validates `argc`, `argv`, the NULL
/// terminators, `envp == argv + argc + 1`, `sp == frame base` and 16-byte
/// alignment internally, prints only when all of that holds, and the selftest
/// compares the resulting transcript verbatim.
const ARGV_LINE: &[u8] = b"argv: argc=3 argv[0]=argv.elf argv[1]=alpha argv[2]=beta env[0]=PWD=/\n";

fn user_abi_argv(_buf: &mut Vec<u8>) -> Outcome {
    let before = frames();
    user::capture_start();
    let result = user::run_image_with_args(ARGV_ELF, &["argv.elf", "alpha", "beta"], &["PWD=/"]);
    let captured = user::capture_stop();

    let mut ok = match result {
        Ok(Exit::Code(0)) => true,
        Ok(other) => {
            crate::kprintln!("[selftest] user: the argv fixture ended {other}");
            false
        }
        Err(error) => {
            crate::kprintln!("[selftest] user: the argv fixture failed: {error}");
            false
        }
    };
    if captured.truncated || captured.bytes[..captured.len] != ARGV_LINE[..] {
        crate::kprintln!(
            "[selftest] user: argv transcript was {:?}",
            core::str::from_utf8(&captured.bytes[..captured.len]).unwrap_or("<non-UTF-8>")
        );
        ok = false;
    }
    if frames() != before {
        crate::kprintln!(
            "[selftest] user: the argv run left the ledger at {} (was {before})",
            frames()
        );
        ok = false;
    }
    verdict(ok)
}

/// The filesystem fixture's inputs: the read-only file the ABI fixture opens,
/// the file it creates/writes/appends, and the relative symlink it resolves.
const ABI_FILE: &str = "abi-syscalls.txt";
const ABI_CREATED: &str = "abi-created.txt";
const ABI_LINK: &str = "abi-link";
const EXEC_CHILD: &str = "exec-child";
const ABI_FILE_BYTES: &[u8] = b"pagh-abi\n";

/// Every name the ABI fixture's mutation half may create. The fixture removes
/// them itself; this list is the kernel-side idempotency guard (a crashed run
/// must not turn the next `selftest` into `EEXIST`) and the "all gone" check.
const ABI_MUTATION_NAMES: [&str; 5] = [
    "abi-mut.txt",
    "abi-renamed.txt",
    "abi-hard.txt",
    "abi-sym",
    "abi-dir",
];

/// Remove every fixture input; missing entries are fine (idempotency).
fn cleanup_abi_files(dir: &Arc<dyn VfsNode>) {
    // A run that died between `mkdirat` and removing the child leaves a
    // non-empty `/mnt/abi-dir`; empty it first so the removal below can work.
    // One level is enough: the fixture only ever creates `inside.txt` there.
    if let Ok(node) = dir.lookup("abi-dir") {
        if let Ok(children) = node.readdir() {
            for child in children {
                let _ = node.remove(child.name());
            }
        }
    }
    for name in [ABI_FILE, ABI_CREATED, ABI_LINK, EXEC_CHILD] {
        match dir.remove(name) {
            Ok(()) | Err(VfsError::NotFound) => {}
            Err(error) => crate::kprintln!(
                "[selftest] user: cannot remove stale /mnt/{}: {}",
                name,
                error.as_str()
            ),
        }
    }
    for name in ABI_MUTATION_NAMES {
        match dir.remove(name) {
            Ok(()) | Err(VfsError::NotFound) => {}
            Err(error) => crate::kprintln!(
                "[selftest] user: cannot remove stale /mnt/{}: {}",
                name,
                error.as_str()
            ),
        }
    }
}

/// Create the fixture's inputs. Called after the ledger baseline is taken, so
/// the check proves the run *and* the setup hand everything back.
fn create_abi_files(dir: &Arc<dyn VfsNode>) -> Result<(), &'static str> {
    let file = dir
        .create_file(ABI_FILE)
        .map_err(|_| "cannot create /mnt/abi-syscalls.txt")?;
    match file.write(0, ABI_FILE_BYTES) {
        Ok(written) if written == ABI_FILE_BYTES.len() => {}
        _ => return Err("cannot write /mnt/abi-syscalls.txt"),
    }
    drop(file);
    dir.create_file(ABI_CREATED)
        .map_err(|_| "cannot create /mnt/abi-created.txt")?;
    let link = dir
        .create_symlink(ABI_LINK, ABI_FILE.as_bytes())
        .map_err(|_| "cannot create /mnt/abi-link")?;
    drop(link);
    let exec = dir
        .create_file(EXEC_CHILD)
        .map_err(|_| "cannot create /mnt/exec-child")?;
    match exec.write(0, EXEC_CHILD_ELF) {
        Ok(written) if written == EXEC_CHILD_ELF.len() => {}
        _ => return Err("cannot write /mnt/exec-child"),
    }
    drop(exec);
    Ok(())
}

/// The native-ABI acceptance run.
///
/// The fixture is one static ELF that calls every syscall the ABI lists. With a
/// filesystem it must exit 0; without one it must exit 77 *after* the
/// filesystem-independent half passed, which is a SKIP (not a pass: the file
/// path was not exercised) but still proves `write`, `brk`, `clock_gettime`,
/// `nanosleep`, `uname`, `getdents64("/")` and the error paths.
///
/// The PMM and heap ledgers are compared across the whole check — creating the
/// inputs, running the program with its directory snapshots and file handles,
/// and removing the inputs — because a `brk` page, an fd node or a snapshot that
/// is not released would move them.
fn user_abi_syscalls(_buf: &mut Vec<u8>) -> Outcome {
    let dir = match vfs::walk("/mnt", true) {
        Ok(node) if node.is_directory() => Some(node),
        Ok(_) => {
            crate::kprintln!("[selftest] user: /mnt is not a directory");
            return Outcome::Fail;
        }
        Err(VfsError::NotFound) => None,
        Err(error) => {
            crate::kprintln!("[selftest] user: cannot resolve /mnt: {}", error.as_str());
            return Outcome::Fail;
        }
    };

    if let Some(dir) = dir.as_ref() {
        cleanup_abi_files(dir);
    }
    let (used_before, mapped_before, _) = crate::heap_stats();
    let frames_before = frames();
    let stats_before = syscall::stats();
    let signals_before = crate::user::signal::stats();
    let mut ok = true;
    if let Some(dir) = dir.as_ref() {
        if let Err(reason) = create_abi_files(dir) {
            crate::kprintln!("[selftest] user: {}", reason);
            ok = false;
        }
    }

    user::capture_start();
    let result = if ok {
        Some(user::run_image_with_args(ABI_ELF, &["abi.elf"], &["PWD=/"]))
    } else {
        None // setup failed; do not run against missing inputs
    };
    let captured = user::capture_stop();
    let stats = syscall::stats();

    // The fixture removes every mutation artifact itself (rename, link, symlink,
    // truncate, unlink and the mkdir/rmdir pair). Verify that from the kernel
    // side before the cleanup below hides a leftover: an empty-looking success
    // that leaked a directory entry is a failed acceptance run.
    if let (Some(dir), Some(Ok(Exit::Code(0)))) = (dir.as_ref(), result.as_ref()) {
        for name in ABI_MUTATION_NAMES {
            match dir.lookup(name) {
                Err(VfsError::NotFound) => {}
                _ => {
                    crate::kprintln!("[selftest] user: the ABI fixture left /mnt/{} behind", name);
                    ok = false;
                }
            }
        }
    }

    if let Some(dir) = dir.as_ref() {
        cleanup_abi_files(dir);
    }

    match (&dir, &result) {
        (_, None) => {}
        (Some(_), Some(Ok(Exit::Code(0)))) => {}
        (None, Some(Ok(Exit::Code(77)))) => {}
        (_, Some(Ok(other))) => {
            crate::kprintln!("[selftest] user: the ABI fixture ended {other}");
            ok = false;
        }
        (_, Some(Err(error))) => {
            crate::kprintln!("[selftest] user: the ABI fixture failed: {error}");
            ok = false;
        }
    }

    // The three process markers are console evidence, not just exit codes: the
    // fork child writes the first, the parent the second, and the forked child
    // that exec'd `/mnt/exec-child` writes the third (with the argv/envp it
    // actually received). `exec` is disk-only because its target lives on /mnt.
    let mut markers_ok = captured_contains(&captured, b"abi: fork ok\n")
        && captured_contains(&captured, b"abi: wait ok\n");
    if dir.is_some() {
        markers_ok &= captured_contains(
            &captured,
            b"abi: exec ok argc=2 argv=exec-arg env=PWD=/mnt\n",
        );
    }
    if !markers_ok {
        crate::kprintln!(
            "[selftest] user: process markers missing from {:?}",
            core::str::from_utf8(&captured.bytes[..captured.len]).unwrap_or("<non-UTF-8>")
        );
        ok = false;
    }

    // Counters: the fixture must have gone through the new entry points. The
    // filesystem-independent set is required on every boot; the file half only
    // when /mnt existed.
    if stats.brks <= stats_before.brks
        || stats.clocks <= stats_before.clocks
        || stats.unames <= stats_before.unames
        || stats.opens <= stats_before.opens
        || stats.closes <= stats_before.closes
        || stats.fstats <= stats_before.fstats
        || stats.sleeps <= stats_before.sleeps
        || stats.mmaps <= stats_before.mmaps
        || stats.munmaps <= stats_before.munmaps
        || stats.mprotects <= stats_before.mprotects
        || stats.pipes <= stats_before.pipes
        || stats.dups <= stats_before.dups
        || stats.polls <= stats_before.polls
        || stats.sockets <= stats_before.sockets
        || stats.sockopts <= stats_before.sockopts
        || stats.clones <= stats_before.clones
        || stats.futex_waits <= stats_before.futex_waits
        || stats.futex_wakes <= stats_before.futex_wakes
        || stats.forks <= stats_before.forks
        || stats.waits <= stats_before.waits
        || stats.getppids <= stats_before.getppids
        || (dir.is_some() && stats.execs <= stats_before.execs)
    {
        crate::kprintln!(
            "[selftest] user: ABI counters did not move as expected (before {:?}, after {:?})",
            stats_before,
            stats
        );
        ok = false;
    }
    if dir.is_some()
        && (stats.exit_groups <= stats_before.exit_groups
            || stats.reads <= stats_before.reads
            || stats.seeks <= stats_before.seeks
            || stats.dirents <= stats_before.dirents
            || stats.readlinks <= stats_before.readlinks
            || stats.mkdirs <= stats_before.mkdirs
            || stats.unlinks <= stats_before.unlinks
            || stats.symlinks <= stats_before.symlinks
            || stats.links <= stats_before.links
            || stats.renames <= stats_before.renames
            || stats.chmods <= stats_before.chmods
            || stats.truncates <= stats_before.truncates)
    {
        crate::kprintln!(
            "[selftest] user: filesystem ABI counters did not move (before {:?}, after {:?})",
            stats_before,
            stats
        );
        ok = false;
    }

    // Pipe rings are static memory, not PMM frames, so the ledger cannot see a
    // leaked one. The fixture closes every end; a non-zero count would mean a
    // reference was lost and the ring would stay allocated for the whole boot.
    if crate::user::process::live_processes() != 0 {
        crate::kprintln!(
            "[selftest] user: the ABI run left {} process slot(s)",
            crate::user::process::live_processes()
        );
        ok = false;
    }
    if crate::user::syscall::live_pipes() != 0 {
        crate::kprintln!(
            "[selftest] user: the ABI run left {} pipe(s) allocated",
            crate::user::syscall::live_pipes()
        );
        ok = false;
    }
    if crate::user::futex::live_waiters() != 0 {
        crate::kprintln!(
            "[selftest] user: the ABI run left {} futex waiter(s) registered",
            crate::user::futex::live_waiters()
        );
        ok = false;
    }

    // The signal section is filesystem-independent: it must have run on every
    // boot. The counters are the kernel-side witness that the fixture really
    // installed a handler, entered it through `kill`, and returned through
    // `rt_sigreturn`; the zero live counts prove the run-scoped table was torn
    // down with the run.
    let signals = crate::user::signal::stats();
    if signals.actions <= signals_before.actions
        || signals.procmasks <= signals_before.procmasks
        || signals.kills <= signals_before.kills
        || signals.sigreturns <= signals_before.sigreturns
        || signals.delivered <= signals_before.delivered
        || signals.ignored <= signals_before.ignored
        || signals.altstacks <= signals_before.altstacks
        || signals.stops <= signals_before.stops
        || signals.continues <= signals_before.continues
        || signals.restarts <= signals_before.restarts
    {
        crate::kprintln!(
            "[selftest] user: signal counters did not move as expected (before {:?}, after {:?})",
            signals_before,
            signals
        );
        ok = false;
    }
    // `faults` counts synchronous SIGSEGV/SIGILL *deliveries*; the fixture
    // deliberately installs a handler and takes one, which is success, not the
    // run ending. Only a default action (terminated) is a failure here.
    if signals.terminated != signals_before.terminated {
        crate::kprintln!(
            "[selftest] user: the ABI run ended through a default signal action (before {:?}, after {:?})",
            signals_before,
            signals
        );
        ok = false;
    }
    if crate::user::signal::live_handlers() != 0
        || crate::user::signal::live_threads() != 0
        || crate::user::signal::live_pending() != 0
        || crate::task::scheduler::stopped_tasks() != 0
    {
        crate::kprintln!(
            "[selftest] user: the ABI run left signal state ({} handler(s), {} thread(s), {} pending)",
            crate::user::signal::live_handlers(),
            crate::user::signal::live_threads(),
            crate::user::signal::live_pending()
        );
        ok = false;
    }

    if frames() != frames_before {
        crate::kprintln!(
            "[selftest] user: the ABI run left the PMM at {} (was {})",
            frames(),
            frames_before
        );
        ok = false;
    }
    let (used, mapped, _) = crate::heap_stats();
    if used != used_before || mapped != mapped_before {
        crate::kprintln!(
            "[selftest] user: the ABI run left the heap at {} B used / {} B mapped (was {}/{} B)",
            used,
            mapped,
            used_before,
            mapped_before
        );
        ok = false;
    }

    if !ok {
        return Outcome::Fail;
    }
    if dir.is_none() {
        Outcome::Skip("no filesystem mounted at /mnt (the filesystem half did not run)")
    } else {
        Outcome::Pass
    }
}

/// The shell path's loader refusal: `run_image` must return `UserError::Elf`,
/// not a half-started task, and the ledger must not move.
fn run_rejects_bad_elf(buf: &mut Vec<u8>) -> bool {
    reset(buf);
    bad_magic(buf.as_mut_slice());
    let before = frames();
    let result = user::run_image(buf.as_slice(), "reject");
    let after = frames();
    let ok = matches!(result, Err(UserError::Elf(ElfError::BadMagic))) && before == after;
    if !ok {
        crate::kprintln!(
            "[selftest] user: run_image on a bad ELF returned {result:?} (frames {before} -> {after})"
        );
    }
    ok
}

/// The acceptance run: `hello` prints through `write`, yields and exits 42 — and
/// two consecutive runs each put every frame back.
fn user_hello(_buf: &mut Vec<u8>) -> bool {
    let before = frames();
    let writes = syscall::stats().writes;
    let yields = syscall::stats().yields;
    let mut ok = true;
    for run in 0..2u32 {
        match user::run_image(HELLO_ELF, "hello") {
            Ok(Exit::Code(42)) => {}
            Ok(other) => {
                crate::kprintln!("[selftest] user: hello run {run} ended {other}");
                ok = false;
            }
            Err(error) => {
                crate::kprintln!("[selftest] user: hello run {run} failed: {error}");
                ok = false;
            }
        }
        if frames() != before {
            crate::kprintln!(
                "[selftest] user: hello run {run} left the ledger at {} (was {before})",
                frames()
            );
            ok = false;
        }
    }
    let stats = syscall::stats();
    if stats.writes < writes + 2 || stats.yields < yields + 2 {
        crate::kprintln!(
            "[selftest] user: syscall counters did not move as expected (writes {} -> {}, yields {} -> {})",
            writes,
            stats.writes,
            yields,
            stats.yields
        );
        ok = false;
    }
    ok
}

/// The fault run: a load from address 0 kills one task, returns `Exit::Fault`
/// with the page-fault cause, and leaves the ledger alone.
fn user_fault(_buf: &mut Vec<u8>) -> bool {
    let before = frames();
    let ok = match user::run_image(FAULT_ELF, "fault") {
        Ok(Exit::Fault { cause, .. }) if cause == CAUSE_LOAD_PAGE_FAULT => true,
        Ok(other) => {
            crate::kprintln!("[selftest] user: the fault fixture ended {other}");
            false
        }
        Err(error) => {
            crate::kprintln!("[selftest] user: the fault fixture failed to start: {error}");
            false
        }
    };
    if frames() != before {
        crate::kprintln!(
            "[selftest] user: the killed task left the ledger at {} (was {before})",
            frames()
        );
        return false;
    }
    ok
}

/// The machine is still a machine after the fault: the next program runs normally.
fn user_after_fault(_buf: &mut Vec<u8>) -> bool {
    let before = frames();
    let ok = matches!(
        user::run_image(HELLO_ELF, "after-fault"),
        Ok(Exit::Code(42))
    );
    if !ok {
        crate::kprintln!("[selftest] user: a program after the fault did not exit 42");
    }
    if frames() != before {
        crate::kprintln!(
            "[selftest] user: the run after the fault left the ledger at {} (was {before})",
            frames()
        );
        return false;
    }
    ok
}

/// The timer-preemption proof: `spin` runs in U-mode for 0.2 s, i.e. twenty tick
/// periods. The counter of timer interrupts taken with saved `SPP == 0` must grow,
/// the program must still return through `ecall` afterwards and exit with 7, and
/// the frames must come back.
fn user_timer_preempts(_buf: &mut Vec<u8>) -> bool {
    let before = frames();
    let ticks_before = crate::arch::riscv64::traps::user_timer_ticks();
    let mut ok = match user::run_image(SPIN_ELF, "spin") {
        Ok(Exit::Code(7)) => true,
        Ok(other) => {
            crate::kprintln!("[selftest] user: the spin fixture ended {other}");
            false
        }
        Err(error) => {
            crate::kprintln!("[selftest] user: the spin fixture failed: {error}");
            false
        }
    };
    let ticks_after = crate::arch::riscv64::traps::user_timer_ticks();
    if ticks_after <= ticks_before {
        crate::kprintln!(
            "[selftest] user: no timer interrupt was taken from U-mode (counter {} -> {})",
            ticks_before,
            ticks_after
        );
        ok = false;
    }
    if frames() != before {
        crate::kprintln!(
            "[selftest] user: the spin run left the ledger at {} (was {before})",
            frames()
        );
        ok = false;
    }
    ok
}

/// The large-image check's scratch file and its padded size.
///
/// 100 KiB is deliberately more than one 64 KiB heap chunk (the old `run
/// <path>` ceiling) and far more than the hello fixture's few KiB of segments:
/// the loader must read only what the headers describe, so the zero tail never
/// becomes an `ElfError`.
const LARGE_FILE_PATH: &str = "/mnt/selftest-large.elf";
const LARGE_FILE_NAME: &str = "selftest-large.elf";
const LARGE_FILE_BYTES: u64 = 100 * 1024;

/// Padding write size. One page: the check must never allocate a file-sized
/// buffer itself, or it would not prove anything about the loader.
const LARGE_WRITE_CHUNK: usize = 4096;

/// Outcome of one check. A skip is neither a pass nor a failure and is counted
/// separately, so a boot without a disk cannot look like full coverage (the
/// same convention as the block suite).
enum Outcome {
    Pass,
    Fail,
    Skip(&'static str),
}

/// Wrap a boolean check in the [`Outcome`] type.
fn verdict(ok: bool) -> Outcome {
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail
    }
}

/// Write the hello fixture to the scratch file and pad it to
/// [`LARGE_FILE_BYTES`] with zeros, one 4 KiB chunk at a time.
fn write_large_image(file: &Arc<dyn VfsNode>) -> Result<(), &'static str> {
    match file.write(0, HELLO_ELF) {
        Ok(written) if written == HELLO_ELF.len() => {}
        _ => return Err("writing the hello fixture to the scratch file failed"),
    }
    let zeros = [0u8; LARGE_WRITE_CHUNK];
    let mut offset = HELLO_ELF.len() as u64;
    while offset < LARGE_FILE_BYTES {
        let take = core::cmp::min(LARGE_WRITE_CHUNK as u64, LARGE_FILE_BYTES - offset) as usize;
        match file.write(offset, &zeros[..take]) {
            Ok(written) if written == take => offset += take as u64,
            Ok(_) => return Err("the scratch file accepted a short padding write"),
            Err(_) => return Err("writing the image padding failed"),
        }
    }
    if file.size() != LARGE_FILE_BYTES {
        return Err("the scratch image is not the size that was written");
    }
    Ok(())
}

/// `run <path>` on an image larger than one 64 KiB heap chunk.
///
/// The file is the hello fixture followed by zero padding to 100 KiB. Every ELF
/// header still describes its few KiB of `PT_LOAD` bytes, so a streaming loader
/// stops at the last segment, ignores the tail and exits 42; a loader that
/// validated or copied the whole file would fail on the size. The file is
/// removed on every path (a failed write included), and the PMM and heap
/// ledgers are compared with the values taken before it was created.
fn user_run_large_file(_buf: &mut Vec<u8>) -> Outcome {
    let dir = match vfs::walk("/mnt", true) {
        Ok(node) if node.is_directory() => node,
        Ok(_) => {
            crate::kprintln!("[selftest] user: /mnt is not a directory");
            return Outcome::Fail;
        }
        Err(VfsError::NotFound) => {
            return Outcome::Skip("no filesystem mounted at /mnt");
        }
        Err(error) => {
            crate::kprintln!("[selftest] user: cannot resolve /mnt: {}", error.as_str());
            return Outcome::Fail;
        }
    };

    // Idempotency: a crashed earlier pass must not turn the next one into
    // `AlreadyExists` (and must not leave a 100 KiB file on the disk either).
    match dir.remove(LARGE_FILE_NAME) {
        Ok(()) | Err(VfsError::NotFound) => {}
        Err(error) => {
            crate::kprintln!(
                "[selftest] user: cannot remove stale {}: {}",
                LARGE_FILE_PATH,
                error.as_str()
            );
            return Outcome::Fail;
        }
    }

    // `dir` is alive on both sides of the measurement, and so is the heap chunk
    // that backs it; only the scratch file's own allocations should move.
    let (used_before, mapped_before, _) = crate::heap_stats();
    let frames_before = frames();
    let mut ok = true;

    match dir.create_file(LARGE_FILE_NAME) {
        Ok(file) => {
            if let Err(reason) = write_large_image(&file) {
                crate::kprintln!("[selftest] user: {}", reason);
                ok = false;
            }
            // Drop our handle before the loader opens its own: the ledger below
            // must not include a scratch `Arc` the test forgot.
            drop(file);
        }
        Err(error) => {
            crate::kprintln!(
                "[selftest] user: cannot create {}: {}",
                LARGE_FILE_PATH,
                error.as_str()
            );
            ok = false;
        }
    }

    if ok {
        match user::run_file(LARGE_FILE_PATH) {
            Ok(Exit::Code(42)) => {}
            Ok(other) => {
                crate::kprintln!(
                    "[selftest] user: the 100 KiB image ended {other}, expected exit code 42"
                );
                ok = false;
            }
            Err(error) => {
                crate::kprintln!("[selftest] user: the 100 KiB image failed: {error}");
                ok = false;
            }
        }
    }

    match dir.remove(LARGE_FILE_NAME) {
        Ok(()) | Err(VfsError::NotFound) => {}
        Err(error) => {
            crate::kprintln!(
                "[selftest] user: cannot remove {}: {}",
                LARGE_FILE_PATH,
                error.as_str()
            );
            ok = false;
        }
    }

    if frames() != frames_before {
        crate::kprintln!(
            "[selftest] user: the large-file run left the PMM at {} (was {})",
            frames(),
            frames_before
        );
        ok = false;
    }
    let (used, mapped, _) = crate::heap_stats();
    if used != used_before || mapped != mapped_before {
        crate::kprintln!(
            "[selftest] user: the large-file run left the heap at {} B used / {} B mapped (was {}/{} B)",
            used,
            mapped,
            used_before,
            mapped_before
        );
        ok = false;
    }
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail
    }
}

/// Run every check, print `SELFTEST user-<name> PASS|FAIL|SKIP`, and return how
/// many passed.
pub fn run() -> usize {
    // The shell's `spawn` command may have left a demo cohort running. Its
    // threads are legitimate persistent state, but the ledger this suite prints
    // must not depend on what the user did before `selftest`.
    crate::task::scheduler::cancel_demo();

    // One reusable scratch copy of the fixture. Allocated before the baseline so
    // the heap chunk the harness itself takes is not counted as a leak.
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(HELLO_ELF.len()).is_err() {
        crate::kprintln!("[selftest] user: no heap for the scratch ELF buffer; suite skipped");
        crate::kprintln!("SELFTEST SUMMARY (user): 0/0 passed");
        return 0;
    }
    buf.extend_from_slice(HELLO_ELF);

    let (before, total) = pmm::stats();
    crate::kprintln!(
        "[selftest] user PMM free frames before: {} of {}",
        before,
        total
    );

    let checks: [(&str, fn(&mut Vec<u8>) -> Outcome); 10] = [
        ("user-abi-argv", user_abi_argv),
        ("user-abi-syscalls", user_abi_syscalls),
        ("user-elf-accept", |buf| verdict(elf_accept(buf))),
        ("user-elf-reject", |buf| verdict(elf_rejects(buf))),
        ("user-run-rejects-bad-elf", |buf| {
            verdict(run_rejects_bad_elf(buf))
        }),
        ("user-hello-roundtrip", |buf| verdict(user_hello(buf))),
        ("user-fault-isolated", |buf| verdict(user_fault(buf))),
        ("user-after-fault", |buf| verdict(user_after_fault(buf))),
        ("user-timer-preempts", |buf| {
            verdict(user_timer_preempts(buf))
        }),
        ("user-run-large-file", user_run_large_file),
    ];
    let mut passed = 0usize;
    let mut skipped = 0usize;
    for (name, check) in checks {
        match check(&mut buf) {
            Outcome::Pass => {
                passed += 1;
                crate::kprintln!("SELFTEST {} PASS", name);
            }
            Outcome::Fail => crate::kprintln!("SELFTEST {} FAIL", name),
            Outcome::Skip(why) => {
                skipped += 1;
                crate::kprintln!("SELFTEST {} SKIP ({})", name, why);
            }
        }
    }

    // Anything a failed check left exited is reaped here, so even a failing pass
    // hands the pool back the way it found it.
    crate::task::scheduler::reap();
    let (after, _) = pmm::stats();
    if skipped == 0 {
        crate::kprintln!(
            "SELFTEST SUMMARY (user): {}/{} passed",
            passed,
            checks.len()
        );
    } else {
        crate::kprintln!(
            "SELFTEST SUMMARY (user): {}/{} passed, {} skipped",
            passed,
            checks.len(),
            skipped
        );
    }
    crate::kprintln!(
        "[selftest] user PMM hygiene: free frames {} -> {}",
        before,
        after
    );
    drop(buf);
    passed
}
