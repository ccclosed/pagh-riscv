//! The block stack's in-QEMU self-tests, run by the shell's `selftest`.
//!
//! Contract, in the same spirit as `memory/selftest.rs`: non-destructive,
//! deterministic and idempotent. Two rules decide the shape:
//!
//! * the **registered device is only ever read** — capacity, the first and last
//!   sector, `flush`, and the refusals (misaligned buffer, request past the end).
//!   A selftest that wrote to the user's disk image would make the command
//!   dangerous, and an interrupted write has no rollback;
//! * the **write path is exercised on the RAM-backed [`MemBlock`]** from
//!   `drivers/block.rs`, which proves the trait's write/read contract — and, with
//!   it, the seam every driver plugs into — without touching the disk.
//!
//! `run` prints `SELFTEST <name> PASS|FAIL` per routine, a summary, and the
//! heap's used/mapped numbers before and after the run. The memory suite's PMM
//! ledger catches frame leaks between passes; this line catches a block routine
//! that forgets to drop what it allocated, which is the other way two `selftest`
//! passes start to differ.
//!
//! The checks print and return a verdict instead of asserting: the caller is the
//! shell, whose job is to keep running, and the verdict on the serial line is
//! what the E2E tools read.

use crate::drivers::block::{self, BlockDevice, MemBlock, SECTOR_SIZE};

/// Sectors of the RAM device: 4 KiB, enough for a write that crosses a sector
/// boundary and for the untouched-sectors checks around it.
const MEMBLOCK_SECTORS: u64 = 8;

/// Byte the RAM device starts filled with, so a read can prove it read the
/// device's bytes and not a zeroed buffer.
const MEMBLOCK_PATTERN: u8 = 0xa5;

/// Byte the round-trip writes. Deliberately not [`MEMBLOCK_PATTERN`].
const MEMBLOCK_WRITE_PATTERN: u8 = 0x5a;

/// First sector the round trip writes; it writes two sectors from here.
const MEMBLOCK_WRITE_START: u64 = 2;

