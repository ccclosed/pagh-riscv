//! A minimal flattened-device-tree reader.
//!
//! The device tree is how an RV64 machine says what it is: OpenSBI hands us the
//! blob's physical address and the kernel is expected to read memory size, the
//! timebase frequency and device addresses out of it. This is not a general FDT
//! library — it walks the structure block, tracks the node path, and answers the
//! three questions the boot path asks. Everything else (phandles, `interrupts`,
//! `stdout-path`) can be added the day something needs it.
//!
//! The blob is big-endian by specification, on a machine that may be either, so
//! every field is read through explicit byte assembly.

use core::str;

const MAGIC: u32 = 0xd00d_feed;
const TOKEN_BEGIN_NODE: u32 = 1;
const TOKEN_END_NODE: u32 = 2;
const TOKEN_PROP: u32 = 3;
const TOKEN_NOP: u32 = 4;
const TOKEN_END: u32 = 9;

/// Why a blob could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdtError {
    /// The magic word at offset 0 is not `0xd00dfeed`.
    BadMagic(u32),
    /// The header advertises a version this reader does not implement.
    UnsupportedVersion(u32),
    /// A block offset/size points outside the blob, or past its own end.
    Truncated,
}

impl core::fmt::Display for FdtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FdtError::BadMagic(m) => write!(f, "bad device tree magic {m:#x}"),
            FdtError::UnsupportedVersion(v) => write!(f, "unsupported device tree version {v}"),
            FdtError::Truncated => write!(f, "device tree is truncated or inconsistent"),
        }
    }
}

/// A parsed header plus the offsets of the three blocks.
pub struct Fdt {
    /// Whole blob, as raw bytes.
    blob: &'static [u8],
    struct_off: usize,
    struct_len: usize,
    strings_off: usize,
    strings_len: usize,
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

impl Fdt {
    /// Parse the header at `address`.
    ///
    /// # Safety-ish
    /// The caller promises `address` is the blob OpenSBI passed in `a1`. It is
    /// read-only for the kernel's lifetime, which is why the returned slice is
    /// `'static`: firmware does not move it.
    pub fn new(address: usize) -> Result<Self, FdtError> {
        // SAFETY: the header is 40 bytes at the firmware-provided address; the
        // fields are read individually below, and every offset is bounds-checked
        // against the advertised total size before any slice is formed.
        let header = unsafe { core::slice::from_raw_parts(address as *const u8, 40) };
        let magic = be_u32(&header[0..4]);
        if magic != MAGIC {
            return Err(FdtError::BadMagic(magic));
        }
        let total = be_u32(&header[4..8]) as usize;
        let struct_off = be_u32(&header[8..12]) as usize;
        let strings_off = be_u32(&header[12..16]) as usize;
        let strings_len = be_u32(&header[32..36]) as usize;
        let version = be_u32(&header[20..24]);
        if version < 16 {
            return Err(FdtError::UnsupportedVersion(version));
        }
        if struct_off >= total || strings_off >= total {
            return Err(FdtError::Truncated);
        }
        // SAFETY: firmware's blob is valid for the whole run and `total` is what
        // it says it is; the offsets were checked against it just above.
        let blob = unsafe { core::slice::from_raw_parts(address as *const u8, total) };
        let struct_len = strings_off - struct_off;
        if strings_off + strings_len > total {
            return Err(FdtError::Truncated);
        }
        Ok(Fdt {
            blob,
            struct_off,
            struct_len,
            strings_off,
            strings_len,
        })
    }

    fn strings(&self) -> &[u8] {
        &self.blob[self.strings_off..self.strings_off + self.strings_len]
    }

    /// Look up a property name in the string block.
    fn property_name(&self, offset: usize) -> Option<&str> {
        let strings = self.strings();
        if offset >= strings.len() {
            return None;
        }
        let end = strings[offset..]
            .iter()
            .position(|b| *b == 0)
            .map(|i| offset + i)?;
        str::from_utf8(&strings[offset..end]).ok()
    }

