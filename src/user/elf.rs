//! The ELF64 loader: validate a static RISC-V executable, map its `PT_LOAD`
//! pages with the user bit, copy the file bytes and zero the `.bss` tail.
//!
//! This is deliberately an *ELF reader for one job*, not a general-purpose
//! loader:
//!
//! * **Per-process root.** `load` installs pages in the active root (the direct
//!   selftest); `load_in(root, …)` installs them in a process's own Sv39 root.
//!   Supervisor pages are copied without `U`, so U-mode cannot read, write or
//!   execute them; two process roots share no user leaf, table frame or ASID.
//!   The caller owns the root and frees it with the process (see
//!   `src/user/mod.rs` and `src/memory/paging.rs`).
//! * **Validate before mapping where possible.** Header, program-header table,
//!   per-segment bounds, flags, alignment, the entry point and the page count are
//!   all checked against the [`ImageSource`]'s length and the user window before
//!   the first frame is allocated. A request that fails here costs the PMM
//!   nothing.
//! * **Stream, never buffer.** The image is read through [`ImageSource`] in
//!   bounded pieces: a 64-byte header, a program-header table bounded by
//!   [`MAX_PHDRS`], and each `PT_LOAD` segment copied into its already-mapped
//!   pages in 4 KiB chunks. The loader has no whole-image allocation, which is
//!   what lets `run <path>` accept an image up to [`MAX_IMAGE_BYTES`] (1 MiB)
//!   even though the kernel heap hands out 64 KiB chunks.
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
//! 0x0001_0000 .. 0x3fff_0000     image window (PT_LOAD pages, U=1; ≤ 256 pages)
//! 0x2000_0000 .. 0x2010_0000     brk heap window (see `src/user/mod.rs`)
//! 0x3000_0000 .. 0x3f00_0000     mmap search window
//! 0x3ff0_0000                    stack floor: the lowest page demand growth maps
//! 0x3ff0_0000 .. 0x3fff_0000     demand-grown stack (up to 960 KiB below)
//! 0x3fff_0000 .. 0x4000_0000     initial user stack (64 KiB, U=1)
//! 0x4000_0000                    stack top / initial sp
//! ```
//!
//! A process starts with the 16 frames of [`USER_STACK_BASE`]`..`[`USER_TOP`]
//! mapped; a U-mode load or store below that window (but no lower than
//! [`USER_STACK_FLOOR`], and close to the faulting task's `sp`) makes the kernel
//! map and zero the pages between, so a deep call chain grows the stack instead
//! of dying. The page below [`USER_STACK_FLOOR`] is never mapped by growth —
//! that is the real guard page the 1 MiB cap leaves behind. See
//! [`crate::user::stack`] for the policy.
//!
//! The window sits below the kernel's identity map (`0x8000_0000`) and below
//! nothing else the kernel maps: the PLIC lives at `0x0c00_0000` and the UART at
//! `0x1000_0000`, both inside the window's numeric range. A crafted ELF that
//! asks for those pages is refused by the page-table walk (`AlreadyMapped`),
//! which is the same mechanism that rejects overlap with a previous user mapping.

use core::fmt;

use alloc::vec::Vec;

use crate::arch::riscv64::mmu::{Flags, PTE_A, PTE_D, PTE_R, PTE_U, PTE_V, PTE_W, PTE_X};
use crate::memory::{paging, pmm, FRAME_SIZE};

/// Lowest virtual address the loader is willing to map.
pub const USER_BASE: usize = 0x0001_0000;

/// One past the highest image address; also the user stack's top.
pub const USER_TOP: usize = 0x4000_0000;

/// Bytes of user stack the process starts with.
pub const USER_STACK_SIZE: usize = 64 * 1024;

/// Lowest address of the *initial* user stack.
///
/// The first demand-grown page is mapped directly below it, so this is not a
/// guard page; the guard is [`USER_STACK_FLOOR`] minus one page (see
/// [`crate::user::stack`]).
pub const USER_STACK_BASE: usize = USER_TOP - USER_STACK_SIZE;

/// Frames in the initial user stack.
pub const USER_STACK_PAGES: usize = USER_STACK_SIZE / FRAME_SIZE;

/// Most user stack one process may ever have mapped: 1 MiB.
///
/// The bound is per process and deliberate: a runaway recursion must fail with a
/// clean page fault instead of draining the frame pool, and the runner's PMM
/// ledger must return every frame once the process is gone. Six live processes
/// at the cap hold at most 6 MiB of stack, which the 256 MiB boot configuration
/// can afford (see `Process` and `MAX_PROCESSES`).
pub const USER_STACK_MAX_SIZE: usize = 1 << 20;

