//! The ELF64 loader: validate a static RISC-V executable, map its `PT_LOAD`
//! pages with the user bit, copy the file bytes and zero the `.bss` tail.
//!
//! This is deliberately an *ELF reader for one job*, not a general-purpose
//! loader:
//!
//! * **One address space.** Pages are installed in the kernel's existing Sv39
//!   tree with `U=1`; there is no second `satp` or per-process page table. The
//!   isolation this buys is exactly one bit: supervisor pages are mapped without
//!   `U`, so U-mode cannot read, write or execute them. It does *not* give a
//!   separate physical memory namespace, an ASID, or protection between two user
//!   processes (only one runs at a time; see `src/user/mod.rs`).
//! * **Validate before mapping where possible.** Header, program-header table,
//!   per-segment bounds, flags, alignment, the entry point and the page count are
//!   all checked against the input buffer and the user window before the first
//!   frame is allocated. A request that fails here costs the PMM nothing.
//! * **Roll back mapping failures.** Once pages are mapped, an error (an address
//!   already mapped, an empty frame pool) unwinds every page this call installed
//!   and returns the frames. The selftest's ledger check is what keeps that
//!   promise honest.
//! * **No panic on corrupt input.** Every read from the buffer is bounds-checked
//!   and every arithmetic step on a header field is checked; a malformed ELF is
//!   an [`ElfError`], never an index panic in a kernel with `panic = "abort"`.
//!
//! The user virtual layout is fixed and small:
//!
//! ```text
//! 0x0001_0000 .. 0x3fff_0000   image window (PT_LOAD pages, U=1)
//! 0x3fff_0000 .. 0x4000_0000   user stack (64 KiB, U=1)
//! 0x4000_0000                  stack top / initial sp
//! ```
//!
//! The window sits below the kernel's identity map (`0x8000_0000`) and below
//! nothing else the kernel maps: the PLIC lives at `0x0c00_0000` and the UART at
//! `0x1000_0000`, both inside the window's numeric range. A crafted ELF that
//! asks for those pages is refused by the page-table walk (`AlreadyMapped`),
//! which is the same mechanism that rejects overlap with a previous user mapping.

use core::fmt;

use crate::arch::riscv64::mmu::{Flags, PTE_A, PTE_D, PTE_R, PTE_U, PTE_V, PTE_W, PTE_X};
use crate::memory::{paging, pmm, FRAME_SIZE};

/// Lowest virtual address the loader is willing to map.
pub const USER_BASE: usize = 0x0001_0000;

/// One past the highest image address; also the user stack's top.
pub const USER_TOP: usize = 0x4000_0000;

/// Bytes of user stack the process starts with.
pub const USER_STACK_SIZE: usize = 64 * 1024;

/// Lowest address of the user stack (a guard page sits below it, unmapped).
pub const USER_STACK_BASE: usize = USER_TOP - USER_STACK_SIZE;

/// Frames in the user stack.
pub const USER_STACK_PAGES: usize = USER_STACK_SIZE / FRAME_SIZE;

/// Largest image the loader will map, in bytes (and therefore in pages).
pub const MAX_IMAGE_BYTES: usize = 1 << 20;

/// Largest number of 4 KiB pages one image may occupy.
pub const MAX_IMAGE_PAGES: usize = MAX_IMAGE_BYTES / FRAME_SIZE;

/// Largest number of `PT_LOAD` program headers accepted.
pub const MAX_SEGMENTS: usize = 8;

/// Largest program-header table accepted (a fixed bound, no allocation).
pub const MAX_PHDRS: usize = 8;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// Why an ELF was refused, or a page could not be installed.
///
/// The variants are the loader's public diagnostic vocabulary: `run` prints
/// [`ElfError::as_str`], the selftest matches them, and none of them can be
/// produced by a panic path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Shorter than one ELF64 header.
    Short,
    /// `e_ident[EI_MAG0..3]` is not `0x7f 'E' 'L' 'F'`.
    BadMagic,
    /// `EI_CLASS` is not 2 (ELFCLASS64).
    BadClass,
    /// `EI_DATA` is not 1 (little-endian); RV64 Linux userland is little-endian.
    BadData,
    /// `EI_VERSION` is not 1.
    BadIdentVersion,
    /// `e_type` is neither `ET_EXEC` nor `ET_DYN`.
    BadType,
    /// `e_machine` is not RISC-V.
    BadMachine,
    /// `e_version`, `e_ehsize` or `e_phentsize` is not what ELF64 requires.
    BadHeader,
    /// The program-header table does not fit in the buffer.
    ProgramHeadersOutOfBounds,
    /// One `PT_LOAD` header contradicts itself (`filesz > memsz`, bad flags,
    /// bad alignment) or points past the buffer.
    BadSegment,
    /// No `PT_LOAD` header at all, or more than [`MAX_SEGMENTS`].
    NoLoadableSegment,
    /// The image needs more than [`MAX_IMAGE_PAGES`] or [`MAX_IMAGE_BYTES`].
    ImageTooLarge,
    /// A segment lands outside the user window (or overlaps the user stack).
    SegmentOutsideUserWindow,
    /// `e_entry` is not inside an executable segment.
    EntryOutsideText,
    /// A bias was supplied for an `ET_EXEC` file, or the bias is unusable.
    BadBias,
    /// A header field overflows when biased or rounded to a page.
    Overflow,
    /// The page is already mapped (a kernel/device page or a duplicate segment
    /// in the same image).
    AddressInUse(usize),
    /// The frame allocator is empty.
    OutOfFrames,
    /// A page-table operation failed for another reason.
    Map(paging::MapError),
}

