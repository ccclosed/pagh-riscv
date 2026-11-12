//! A small, read-only `/proc`: live kernel state as files.
//!
//! The tree is fixed and rendered on demand; nothing is snapshotted at mount
//! time. That is the point of the filesystem — `/proc/meminfo` read after a
//! `run` must show the frames that run used, not the numbers boot had.
//!
//! Design rules this module keeps:
//!
//! * **Content is rendered per open, not per path.** `lookup()`/`readdir()`
//!   return fresh nodes, so two descriptors on `/proc/uptime` never share a
//!   snapshot, and a node's buffer only lives as long as the descriptor that
//!   looked it up.
//! * **A reading pass is one snapshot.** A read from offset 0 re-renders (so a
//!   program that samples the file sees current state); the following reads of
//!   the same descriptor keep that render, so a multi-chunk reader never splices
//!   two different uptimes into one file. `size()` is the rendered length so
//!   `stat`/`fstat` (and `lseek(SEEK_END)`) report the same text a read would
//!   return; EOF itself comes from `read` answering 0, not from the size.
//! * **Read-only by construction.** `write`/`truncate`/`create_*`/`remove` keep
//!   the [`VfsNode`] defaults (`NotSupported`); the syscall seam maps that to
//!   `-ENOTSUP`. There is no flag that makes a proc file writable.
//! * **No invented state.** A file reports what the kernel already tracks: the
//!   second field of `/proc/uptime` is `0.00` because idle time is not
//!   accounted, and `/proc/net/arp` lists only entries the cache still
//!   considers fresh. There is no `/proc/<pid>` tree in this slice: unknown
//!   names are `NotFound` (`-ENOENT`), never synthesised.
//! * **Mounted from [`crate::vfs::init`], not from the disk path.** `/proc`
//!   must exist when `/mnt` does not — a machine without a disk is exactly the
//!   case where a diagnostic filesystem is worth having.
//! * **Lock order is state-then-node.** [`ProcFile::refresh`] renders *before*
//!   taking the node's own buffer lock, and the renderers take at most one
//!   subsystem lock at a time (the PMM bitmap, the heap, the scheduler table,
//!   the net config, the ARP cache, the mount table). A node lock is never held
//!   across a render.
//!
//! The files, all under `/proc`:
//!
//! | file | content |
//! |---|---|
//! | `cpuinfo` | hart, timebase, hardware/software clock |
//! | `meminfo` | frame pool, heap and Sv39 counts (Linux key names for the memory rows) |
//! | `mounts` | the mount table, Linux `/proc/mounts` field order |
//! | `net/dev` | the interface, its MAC and the DHCP lease |
//! | `net/arp` | the live ARP cache |
//! | `tasks` | the scheduler table — kernel threads and user threads (a pagh extension) |
//! | `uptime` | seconds since boot (Linux shape; the idle field is always 0.00) |
//! | `version` | kernel name, crate version, machine, build number |

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::arch::riscv64::timer;
use crate::memory::FRAME_SIZE;
use crate::net::wire::{format_ipv4, format_mac};
use crate::sync::Spinlock;

use super::{VfsError, VfsNode, VfsResult};

/// The `/proc` subtree root, ready for [`crate::vfs::mount_at`].
pub fn root() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir::new(Dir::Root))
}

// ─── the fixed tree ────────────────────────────────────────────────────────

/// One directory in the tree.
#[derive(Clone, Copy)]
enum Dir {
    Root,
    Net,
}

/// One file in the tree. The variant is the renderer; nothing about the content
/// lives in the node until a read touches it.
#[derive(Clone, Copy)]
enum File {
    CpuInfo,
    MemInfo,
    Mounts,
    NetArp,
    NetDev,
    Tasks,
    Uptime,
    Version,
}

/// A child of a directory: either a file or another directory.
#[derive(Clone, Copy)]
enum Child {
    File(File),
    Dir(Dir),
}

/// The root's children, in `ls` order (alphabetical, so the listing is stable).
const ROOT_CHILDREN: [(&str, Child); 7] = [
    ("cpuinfo", Child::File(File::CpuInfo)),
    ("meminfo", Child::File(File::MemInfo)),
    ("mounts", Child::File(File::Mounts)),
    ("net", Child::Dir(Dir::Net)),
    ("tasks", Child::File(File::Tasks)),
    ("uptime", Child::File(File::Uptime)),
    ("version", Child::File(File::Version)),
];