/// Lowest address the demand-grown stack may reach.
///
/// The page below this one is never mapped by growth: it is the real guard page
/// that makes "recursion past the cap" a clean fault. The region between the
/// `mmap` window's top (`0x3f00_0000`) and this floor is left unmapped, so a
/// stack fault and a wild `mmap`-window pointer cannot be confused.
pub const USER_STACK_FLOOR: usize = USER_TOP - USER_STACK_MAX_SIZE;

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

/// Why a source read failed.
///
/// Deliberately small and allocation-free: the loader turns it into
/// [`ElfError::Source`] or [`ElfError::Truncated`], and the shell prints the
/// carried string. The VFS adapter maps [`crate::vfs::VfsError`] into
/// [`SourceError::Io`] with that error's own one-line text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    /// The backing store refused the read; the string is its reason.
    Io(&'static str),
    /// The requested range starts or ends past the source's own length.
    OutOfBounds,
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Io(reason) => f.write_str(reason),
            SourceError::OutOfBounds => f.write_str("read past the end of the image"),
        }
    }
}

/// A random-access byte source the loader reads an image from.
///
/// The loader never asks for the whole image: it reads the 64-byte ELF header,
/// the program-header table and then each `PT_LOAD` segment in page-sized
/// chunks. The embedded fixtures use the [`&[u8]`](ImageSource) implementation;
/// `src/user/mod.rs` implements this for a VFS file, so `run <path>` streams
/// from ext2 and never allocates a whole-image buffer.
///
/// Contract: `read_at(offset, buf)` returns `Ok(n)` with `n <= buf.len()`; `0`
/// means end of source; an error means the read failed. A source may return
/// fewer bytes than `buf.len()` — the loader loops — but it must never claim
/// more than it wrote. `len()` is used for the cheap bounds checks up front;
/// every actual read is still checked, and a source that ends early produces
/// [`ElfError::Truncated`], never a panic.
pub trait ImageSource {
    /// Total length of the image in bytes.
    fn len(&self) -> u64;

    /// Read up to `buf.len()` bytes at `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError>;
}

impl ImageSource for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let start = usize::try_from(offset).map_err(|_| SourceError::OutOfBounds)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(SourceError::OutOfBounds)?;
        let source = <[u8]>::get(self, start..end).ok_or(SourceError::OutOfBounds)?;
        buf.copy_from_slice(source);
        Ok(source.len())
    }
}

/// Fill `buf` from `source` at `offset`, looping over short reads.
///
/// A source that ends before `offset + buf.len()` is [`ElfError::Truncated`]; an
/// I/O failure is [`ElfError::Source`]. This is the only read primitive in the
/// loader — there is no path that indexes a whole-image slice.
fn read_exact<S: ImageSource + ?Sized>(
    source: &S,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), ElfError> {
    let mut done = 0usize;
    while done < buf.len() {
        let at = offset.checked_add(done as u64).ok_or(ElfError::Overflow)?;
        let result = source.read_at(at, &mut buf[done..]);
        match result {
            Ok(0) => return Err(ElfError::Truncated),
            Ok(read) if read <= buf.len() - done => done += read,
            // A source that claims more than it was given a buffer for is
            // broken; refuse it instead of trusting the count.
            Ok(_) => return Err(ElfError::Source(SourceError::Io("invalid read length"))),
            Err(SourceError::OutOfBounds) => return Err(ElfError::Truncated),
            Err(error) => return Err(ElfError::Source(error)),
        }
    }
    Ok(())
}

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
    /// The program-header table does not fit in the source.
    ProgramHeadersOutOfBounds,
    /// One `PT_LOAD` header contradicts itself (`filesz > memsz`, bad flags,
    /// bad alignment) or points past the source.
    BadSegment,
    /// The source ended before a range the headers promised (a short file, or a
    /// read that returned 0 mid-segment).
    Truncated,
    /// The source refused a read; the reason is carried in [`SourceError`].
    Source(SourceError),
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
            ElfError::Truncated => "image ended before the data its headers promise",
            ElfError::Source(_) => "reading the image failed",
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
            ElfError::Source(error) => write!(f, "{}: {error}", self.as_str()),
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
/// The page list is a `Vec`, not an inline array: a 256-entry `[usize; 256]`
/// made every `Loaded` value 2 KiB, and a fork that built one on the task's
/// 16 KiB kernel stack (plus the loader's own buffers) overflowed the guard.
/// The allocation is bounded by [`MAX_IMAGE_PAGES`] and is released with the
/// image; `try_reserve` keeps a full heap a refusal, not an abort.
pub struct Loaded {
    /// Root table of the address space the pages live in. `release` walks this
    /// root, not the currently active one: the image may outlive the satp value
    /// it was loaded under (a foreground process torn down by the shell, or the
    /// old image of an `execve`).
    pub root: usize,
    /// User virtual address to start at.
    pub entry: usize,
    pages: Vec<usize>,
}