impl ElfError {
    /// A one-line description for the shell; stable enough to grep.
    pub fn as_str(self) -> &'static str {
        match self {
            ElfError::Short => "not an ELF (shorter than the 64-byte header)",
            ElfError::BadMagic => "not an ELF (bad magic)",
            ElfError::BadClass => "not a 64-bit ELF (bad class)",
            ElfError::BadData => "not a little-endian ELF",
            ElfError::BadIdentVersion => "unknown ELF identification version",
            ElfError::BadType => "not an executable ELF (need ET_EXEC or ET_DYN)",
            ElfError::BadMachine => "not a RISC-V ELF",
            ElfError::BadHeader => "malformed ELF header",
            ElfError::ProgramHeadersOutOfBounds => "program headers outside the file",
            ElfError::BadSegment => "malformed PT_LOAD segment",
            ElfError::NoLoadableSegment => "no PT_LOAD segments",
            ElfError::ImageTooLarge => "image is too large for the user window",
            ElfError::SegmentOutsideUserWindow => "segment outside the user windows",
            ElfError::EntryOutsideText => "entry point is outside an executable segment",
            ElfError::BadBias => "invalid load bias",
            ElfError::Overflow => "ELF field overflows the address space",
            ElfError::AddressInUse(_) => "a segment would replace a live mapping",
            ElfError::OutOfFrames => "the frame allocator is empty",
            ElfError::Map(_) => "page mapping failed",
        }
    }
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElfError::Map(e) => write!(f, "{}: {e}", self.as_str()),
            ElfError::AddressInUse(address) => {
                write!(f, "{} at {address:#x}", self.as_str())
            }
            _ => f.write_str(self.as_str()),
        }
    }
}

/// One validated `PT_LOAD` header, in user virtual addresses.
#[derive(Clone, Copy)]
struct Segment {
    file_offset: usize,
    vaddr: usize,
    filesz: usize,
}

/// One page to install, with the union of the flags of every segment covering it.
#[derive(Clone, Copy)]
struct PagePlan {
    vaddr: usize,
    flags: u64,
}

/// A successfully loaded image: its entry point and the pages it owns.
///
/// The caller is responsible for [`Loaded::release`] once the process is gone;
/// dropping the value does not unmap anything (there is no `Drop` impl, because
/// unmapping can allocate/free page tables and this kernel's teardown paths are
/// explicit).
pub struct Loaded {
    /// User virtual address to start at.
    pub entry: usize,
    pages: [usize; MAX_IMAGE_PAGES],
    count: usize,
}

impl Loaded {
    const fn empty() -> Self {
        Loaded {
            entry: 0,
            pages: [0; MAX_IMAGE_PAGES],
            count: 0,
        }
    }

    /// The user virtual addresses this image owns, in mapping order.
    pub fn pages(&self) -> &[usize] {
        &self.pages[..self.count]
    }

    /// Unmap every page and return its physical frame to the PMM.
    ///
    /// Idempotent: a second call has nothing to release. Errors from `unmap`
    /// (a page that disappeared) are ignored here because there is no better
    /// answer after a process has ended; the ledger check catches a real leak.
    pub fn release(&mut self) {
        for index in 0..self.count {
            if let Ok(frame) = paging::unmap(self.pages[index]) {
                pmm::free_frame(frame);
            }
            self.pages[index] = 0;
        }
        self.count = 0;
    }
}

/// Read a little-endian `u16` at `offset`, or `None` when it is out of bounds.
fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let slice = bytes.get(offset..end)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

/// Read a little-endian `u32` at `offset`, or `None`.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Read a little-endian `u64` at `offset`, or `None`.
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let slice = bytes.get(offset..end)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

