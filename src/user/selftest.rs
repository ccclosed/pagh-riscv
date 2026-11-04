//! The user-mode (M6b) self-tests, run by the shell's `selftest` after the task
//! suite.
//!
//! The contract is the same as the M6a/M5 suites: non-destructive, deterministic
//! and idempotent. The property this suite exists to prove is the *ledger*
//! property — a started program, a refused image and a killed process all leave
//! the PMM exactly as they found it — plus the two acceptance runs the milestone
//! names: `hello` exits 42 through `write`/`sched_yield`/`exit`, and `fault` dies
//! from a U-mode page fault without taking the machine with it.
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
use crate::user::{self, Exit, UserError, ARGV_ELF, FAULT_ELF, HELLO_ELF, SPIN_ELF};
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

    let checks: [(&str, fn(&mut Vec<u8>) -> Outcome); 9] = [
        ("user-abi-argv", user_abi_argv),
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