/// Outcome of one routine. A skipped check is not a pass: the summary says how
/// many were skipped, so a boot without a disk cannot look like full coverage.
enum Outcome {
    Pass,
    Fail,
    Skip(&'static str),
}

/// Run every routine, print the verdicts, and return how many passed.
pub fn run() -> usize {
    let (used_before, mapped_before, chunks_before) = crate::heap_stats();
    let checks: [(&str, fn() -> Outcome); 2] = [
        ("block-memblock", memblock_roundtrip),
        ("block-device", registered_device_read_only),
    ];

    let mut passed = 0usize;
    let mut skipped = 0usize;
    for (name, check) in checks {
        match check() {
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

    // The hygiene check is a check of its own: every routine above is expected
    // to hand its allocations back, and a kernel with a registered driver has a
    // non-zero baseline (the driver's rings and the registry's `Arc`), so the
    // comparison is against the numbers this run found, not against zero.
    let (used, mapped, chunks) = crate::heap_stats();
    let clean = used == used_before && mapped == mapped_before;
    crate::kprintln!(
        "[selftest] block heap: {} B used / {} B mapped ({} chunk(s)) before, {} B / {} B ({} chunk(s)) after",
        used_before,
        mapped_before,
        chunks_before,
        used,
        mapped,
        chunks
    );
    let total = checks.len() + 1;
    if clean {
        passed += 1;
        crate::kprintln!("SELFTEST block-heap-hygiene PASS");
    } else {
        crate::kprintln!(
            "SELFTEST block-heap-hygiene FAIL ({} B used / {} B mapped now, {} B / {} B before)",
            used,
            mapped,
            used_before,
            mapped_before
        );
    }

    if skipped == 0 {
        crate::kprintln!("SELFTEST SUMMARY (block): {}/{} passed", passed, total);
    } else {
        crate::kprintln!(
            "SELFTEST SUMMARY (block): {}/{} passed, {} skipped",
            passed,
            total,
            skipped
        );
    }
    passed
}

/// Write a pattern through the block seam into a RAM device, read it back, and
/// check the sectors around it are untouched.
fn memblock_roundtrip() -> Outcome {
    let device = MemBlock::with_pattern("memblock-selftest", MEMBLOCK_SECTORS, MEMBLOCK_PATTERN);
    let mut ok = true;

    if device.sector_count() != MEMBLOCK_SECTORS {
        crate::kprintln!(
            "[selftest] MemBlock reports {} sectors, not {}",
            device.sector_count(),
            MEMBLOCK_SECTORS
        );
        ok = false;
    }

    // A read sees the device's contents, not a zeroed local buffer.
    let mut sector = [0u8; SECTOR_SIZE];
    match device.read_block(3, &mut sector) {
        Ok(bytes)
            if bytes == SECTOR_SIZE && sector.iter().all(|byte| *byte == MEMBLOCK_PATTERN) => {}
        other => {
            crate::kprintln!(
                "[selftest] MemBlock sector 3 read did not return the pattern: {other:?}"
            );
            ok = false;
        }
    }

    // The other constructor's documented contract: a fresh RAM device reads as
    // zeros. Cheap to check, and it keeps `MemBlock::new` from being a second
    // way to build a device that nobody ever exercises.
    let blank = MemBlock::new("memblock-blank", 1);
    let mut zeroed = [0xffu8; SECTOR_SIZE];
    match blank.read_block(0, &mut zeroed) {
        Ok(bytes) if bytes == SECTOR_SIZE && zeroed.iter().all(|byte| *byte == 0) => {}
        other => {
            crate::kprintln!("[selftest] a fresh MemBlock did not read as zeros: {other:?}");
            ok = false;
        }
    }

    // The write path, whole sectors only: two sectors starting at sector 2.
    let payload = [MEMBLOCK_WRITE_PATTERN; 2 * SECTOR_SIZE];
    match device.write_block(MEMBLOCK_WRITE_START, &payload) {
        Ok(bytes) if bytes == payload.len() => {}
        other => {
            crate::kprintln!("[selftest] MemBlock write returned {other:?}");
            ok = false;
        }
    }
    let mut readback = [0u8; 2 * SECTOR_SIZE];
    match device.read_block(MEMBLOCK_WRITE_START, &mut readback) {
        Ok(bytes) if bytes == readback.len() => {
            if !readback.iter().all(|byte| *byte == MEMBLOCK_WRITE_PATTERN) {
                crate::kprintln!("[selftest] MemBlock read back different bytes than were written");
                ok = false;
            }
        }
        other => {
            crate::kprintln!("[selftest] MemBlock read-back returned {other:?}");
            ok = false;
        }
    }
    // The sectors just outside the write still hold the original pattern: a
    // driver that ignored the block number (or wrote a whole page) shows up here.
    for (sector_number, label) in [(1u64, "before"), (4u64, "after")] {
        let mut untouched = [0u8; SECTOR_SIZE];
        match device.read_block(sector_number, &mut untouched) {
            Ok(bytes)
                if bytes == SECTOR_SIZE && untouched.iter().all(|b| *b == MEMBLOCK_PATTERN) => {}
            other => {
                crate::kprintln!(
                    "[selftest] the sector {label} the write should not have touched changed: {other:?}"
                );
                ok = false;
            }
        }
    }
    if device.flush().is_err() {
        crate::kprintln!("[selftest] MemBlock flush failed");
        ok = false;
    }

    // Refusals: the seam promises whole sectors only and an `Err` past the end.
    if device.write_block(0, &[]).is_ok() {
        crate::kprintln!("[selftest] MemBlock accepted an empty write");
        ok = false;
    }
    let mut odd = [0u8; SECTOR_SIZE + 1];
    if device.read_block(0, &mut odd).is_ok() {
        crate::kprintln!("[selftest] MemBlock accepted a non-whole-sector read");
        ok = false;
    }
    if device.read_block(MEMBLOCK_SECTORS, &mut sector).is_ok() {
        crate::kprintln!("[selftest] MemBlock accepted a read past its end");
        ok = false;
    }

    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail
    }
}

/// Read-only checks against the registered device, when there is one.
fn registered_device_read_only() -> Outcome {
    let Some(device) = block::get() else {
        return Outcome::Skip("no block device registered");
    };
    let mut ok = true;

    if device.name().is_empty() {
        crate::kprintln!("[selftest] the registered block device has an empty name");
        ok = false;
    }
    let capacity = device.sector_count();
    if capacity == 0 {
        crate::kprintln!("[selftest] the registered block device reports capacity 0");
        ok = false;
    }

    // Sector 0 and the last addressable sector, read-only: the second one is
    // what proves `sector_count` is the real capacity and not a fixed guess.
    let mut sector = [0u8; SECTOR_SIZE];
    match device.read_block(0, &mut sector) {
        Ok(bytes) if bytes == SECTOR_SIZE => {}
        other => {
            crate::kprintln!(
                "[selftest] {} sector 0 read failed: {other:?}",
                device.name()
            );
            ok = false;
        }
    }
    if capacity > 1 {
        match device.read_block(capacity - 1, &mut sector) {
            Ok(bytes) if bytes == SECTOR_SIZE => {}
            other => {
                crate::kprintln!(
                    "[selftest] {} last sector {} read failed: {other:?}",
                    device.name(),
                    capacity - 1
                );
                ok = false;
            }
        }
    }

    // A flush with no pending writes is a cache writeback, not a data change.
    if device.flush().is_err() {
        crate::kprintln!("[selftest] {} flush failed", device.name());
        ok = false;
    }

    // The driver's own guards, reached through the trait: both must be refused
    // before the device is asked to do anything.
    let mut odd = [0u8; SECTOR_SIZE + 1];
    if device.read_block(0, &mut odd).is_ok() {
        crate::kprintln!(
            "[selftest] {} accepted a non-whole-sector read",
            device.name()
        );
        ok = false;
    }
    if device.write_block(0, &[]).is_ok() {
        crate::kprintln!("[selftest] {} accepted an empty write", device.name());
        ok = false;
    }
    if device.read_block(capacity, &mut sector).is_ok() {
        crate::kprintln!(
            "[selftest] {} accepted a read past its capacity",
            device.name()
        );
        ok = false;
    }

    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail
    }
}