/// `/proc/net`'s children, in `ls` order.
const NET_CHILDREN: [(&str, Child); 2] = [
    ("arp", Child::File(File::NetArp)),
    ("dev", Child::File(File::NetDev)),
];

fn children(kind: Dir) -> &'static [(&'static str, Child)] {
    match kind {
        Dir::Root => &ROOT_CHILDREN,
        Dir::Net => &NET_CHILDREN,
    }
}

// ─── directory node ────────────────────────────────────────────────────────

/// A synthetic directory. It owns no children: every `lookup`/`readdir` builds
/// fresh file nodes, which is what keeps two readers' snapshots apart.
struct ProcDir {
    kind: Dir,
}

impl ProcDir {
    const fn new(kind: Dir) -> Self {
        ProcDir { kind }
    }

    fn child_node(name: &'static str, child: Child) -> Arc<dyn VfsNode> {
        match child {
            Child::File(file) => Arc::new(ProcFile::new(name, file)),
            Child::Dir(dir) => Arc::new(ProcDir::new(dir)),
        }
    }
}

impl VfsNode for ProcDir {
    fn name(&self) -> &str {
        match self.kind {
            Dir::Root => "proc",
            Dir::Net => "net",
        }
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let mut entries = Vec::with_capacity(children(self.kind).len());
        for (name, child) in children(self.kind) {
            entries.push(ProcDir::child_node(name, *child));
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        // The table is per directory, not global: `arp` only exists under
        // `/proc/net`, and asking the root for it is a miss, not a hit.
        for (child_name, child) in children(self.kind) {
            if *child_name == name {
                return Ok(ProcDir::child_node(child_name, *child));
            }
        }
        Err(VfsError::NotFound)
    }

    fn mode(&self) -> u16 {
        // Synthetic directory bits: `fstat` reports the same 0o755 it gives the
        // root, and the shell's `stat` prints the low permission bits.
        0o40555
    }

    fn fs_type(&self) -> &'static str {
        "procfs"
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── file node ─────────────────────────────────────────────────────────────

/// A synthetic file: its name, its renderer, and the render of the current
/// reading pass. The buffer is `None` until the first `size()`/`read()`.
struct ProcFile {
    name: &'static str,
    file: File,
    /// The current reading pass's text. A read from offset 0 replaces it; a
    /// later offset continues it. `None` means "not rendered yet".
    buf: Spinlock<Option<Vec<u8>>>,
}

impl ProcFile {
    fn new(name: &'static str, file: File) -> Self {
        ProcFile {
            name,
            file,
            buf: Spinlock::new(None),
        }
    }

    /// Render the file's current text. Called with **no** node lock held: it
    /// reaches into the PMM, heap, scheduler, net and mount table, each of which
    /// takes its own lock.
    fn render(&self) -> Vec<u8> {
        let mut out = String::with_capacity(512);
        match self.file {
            File::CpuInfo => render_cpuinfo(&mut out),
            File::MemInfo => render_meminfo(&mut out),
            File::Mounts => render_mounts(&mut out),
            File::NetArp => render_net_arp(&mut out),
            File::NetDev => render_net_dev(&mut out),
            File::Tasks => render_tasks(&mut out),
            File::Uptime => render_uptime(&mut out),
            File::Version => {
                // The real crate version (`Cargo.toml` is the single source),
                // machine tag and the constant build number.
                let _ = writeln!(out, "pagh {} (riscv64) #1", env!("CARGO_PKG_VERSION"));
            }
        }
        out.into_bytes()
    }

    /// Re-render and publish the new snapshot.
    fn refresh(&self) {
        let rendered = self.render();
        *self.buf.lock() = Some(rendered);
    }

    /// Render only if this node has no snapshot yet (a `size()` before the
    /// first `read`, or a read that started past offset 0).
    fn ensure_rendered(&self) {
        let missing = self.buf.lock().is_none();
        if missing {
            self.refresh();
        }
    }
}

impl VfsNode for ProcFile {
    fn name(&self) -> &str {
        self.name
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if offset == 0 {
            // Start of a reading pass: show current state.
            self.refresh();
        } else {
            // A continuation keeps the snapshot it started with.
            self.ensure_rendered();
        }
        let guard = self.buf.lock();
        let Some(data) = guard.as_ref() else {
            return Ok(0);
        };
        let start = offset as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let count = core::cmp::min(buf.len(), data.len() - start);
        buf[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }

    fn size(&self) -> u64 {
        self.ensure_rendered();
        self.buf.lock().as_ref().map_or(0, |data| data.len() as u64)
    }

    fn mode(&self) -> u16 {
        // Synthetic file bits: a regular file with the 0o644 default `fstat`
        // would otherwise have to invent.
        0o100644
    }

    fn nlink(&self) -> u64 {
        1
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── renderers ─────────────────────────────────────────────────────────────
//
// Each renderer is a pure function of kernel state into text: no printing, no
// locks of its own beyond what the state accessors take, and no allocation
// larger than the line it formats. The format description lives next to each
// one because the text *is* the contract (docs and scripts read it).

/// `/proc/cpuinfo`: the one hart, the timebase and which clock drives the tick.
fn render_cpuinfo(out: &mut String) {
    let _ = writeln!(out, "processor\t: 0");
    let _ = writeln!(out, "hart\t\t: {}", crate::arch::riscv64::hart_id());
    let _ = writeln!(out, "timebase\t: {} Hz", timer::timebase_hz());
    let _ = writeln!(
        out,
        "clock\t\t: {}",
        if timer::running() {
            "hardware timer"
        } else {
            "software (no timer interrupt)"
        }
    );
}

/// `/proc/meminfo`: Linux's `Mem*` rows for the frame pool, then the heap and
/// Sv39 counters this kernel actually tracks.
///
/// `MemTotal` is *every frame the PMM tracks* (the whole device-tree RAM range,
/// including the frames the kernel reserved for itself and the boot stack), not
/// Linux's "RAM usable after firmware"; `MemFree` is what the allocator can hand
/// out right now, so `MemTotal - MemFree` is the kernel's own footprint plus the
/// reservations. `MemAvailable` equals `MemFree`: there is no page cache (no
/// file-backed paging, no reclaim), so a byte that is free is available, and
/// claiming otherwise would invent a cache to drop.
fn render_meminfo(out: &mut String) {
    let (free_frames, total_frames) = crate::memory::stats();
    let frame_kb = (FRAME_SIZE / 1024) as u64;
    let total_kb = total_frames as u64 * frame_kb;
    let free_kb = free_frames as u64 * frame_kb;
    let _ = writeln!(out, "MemTotal:        {total_kb:>8} kB");
    let _ = writeln!(out, "MemFree:         {free_kb:>8} kB");
    let _ = writeln!(out, "MemAvailable:    {free_kb:>8} kB");
    let (heap_used, heap_mapped, chunks) = crate::heap_stats();
    let _ = writeln!(out, "HeapUsed:        {heap_used:>8} B");
    let _ = writeln!(out, "HeapMapped:      {heap_mapped:>8} B");
    let _ = writeln!(out, "HeapChunks:      {chunks:>8}");
    match crate::memory::paging::snapshot() {
        Some(space) => {
            let _ = writeln!(out, "Sv39Pages:       {:>8}", space.mappings);
            let _ = writeln!(out, "Sv39DevicePages: {:>8}", space.device_pages);
        }
        None => {
            let _ = writeln!(out, "Sv39Pages:              -");
            let _ = writeln!(out, "Sv39DevicePages:        -");
        }
    }
}

/// `/proc/mounts`: Linux field order `device mount_point fstype options dump
/// pass`. There are no separate block devices, so the device field is the mount
/// point itself (the x86 tree's `/proc/mounts` makes the same choice); nothing
/// is mounted read-only and there is no dump/pass pass, hence `rw 0 0`.
fn render_mounts(out: &mut String) {
    for (name, fs_type) in crate::vfs::mount_snapshot() {
        let point = format!("/{name}");
        let _ = writeln!(out, "{point} {point} {fs_type} rw 0 0");
    }
}

/// `/proc/net/dev`: the interface this kernel owns, its MAC and the DHCP lease.
/// A machine without a NIC is a valid configuration and says so.
fn render_net_dev(out: &mut String) {
    let Some(mac) = crate::net::mac() else {
        out.push_str("virtio-net0: no interface\n");
        return;
    };
    let mac = mac_text(mac);
    match crate::net::status() {
        Some(status) => {
            let _ = writeln!(
                out,
                "virtio-net0: mac {mac} ip {} mask {} gateway {} dns {} server {} lease {} s",
                ipv4_text(status.config.ip),
                ipv4_text(status.config.mask),
                ipv4_text(status.config.gateway),
                ipv4_text(status.config.dns),
                ipv4_text(status.config.server),
                status.config.lease_seconds
            );
        }
        None => {
            let _ = writeln!(out, "virtio-net0: mac {mac} (no DHCP lease)");
        }
    }
}

/// `/proc/net/arp`: the live ARP cache, one entry per line. Only entries the
/// cache still considers fresh are listed; a stale entry is not a mapping a
/// reader could use, and printing it would be the kind of invented state this
/// filesystem refuses elsewhere. The format is this kernel's (no Linux `HW
/// type`/`Flags` columns), documented in `src/vfs/README.md`.
fn render_net_arp(out: &mut String) {
    if crate::net::mac().is_none() {
        out.push_str("virtio-net0: no interface\n");
        return;
    }
    out.push_str("IP address       HW address        Device\n");
    let mut entries = [([0u8; 4], [0u8; 6]); crate::net::ARP_ENTRIES];
    let count = crate::net::arp_snapshot(&mut entries);
    for (ip, mac) in &entries[..count] {
        let _ = writeln!(
            out,
            "{:<16} {:<17} virtio-net0",
            ipv4_text(*ip),
            mac_text(*mac)
        );
    }
}

/// `/proc/tasks`: the scheduler table (the shell's `tasks` command renders the
/// same snapshot). A pagh extension, not a Linux file: it lists kernel threads
/// and user threads together, which is what the one table holds.
fn render_tasks(out: &mut String) {
    let mut infos = [crate::task::TaskInfo::zeroed(); crate::task::MAX_TASKS];
    let count = crate::task::snapshot(&mut infos);
    let stats = crate::task::stats();
    let _ = writeln!(
        out,
        "tasks: {}/{} live, {} ready, {} ticks, {} switches, {} reaps, {} clock",
        stats.live_tasks,
        crate::task::MAX_TASKS,
        stats.ready_tasks,
        stats.ticks,
        stats.switches,
        stats.reaps,
        if stats.hardware_clock {
            "timer"
        } else {
            "software"
        }
    );
    let _ = writeln!(out, "  id  name       state     switches   work  stack");
    for info in &infos[..count] {
        if info.stack_bytes() == 0 {
            // The boot thread runs on the linker's stack, not on PMM frames.
            let _ = writeln!(
                out,
                "  {:<3} {:<10} {:<8} {:>8}  {:>4}  boot",
                info.id,
                info.name.as_str(),
                info.state.name(),
                info.switches,
                info.counter
            );
        } else {
            let _ = writeln!(
                out,
                "  {:<3} {:<10} {:<8} {:>8}  {:>4}  {} KiB",
                info.id,
                info.name.as_str(),
                info.state.name(),
                info.switches,
                info.counter,
                info.stack_bytes() / 1024
            );
        }
    }
}

/// `/proc/uptime`: `"<sec>.<centi> 0.00"`, Linux's shape. The idle field is
/// `0.00` on purpose: this kernel does not account idle time, and a fabricated
/// number would be worse than the truth (the shell's `time` command exposes the
/// raw tick and interrupt counters for anyone who needs them).
fn render_uptime(out: &mut String) {
    let hz = timer::TICK_HZ as u64;
    let ticks = timer::ticks();
    let _ = writeln!(out, "{}.{:02} 0.00", ticks / hz, (ticks % hz) * 100 / hz);
}

// ─── small formatters ──────────────────────────────────────────────────────
//
// `net::wire` formats into caller-owned byte buffers (it is the stack's
// allocation-free formatter); these two wrap it for a `String` sink.

fn ipv4_text(ip: [u8; 4]) -> String {
    let mut buffer = [0u8; 15];
    match format_ipv4(ip, &mut buffer) {
        Some(length) => String::from_utf8_lossy(&buffer[..length]).into_owned(),
        // The wire formatter only refuses a 15-byte buffer, so this is
        // unreachable for the sizes above; the fallback exists so a file can
        // never panic on a formatting edge.
        None => String::from("?"),
    }
}

fn mac_text(mac: [u8; 6]) -> String {
    let mut buffer = [0u8; 17];
    let length = format_mac(mac, &mut buffer);
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}
