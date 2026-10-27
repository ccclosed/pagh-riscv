//! A minimal flattened-device-tree reader.
//!
//! The device tree is how an RV64 machine says what it is: OpenSBI hands us the
//! blob's physical address and the kernel is expected to read memory size, the
//! timebase frequency, the console's interrupt line and the interrupt controller
//! out of it. This is not a general FDT library — it walks the structure block,
//! tracks the node path, and answers the questions the boot path asks. Everything
//! else (phandles, `interrupt-parent` resolution, `stdout-path`) can be added the
//! day something needs it.
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

/// A big-endian 64-bit field, assembled from two 32-bit reads.
///
/// `bytes` must be at least eight long; callers check the property length first.
fn be_u64(bytes: &[u8]) -> u64 {
    ((be_u32(&bytes[0..4]) as u64) << 32) | be_u32(&bytes[4..8]) as u64
}

/// One `virtio,mmio` transport window, as the device tree declares it.
///
/// The virtio-mmio binding puts the whole register file in one node's `reg`;
/// QEMU `virt` declares eight of them, one per transport slot. `base` is a
/// physical address — the kernel identity-maps RAM, and MMIO is addressed
/// physically — which is what makes it usable as the driver's MMIO base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioMmio {
    /// Register window base (physical address).
    pub base: usize,
    /// Window length in bytes; the virtio-mmio register file is 0x1000.
    pub length: usize,
}

impl VirtioMmio {
    /// A zeroed placeholder, so a caller can build a fixed-size array without
    /// wrapping every slot in an `Option`.
    pub const NONE: Self = VirtioMmio { base: 0, length: 0 };
}

/// Transport windows one tree may report: QEMU `virt` declares exactly eight.
pub const MAX_VIRTIO_MMIO: usize = 8;

/// Deepest node path the [`Fdt::virtio_mmio`] walk tracks the layout of.
///
/// `/soc/virtio_mmio@10001000` is depth 2; the extra room is for a tree that
/// nests its transports one level deeper. Nodes below this are ignored rather
/// than interpreted with a guessed parent layout.
const MAX_VIRTIO_DEPTH: usize = 8;

/// Depth of a node path: the root is 0, `/soc` is 1, `/soc/x@1` is 2.
fn node_depth(path: &str) -> usize {
    if path == "/" {
        0
    } else {
        path.bytes().filter(|byte| *byte == b'/').count()
    }
}

/// Whether a device-tree string list contains `wanted` as a whole entry.
///
/// `compatible` is a list of NUL-terminated strings; a substring match would let
/// `"foo,virtio,mmio"` (or a longer name) answer for the binding it is not.
fn compatible_list_has(value: &[u8], wanted: &str) -> bool {
    value
        .split(|byte| *byte == 0)
        .any(|entry| entry == wanted.as_bytes())
}