impl Loaded {
    const fn empty() -> Self {
        Loaded {
            root: 0,
            entry: 0,
            pages: Vec::new(),
        }
    }

    /// A descriptor for an image whose pages were copied into `root` by the
    /// caller (the `fork` path). `pages` are the virtual addresses, in the same
    /// order as the parent's image; no validation is repeated here.
    pub fn from_pages(root: usize, entry: usize, pages: &[usize]) -> Option<Self> {
        if pages.len() > MAX_IMAGE_PAGES {
            return None;
        }
        let mut loaded = Loaded::empty();
        loaded.root = root;
        loaded.entry = entry;
        loaded.pages.try_reserve_exact(pages.len()).ok()?;
        loaded.pages.extend_from_slice(pages);
        Some(loaded)
    }

    /// The user virtual addresses this image owns, in mapping order.
    pub fn pages(&self) -> &[usize] {
        &self.pages
    }

    /// Unmap every page and return its physical frame to the PMM.
    ///
    /// Idempotent: a second call has nothing to release. Errors from `unmap`
    /// (a page that disappeared) are ignored here because there is no better
    /// answer after a process has ended; the ledger check catches a real leak.
    pub fn release(&mut self) {
        for page in self.pages.drain(..) {
            if let Ok(frame) = paging::unmap_in(self.root, page) {
                pmm::free_frame(frame);
            }
        }
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

/// Load the image behind `source` at an optional `bias`.
///
/// `bias` is added to every `p_vaddr` and to `e_entry`; it is how a static PIE
/// (`ET_DYN`) is placed. `ET_EXEC` files must be loaded with `bias == 0`.
///
/// The image is read in bounded pieces through [`ImageSource`]: the header, the
/// program-header table, and each segment copied into its mapped pages in 4 KiB
/// chunks. No whole-image buffer exists, so an image larger than one 64 KiB heap
/// chunk is loaded normally.
///
/// On `Err`, every page mapped by this call has already been unmapped and its
/// frame returned: the PMM ledger is the same as before the call.
pub fn load<S: ImageSource + ?Sized>(source: &S, bias: usize) -> Result<Loaded, ElfError> {
    load_in(paging::active_root(), source, bias)
}

/// [`load`] into an explicit address-space root.
///
/// The process loader uses this so an image is mapped into its own root before
/// that root is installed; the direct selftest uses [`load`] on the kernel root.
pub fn load_in<S: ImageSource + ?Sized>(
    root: usize,
    source: &S,
    bias: usize,
) -> Result<Loaded, ElfError> {
    // ── ELF header ────────────────────────────────────────────────────────
    let source_len = source.len();
    if source_len < EHDR_SIZE as u64 {
        return Err(ElfError::Short);
    }
    let mut ehdr = [0u8; EHDR_SIZE];
    read_exact(source, 0, &mut ehdr)?;
    if ehdr.get(0..4) != Some(b"\x7fELF") {
        return Err(ElfError::BadMagic);
    }
    if ehdr.get(4) != Some(&2) {
        return Err(ElfError::BadClass);
    }
    if ehdr.get(5) != Some(&1) {
        return Err(ElfError::BadData);
    }
    if ehdr.get(6) != Some(&1) {
        return Err(ElfError::BadIdentVersion);
    }

    let e_type = u16_at(&ehdr, 16).ok_or(ElfError::Short)?;
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(ElfError::BadType);
    }
    if u16_at(&ehdr, 18).ok_or(ElfError::Short)? != EM_RISCV {
        return Err(ElfError::BadMachine);
    }
    if u32_at(&ehdr, 20).ok_or(ElfError::Short)? != 1 {
        return Err(ElfError::BadHeader);
    }
    let entry_raw = u64_at(&ehdr, 24).ok_or(ElfError::Short)? as usize;
    let phoff = u64_at(&ehdr, 32).ok_or(ElfError::Short)? as usize;
    let ehsize = u16_at(&ehdr, 52).ok_or(ElfError::Short)? as usize;
    let phentsize = u16_at(&ehdr, 54).ok_or(ElfError::Short)? as usize;
    let phnum = u16_at(&ehdr, 56).ok_or(ElfError::Short)? as usize;
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
    if ph_end as u64 > source_len {
        return Err(ElfError::ProgramHeadersOutOfBounds);
    }
    let mut phdrs = [0u8; MAX_PHDRS * PHDR_SIZE];
    let phdrs_len = phnum * PHDR_SIZE;
    read_exact(source, phoff as u64, &mut phdrs[..phdrs_len])?;

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
    // A 256-entry `PagePlan` array is 4 KiB of kernel stack, and `execve`
    // calls this loader from a 16 KiB kernel stack that already holds the trap
    // frame and the syscall/process frames; the first version overflowed the
    // guard there. Keep the plan list on the heap, like `Loaded`.
    let mut plans: Vec<PagePlan> = Vec::new();
    if plans.try_reserve_exact(MAX_IMAGE_PAGES).is_err() {
        return Err(ElfError::OutOfFrames);
    }

    for index in 0..phnum {
        let base = index * PHDR_SIZE;
        let p_type = u32_at(&phdrs, base).ok_or(ElfError::Short)?;
        if p_type != PT_LOAD {
            continue;
        }
        if segment_count == MAX_SEGMENTS {
            return Err(ElfError::NoLoadableSegment);
        }
        let file_offset = u64_at(&phdrs, base + 8).ok_or(ElfError::Short)? as usize;
        let raw_vaddr = u64_at(&phdrs, base + 16).ok_or(ElfError::Short)? as usize;
        let filesz = u64_at(&phdrs, base + 32).ok_or(ElfError::Short)? as usize;
        let memsz = u64_at(&phdrs, base + 40).ok_or(ElfError::Short)? as usize;
        let flags = u32_at(&phdrs, base + 4).ok_or(ElfError::Short)?;
        let align = u64_at(&phdrs, base + 48).ok_or(ElfError::Short)? as usize;

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
        if file_end as u64 > source_len {
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
            match plans.iter_mut().find(|plan| plan.vaddr == page) {
                Some(plan) => plan.flags |= merged,
                None => {
                    if plans.len() == MAX_IMAGE_PAGES {
                        return Err(ElfError::ImageTooLarge);
                    }
                    plans.push(PagePlan {
                        vaddr: page,
                        flags: merged,
                    });
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
    loaded.root = root;
    loaded.entry = entry;
    if loaded.pages.try_reserve_exact(plans.len()).is_err() {
        return Err(ElfError::OutOfFrames);
    }
    for plan in &plans {
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
        match paging::map_in(root, plan.vaddr, frame, Flags::from_bits(plan.flags)) {
            Ok(()) => loaded.pages.push(plan.vaddr),
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

    // ── stream the file bytes; the rest of every page is already zero ─────
    //
    // One small stack buffer serves every segment: each read is bounded by the
    // page it lands in and by `SEGMENT_CHUNK`, so the loader's own footprint does
    // not grow with the image. 512 bytes (not a whole page) is deliberate: the
    // shell runs on the 128 KiB boot stack, `run_file` is its deepest call chain,
    // and a 4 KiB buffer here left barely any headroom below the stack on a
    // file-backed image. The cost is a handful of extra VFS reads.
    const SEGMENT_CHUNK: usize = 512;
    let mut page_buf = [0u8; SEGMENT_CHUNK];
    for segment in &segments[..segment_count] {
        let mut copied = 0usize;
        while copied < segment.filesz {
            let va = segment.vaddr + copied;
            let page = va & !(FRAME_SIZE - 1);
            let offset_in_page = va - page;
            let available = FRAME_SIZE - offset_in_page;
            let bytes_left = segment.filesz - copied;
            let take = available.min(bytes_left).min(SEGMENT_CHUNK);
            let source_start = segment.file_offset + copied;
            if let Err(error) = read_exact(source, source_start as u64, &mut page_buf[..take]) {
                loaded.release();
                return Err(error);
            }
            let Some(frame) = paging::translate_in(root, page) else {
                loaded.release();
                return Err(ElfError::Map(paging::MapError::NotMapped(page)));
            };
            // SAFETY: `frame` is the physical base of a page this call mapped,
            // and the destination range `offset_in_page..offset_in_page+take`
            // lies inside that 4 KiB frame. `page_buf` is a live stack array
            // disjoint from the PMM frame, and `take <= FRAME_SIZE`; the frame
            // is still mapped supervisor-RW in the identity map, which is how
            // this store reaches it before the user ever runs.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    page_buf.as_ptr(),
                    (frame + offset_in_page) as *mut u8,
                    take,
                )
            };
            copied += take;
        }
    }

    Ok(loaded)
}