/// Load `bytes` at an optional `bias`.
///
/// `bias` is added to every `p_vaddr` and to `e_entry`; it is how a static PIE
/// (`ET_DYN`) is placed. `ET_EXEC` files must be loaded with `bias == 0`.
///
/// On `Err`, every page mapped by this call has already been unmapped and its
/// frame returned: the PMM ledger is the same as before the call.
pub fn load(bytes: &[u8], bias: usize) -> Result<Loaded, ElfError> {
    // ── ELF header ────────────────────────────────────────────────────────
    if bytes.len() < EHDR_SIZE {
        return Err(ElfError::Short);
    }
    if bytes.get(0..4) != Some(b"\x7fELF") {
        return Err(ElfError::BadMagic);
    }
    if bytes.get(4) != Some(&2) {
        return Err(ElfError::BadClass);
    }
    if bytes.get(5) != Some(&1) {
        return Err(ElfError::BadData);
    }
    if bytes.get(6) != Some(&1) {
        return Err(ElfError::BadIdentVersion);
    }

    let e_type = u16_at(bytes, 16).ok_or(ElfError::Short)?;
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(ElfError::BadType);
    }
    if u16_at(bytes, 18).ok_or(ElfError::Short)? != EM_RISCV {
        return Err(ElfError::BadMachine);
    }
    if u32_at(bytes, 20).ok_or(ElfError::Short)? != 1 {
        return Err(ElfError::BadHeader);
    }
    let entry_raw = u64_at(bytes, 24).ok_or(ElfError::Short)? as usize;
    let phoff = u64_at(bytes, 32).ok_or(ElfError::Short)? as usize;
    let ehsize = u16_at(bytes, 52).ok_or(ElfError::Short)? as usize;
    let phentsize = u16_at(bytes, 54).ok_or(ElfError::Short)? as usize;
    let phnum = u16_at(bytes, 56).ok_or(ElfError::Short)? as usize;
    if ehsize != EHDR_SIZE || phentsize != PHDR_SIZE {
        return Err(ElfError::BadHeader);
    }
    if phnum == 0 || phnum > MAX_PHDRS {
        return Err(ElfError::NoLoadableSegment);
    }
    let ph_end = phnum
        .checked_mul(PHDR_SIZE)
        .and_then(|size| phoff.checked_add(size))
        .ok_or(ElfError::Overflow)?;
    if ph_end > bytes.len() {
        return Err(ElfError::ProgramHeadersOutOfBounds);
    }

    if e_type == ET_EXEC && bias != 0 {
        return Err(ElfError::BadBias);
    }
    if e_type == ET_DYN && bias % FRAME_SIZE != 0 {
        return Err(ElfError::BadBias);
    }
    let entry = entry_raw.checked_add(bias).ok_or(ElfError::Overflow)?;

    // ── validate every PT_LOAD before touching the page tables ────────────
    let mut segments = [Segment {
        file_offset: 0,
        vaddr: 0,
        filesz: 0,
    }; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    let mut executable_contains_entry = false;
    let mut plans = [PagePlan { vaddr: 0, flags: 0 }; MAX_IMAGE_PAGES];
    let mut plan_count = 0usize;

    for index in 0..phnum {
        let base = phoff + index * PHDR_SIZE;
        let p_type = u32_at(bytes, base).ok_or(ElfError::Short)?;
        if p_type != PT_LOAD {
            continue;
        }
        if segment_count == MAX_SEGMENTS {
            return Err(ElfError::NoLoadableSegment);
        }
        let file_offset = u64_at(bytes, base + 8).ok_or(ElfError::Short)? as usize;
        let raw_vaddr = u64_at(bytes, base + 16).ok_or(ElfError::Short)? as usize;
        let filesz = u64_at(bytes, base + 32).ok_or(ElfError::Short)? as usize;
        let memsz = u64_at(bytes, base + 40).ok_or(ElfError::Short)? as usize;
        let flags = u32_at(bytes, base + 4).ok_or(ElfError::Short)?;
        let align = u64_at(bytes, base + 48).ok_or(ElfError::Short)? as usize;

        if filesz > memsz {
            return Err(ElfError::BadSegment);
        }
        // The permission bits are the only meaningful ones in `p_flags`; a
        // write-only segment has no valid Sv39 leaf encoding here (W without R
        // is reserved), and read-less code/data is not something this loader
        // supports.
        if flags & PF_R == 0 {
            return Err(ElfError::BadSegment);
        }
        let file_end = file_offset.checked_add(filesz).ok_or(ElfError::Overflow)?;
        if file_end > bytes.len() {
            return Err(ElfError::BadSegment);
        }
        let vaddr = raw_vaddr.checked_add(bias).ok_or(ElfError::Overflow)?;
        let mem_end = vaddr.checked_add(memsz).ok_or(ElfError::Overflow)?;
        if memsz > MAX_IMAGE_BYTES {
            return Err(ElfError::ImageTooLarge);
        }
        if align > 1 {
            // ELF requires a power of two and congruence between the virtual
            // address and the file offset. A non-conforming file is refused
            // rather than loaded at an address the linker did not intend.
            if !align.is_power_of_two() || vaddr % align != file_offset.wrapping_add(bias) % align {
                return Err(ElfError::BadSegment);
            }
        }
        // Empty segments contribute no mapping but are otherwise harmless.
        if memsz == 0 {
            continue;
        }
        let page_start = vaddr & !(FRAME_SIZE - 1);
        let page_end = mem_end
            .checked_add(FRAME_SIZE - 1)
            .ok_or(ElfError::Overflow)?
            & !(FRAME_SIZE - 1);
        if page_start < USER_BASE || page_end > USER_STACK_BASE {
            return Err(ElfError::SegmentOutsideUserWindow);
        }
        if flags & PF_X != 0 && entry >= vaddr && entry < mem_end {
            executable_contains_entry = true;
        }

        // Walk the pages this segment covers and merge flags into the plan.
        let mut page = page_start;
        while page < page_end {
            let mut merged = PTE_V | PTE_U | PTE_A | PTE_D;
            if flags & PF_R != 0 {
                merged |= PTE_R;
            }
            if flags & PF_W != 0 {
                merged |= PTE_W;
            }
            if flags & PF_X != 0 {
                merged |= PTE_X;
            }
            match plans[..plan_count]
                .iter_mut()
                .find(|plan| plan.vaddr == page)
            {
                Some(plan) => plan.flags |= merged,
                None => {
                    if plan_count == MAX_IMAGE_PAGES {
                        return Err(ElfError::ImageTooLarge);
                    }
                    plans[plan_count] = PagePlan {
                        vaddr: page,
                        flags: merged,
                    };
                    plan_count += 1;
                }
            }
            page += FRAME_SIZE;
        }

        segments[segment_count] = Segment {
            file_offset,
            vaddr,
            filesz,
        };
        segment_count += 1;
    }

    if segment_count == 0 {
        return Err(ElfError::NoLoadableSegment);
    }
    if !executable_contains_entry {
        return Err(ElfError::EntryOutsideText);
    }

    // ── map (rollback on the first failure) ───────────────────────────────
    let mut loaded = Loaded::empty();
    loaded.entry = entry;
    for plan in &plans[..plan_count] {
        let Some(frame) = pmm::alloc_frame() else {
            loaded.release();
            return Err(ElfError::OutOfFrames);
        };
        // SAFETY: `frame` is a fresh PMM frame, 4 KiB aligned and owned by this
        // loader; RAM is identity-mapped supervisor RW (and not yet user
        // accessible), so zeroing it through its physical address is valid. The
        // whole frame is cleared, which is what gives the segment's `.bss` tail
        // its zeros.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME_SIZE) };
        match paging::map(plan.vaddr, frame, Flags::from_bits(plan.flags)) {
            Ok(()) => {
                loaded.pages[loaded.count] = plan.vaddr;
                loaded.count += 1;
            }
            Err(paging::MapError::AlreadyMapped(address)) => {
                pmm::free_frame(frame);
                loaded.release();
                return Err(ElfError::AddressInUse(address));
            }
            Err(error) => {
                pmm::free_frame(frame);
                loaded.release();
                return Err(ElfError::Map(error));
            }
        }
    }

    // ── copy file bytes; the rest of every page is already zero ───────────
    for segment in &segments[..segment_count] {
        let mut copied = 0usize;
        while copied < segment.filesz {
            let va = segment.vaddr + copied;
            let page = va & !(FRAME_SIZE - 1);
            let offset_in_page = va - page;
            let available = FRAME_SIZE - offset_in_page;
            let bytes_left = segment.filesz - copied;
            let chunk = available.min(bytes_left);
            let source_start = segment.file_offset + copied;
            let Some(source) = bytes.get(source_start..source_start + chunk) else {
                loaded.release();
                return Err(ElfError::BadSegment);
            };
            let Some(frame) = paging::translate(page) else {
                loaded.release();
                return Err(ElfError::Map(paging::MapError::NotMapped(page)));
            };
            // SAFETY: `frame` is the physical base of a page this call mapped,
            // and the destination range `offset_in_page..offset_in_page+chunk`
            // lies inside that 4 KiB frame. `source` is a validated subslice of
            // the caller's buffer; the regions cannot overlap because one is the
            // ELF buffer and the other is a PMM frame, and nothing else is using
            // either for the duration of this copy. The frame is still mapped
            // supervisor-RW in the identity map, which is how this store reaches
            // it before the user ever runs.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    (frame + offset_in_page) as *mut u8,
                    chunk,
                )
            };
            copied += chunk;
        }
    }

    Ok(loaded)
}