/// One structural event from [`Fdt::walk_events`].
///
/// `Enter`/`Leave` bracket a node, so a visitor can keep per-node state (the
/// cell layout its children's `reg` properties are read with) without the walker
/// exposing node offsets. `Leave` arrives before the path is popped, so `path`
/// still names the node that ends.
enum Event<'a> {
    /// A node began.
    Enter(&'a str),
    /// One property of the current node.
    Property(&'a str, &'a str, &'a [u8]),
    /// The current node ends.
    Leave(&'a str),
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
        self.walk_events(|event| match event {
            Event::Property(path, name, value) => visit(path, name, value),
            Event::Enter(_) | Event::Leave(_) => false,
        })
    }

    /// Walk the structure block as a stream of node/property events.
    ///
    /// The tokens, the path bookkeeping and the "a malformed block returns
    /// `None`" policy are the same as [`Self::walk`] describes; the extra
    /// `Enter`/`Leave` events exist for readers that need per-node state — the
    /// `virtio_mmio` enumeration below has to know the *parent's* cell layout
    /// when it meets a child's `reg`.
    fn walk_events(&self, mut visit: impl FnMut(Event<'_>) -> bool) -> Option<()> {
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
                    if visit(Event::Enter(path.as_str())) {
                        return Some(());
                    }
                    pos = (end + 1 + 3) & !3; // names are padded to 4 bytes
                }
                TOKEN_END_NODE => {
                    if visit(Event::Leave(path.as_str())) {
                        return Some(());
                    }
                    path.pop();
                }
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
                    if visit(Event::Property(path.as_str(), name, value)) {
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

    /// The `virtio,mmio` transport windows, in device-tree order.
    ///
    /// Fills `out` and returns how many entries were written. The match is on
    /// `compatible = "virtio,mmio"`, not on the node name — a transport is a
    /// transport wherever a tree puts it — and a node with a matching
    /// `compatible` but no usable `reg` is skipped rather than guessed at.
    ///
    /// `reg` is interpreted with the **parent's** `#address-cells`/`#size-cells`,
    /// which one walk tracks per depth (the defaults are the DT spec's 2/1). Only
    /// the 2+2 layout (`address, size` as two 64-bit cells each) is read: a tree
    /// that declares another layout is skipped, because a misread register window
    /// sends the driver to the wrong MMIO address — the same refusal
    /// [`Self::memory`] makes for the same reason.
    ///
    /// More transports than `out` holds truncate the list to `out.len()`; nodes
    /// deeper than [`MAX_VIRTIO_DEPTH`] are ignored rather than read with a
    /// guessed parent layout (QEMU `virt` declares them at depth 2).
    pub fn virtio_mmio(&self, out: &mut [VirtioMmio]) -> usize {
        let mut count = 0usize;
        // Per depth: the node's own cell layout (for its children's `reg`), the
        // `reg` value of the node at this depth, and whether its `compatible`
        // listed the virtio-mmio binding. Each node resets its slot on Enter, so
        // a node that declares nothing is read with the spec's 2/1 defaults.
        let mut cells = [(2u32, 1u32); MAX_VIRTIO_DEPTH];
        let mut reg = [VirtioMmio::NONE; MAX_VIRTIO_DEPTH];
        let mut is_virtio = [false; MAX_VIRTIO_DEPTH];
        self.walk_events(|event| match event {
            Event::Enter(path) => {
                let depth = node_depth(path);
                if depth < MAX_VIRTIO_DEPTH {
                    cells[depth] = (2, 1);
                    reg[depth] = VirtioMmio::NONE;
                    is_virtio[depth] = false;
                }
                false
            }
            Event::Property(path, name, value) => {
                let depth = node_depth(path);
                if depth >= MAX_VIRTIO_DEPTH {
                    return false;
                }
                match name {
                    "#address-cells" if value.len() >= 4 => {
                        cells[depth].0 = be_u32(&value[0..4]);
                    }
                    "#size-cells" if value.len() >= 4 => {
                        cells[depth].1 = be_u32(&value[0..4]);
                    }
                    "compatible" => {
                        is_virtio[depth] |= compatible_list_has(value, "virtio,mmio");
                    }
                    // A child's `reg` describes it in the *parent's* cells, which
                    // is exactly what the slot one level up holds.
                    "reg" if depth > 0 => {
                        let (address_cells, size_cells) = cells[depth - 1];
                        if address_cells == 2 && size_cells == 2 && value.len() >= 16 {
                            reg[depth] = VirtioMmio {
                                base: be_u64(&value[0..8]) as usize,
                                length: be_u64(&value[8..16]) as usize,
                            };
                        }
                    }
                    _ => {}
                }
                false
            }
            Event::Leave(path) => {
                let depth = node_depth(path);
                if depth < MAX_VIRTIO_DEPTH && is_virtio[depth] && reg[depth].length != 0 {
                    if count == out.len() {
                        return true; // the caller's array is full
                    }
                    out[count] = reg[depth];
                    count += 1;
                }
                false
            }
        });
        count
    }

    /// The PLIC's `(base, size)` as the device tree declares it, or `None`.
    ///
    /// The node is found by its `compatible` (`riscv,plic0` or the newer
    /// `sifive,plic-1.0.0`), not by its name: the unit address that QEMU bakes
    /// into `plic@c000000` is exactly the kind of machine-specific detail this
    /// reader refuses to depend on. Its `reg` must be one 2+2-cell region — the
    /// layout every 64-bit DT declares — and anything else yields `None` rather
    /// than a base address that would point MMIO at unrelated memory.
    pub fn plic(&self) -> Option<(usize, usize)> {
        let mut path = [0u8; NODE_PATH_CAPACITY];
        let len = self.find_compatible(&["riscv,plic0", "sifive,plic-1.0.0"], &mut path)?;
        self.reg_at(&path[..len])
    }

    /// The console UART's `interrupts` cell: the PLIC source number it raises
    /// when a byte arrives (`10` on QEMU `virt`), or `None`.
    ///
    /// The node is found by its `ns16550a` compatible. The property must be
    /// exactly one cell: a multi-cell `interrupts` description uses some other
    /// controller's encoding, and guessing a source number from it would enable
    /// an interrupt line nobody routed. `None` keeps the console polled, which is
    /// the design's honest fallback rather than a failure to boot.
    pub fn uart_interrupt(&self) -> Option<u32> {
        let mut path = [0u8; NODE_PATH_CAPACITY];
        let len = self.find_compatible(&["ns16550a", "ns16550"], &mut path)?;
        self.u32_prop_at(&path[..len], "interrupts")
    }

    /// Find the first node whose `compatible` names one of `wanted`, and copy its
    /// path into `out`. Returns the copied length.
    ///
    /// The two-pass shape — find the node, then read a property of it — exists
    /// because [`Self::walk`] lends the path for the duration of one callback
    /// only. Copying it into a fixed array lets a later pass ask "is this the
    /// node?" without tracking per-node state in the walker, and without an
    /// allocator (the tree is parsed before the boot heap is usable).
    fn find_compatible(
        &self,
        wanted: &[&str],
        out: &mut [u8; NODE_PATH_CAPACITY],
    ) -> Option<usize> {
        let mut found = None;
        self.walk(|path, name, value| {
            if name != "compatible" || !compatible_matches(value, wanted) {
                return false;
            }
            let bytes = path.as_bytes();
            let take = bytes.len().min(NODE_PATH_CAPACITY);
            out[..take].copy_from_slice(&bytes[..take]);
            found = Some(take);
            true
        });
        found
    }

    /// The `reg` of the node at `target`, as `(base, size)`, if it is one 2+2-cell
    /// region. See [`Self::plic`] for why any other layout is refused.
    fn reg_at(&self, target: &[u8]) -> Option<(usize, usize)> {
        let mut result = None;
        self.walk(|path, name, value| {
            if name != "reg" || path.as_bytes() != target || value.len() != 16 {
                return false;
            }
            let base = addr_cells2(&value[0..8]);
            let size = addr_cells2(&value[8..16]);
            result = Some((base, size));
            true
        });
        result
    }

    /// The single-cell `property` of the node at `target`.
    fn u32_prop_at(&self, target: &[u8], property: &str) -> Option<u32> {
        let mut result = None;
        self.walk(|path, name, value| {
            if name != property || path.as_bytes() != target || value.len() != 4 {
                return false;
            }
            result = Some(be_u32(value));
            true
        });
        result
    }
}

/// Longest node path this reader will copy while scanning: `/soc/serial@10000000`
/// is 22 bytes; a deeper `virt` variant still fits with room to spare.
const NODE_PATH_CAPACITY: usize = 96;

/// Read two big-endian cells (one 64-bit `reg` address or size) byte-by-byte.
fn addr_cells2(cells: &[u8]) -> usize {
    ((be_u32(&cells[0..4]) as u64) << 32 | be_u32(&cells[4..8]) as u64) as usize
}

/// Whether a `compatible` property (NUL-separated list of names) contains any
/// name in `wanted`.
fn compatible_matches(value: &[u8], wanted: &[&str]) -> bool {
    value.split(|byte| *byte == 0).any(|entry| {
        core::str::from_utf8(entry)
            .map(|name| wanted.contains(&name))
            .unwrap_or(false)
    })
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