    /// Walk the structure block, calling `visit(path, name, value)` for every
    /// property and stopping when the visitor returns `true`.
    fn walk(&self, mut visit: impl FnMut(&str, &str, &[u8]) -> bool) -> Option<()> {
        let block = &self.blob[self.struct_off..self.struct_off + self.struct_len];
        let mut path = alloc_path::Path::new();
        let mut pos = 0usize;
        while pos + 4 <= block.len() {
            let token = be_u32(&block[pos..pos + 4]);
            pos += 4;
            match token {
                TOKEN_BEGIN_NODE => {
                    let end = block[pos..].iter().position(|b| *b == 0)? + pos;
                    let name = str::from_utf8(&block[pos..end]).ok()?;
                    path.push(name);
                    pos = (end + 1 + 3) & !3; // names are padded to 4 bytes
                }
                TOKEN_END_NODE => path.pop(),
                TOKEN_PROP => {
                    if pos + 8 > block.len() {
                        return None;
                    }
                    let len = be_u32(&block[pos..pos + 4]) as usize;
                    let name_off = be_u32(&block[pos + 4..pos + 8]) as usize;
                    pos += 8;
                    if pos + len > block.len() {
                        return None;
                    }
                    let value = &block[pos..pos + len];
                    let name = self.property_name(name_off)?;
                    if visit(path.as_str(), name, value) {
                        return Some(());
                    }
                    pos = (pos + len + 3) & !3;
                }
                TOKEN_NOP => {}
                TOKEN_END => return Some(()),
                _ => return None,
            }
        }
        Some(())
    }

    /// The root's `#address-cells`/`#size-cells`, defaulting to the DT spec's 2/1.
    ///
    /// These live on the *parent* node of a `reg` property, and for `/memory` the
    /// parent is the root. Reading them is what lets [`Self::memory`] tell a
    /// layout it understands from one it would misread.
    fn root_cell_counts(&self) -> (u32, u32) {
        let (mut addr, mut size) = (2u32, 1u32);
        self.walk(|path, name, value| {
            if path != "/" && !path.is_empty() {
                return false;
            }
            match name {
                "#address-cells" if value.len() >= 4 => addr = be_u32(&value[0..4]),
                "#size-cells" if value.len() >= 4 => size = be_u32(&value[0..4]),
                _ => {}
            }
            false
        });
        (addr, size)
    }

    /// The first `/memory` node's `reg` as `(base, end)`, if the tree has one.
    ///
    /// `reg` is a sequence of `(address, size)` pairs whose width comes from the
    /// parent's `#address-cells`/`#size-cells` — the root's, for `/memory`. This
    /// reader implements the 2+2 layout that 64-bit machines use (QEMU `virt`
    /// included) and **refuses anything else**: a tree that declares a different
    /// layout is skipped, because a misread memory range corrupts every allocator
    /// decision downstream, and a silently wrong range is worse than none.
    pub fn memory(&self) -> Option<(usize, usize)> {
        let (addr_cells, size_cells) = self.root_cell_counts();
        if addr_cells != 2 || size_cells != 2 {
            return None;
        }
        let mut result = None;
        self.walk(|path, name, value| {
            if !path.starts_with("/memory") || name != "reg" || value.len() < 16 {
                return false;
            }
            let base = ((be_u32(&value[0..4]) as u64) << 32 | be_u32(&value[4..8]) as u64) as usize;
            let size =
                ((be_u32(&value[8..12]) as u64) << 32 | be_u32(&value[12..16]) as u64) as usize;
            result = Some((base, base + size));
            true
        });
        result
    }

    /// `/cpus/timebase-frequency`, the number of `time` ticks per second.
    pub fn timebase_frequency(&self) -> Option<u32> {
        let mut result = None;
        self.walk(|path, name, value| {
            if path == "/cpus" && name == "timebase-frequency" && value.len() >= 4 {
                result = Some(be_u32(&value[0..4]));
                true
            } else {
                false
            }
        });
        result
    }
}

/// A tiny fixed-capacity path builder — the device tree is deep, not wide, and
/// the kernel has no allocator at this point in the boot.
mod alloc_path {
    /// Longest node path this reader tracks (`/soc/serial@10000000` is 22 bytes).
    const CAPACITY: usize = 256;

    pub struct Path {
        buf: [u8; CAPACITY],
        len: usize,
    }

    impl Path {
        pub const fn new() -> Self {
            Path {
                buf: [0; CAPACITY],
                len: 0,
            }
        }

        pub fn push(&mut self, name: &str) {
            if self.len == 1 && self.buf[0] == b'/' {
                // The root node is named "" and the path is already "/".
                if name.is_empty() {
                    return;
                }
                self.len = 0;
            }
            let bytes = name.as_bytes();
            let room = CAPACITY - self.len - 1;
            let take = bytes.len().min(room);
            self.buf[self.len] = b'/';
            self.len += 1;
            self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
            self.buf[self.len] = 0;
        }

        pub fn pop(&mut self) {
            while self.len > 1 {
                self.len -= 1;
                if self.buf[self.len] == b'/' {
                    break;
                }
            }
            self.buf[self.len] = 0;
        }

        pub fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("/")
        }
    }
}
