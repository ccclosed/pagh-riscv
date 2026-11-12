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
//!   accounted, `/proc/net/arp` lists only entries the cache still considers
//!   fresh, and a per-pid file exists only for a process that has at least one
//!   live thread. A missing id is `NotFound` (`-ENOENT`), never synthesised.
//! * **Two namespaces.** `/proc/<pid>` is a *process*: the id is `getpid()`, the
//!   identity the userland ABI already hands out, and the files describe the
//!   whole program (its threads included). Kernel threads have no pid, so they
//!   are listed under `/proc/kthreads/<tid>` (a pagh extension, not Linux); that
//!   is what makes `/proc/self` work for the shell too. `/proc/self` is a
//!   symlink to the reader's own entry — `<pid>` for a user thread,
//!   `kthreads/<tid>` for a kernel thread — resolved by the ordinary VFS walk.
//! * **No permissions.** pagh has no users or modes it enforces for reads;
//!   every live entry is readable by any caller, and this file says so instead
//!   of pretending a check happened.
//! * **Mounted from [`crate::vfs::init`], not from the disk path.** `/proc`
//!   must exist when `/mnt` does not — a machine without a disk is exactly the
//!   case where a diagnostic filesystem is worth having.
//! * **Lock order is state-then-node.** [`ProcFile::refresh`] renders *before*
//!   taking the node's own buffer lock, and a renderer takes one subsystem lock
//!   at a time: the PMM bitmap, the heap, the scheduler table, the process
//!   table, the net config, the ARP cache or the mount table. A node lock is
//!   never held across a render, and the process table is never held while the
//!   scheduler lock is taken (process reads copy a value out and release).
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
//! | `tasks` | the whole scheduler table — kernel threads and user threads (a pagh extension) |
//! | `uptime` | seconds since boot (Linux shape; the idle field is always 0.00) |
//! | `version` | kernel name, crate version, machine, build number |
//! | `kthreads/<tid>/{status,stat}` | one kernel thread (name, state, switches, stack) |
//! | `<pid>/{status,stat,cmdline}` | one process: aggregate state of its live threads, the pid/ppid/thread count and the argv snapshot |
//!
//! The per-pid files are a documented native format, not Linux's 52-field
//! `stat`: this kernel has no per-task CPU accounting to report, so the line
//! carries only fields that are real (see [`render_stat`]). `cmdline` *is* the
//! Linux shape — NUL-separated arguments with a trailing NUL — but it is a
//! bounded snapshot taken when the kernel built the entry frame (`Cmdline` in
//! `src/user/mod.rs`), not a live view of the program's stack.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::arch::riscv64::timer;
use crate::memory::FRAME_SIZE;
use crate::net::wire::{format_ipv4, format_mac};
use crate::sync::Spinlock;
use crate::task::scheduler::{self, State, TaskInfo};
use crate::user::process;
use crate::user::{Cmdline, RunName};

use super::{VfsError, VfsNode, VfsResult};

/// The `/proc` subtree root, ready for [`crate::vfs::mount_at`].
pub fn root() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir::new(Dir::Root))
}

// ─── the fixed tree plus its two dynamic namespaces ────────────────────────

/// One directory. `Pid`/`Kthread` carry the identity the path component named;
/// the two dynamic directories exist only while that entry is live.
#[derive(Clone, Copy)]
enum Dir {
    Root,
    Net,
    Kthreads,
    Pid(u64),
    Kthread(u64),
}

/// Which task a per-task file belongs to.
#[derive(Clone, Copy)]
enum Target {
    Process(u64),
    Kthread(u64),
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
    Status(Target),
    Cmdline(Target),
    Stat(Target),
}

/// A fixed child of the root. `self` is listed here so `/proc` has one stable
/// order; its target is computed when it is resolved, not stored.
#[derive(Clone, Copy)]
enum RootChild {
    File(File),
    Dir(Dir),
    SelfLink,
}

/// The root's fixed children, in `ls` order (alphabetical so the listing is
/// stable; the dynamic `/proc/<pid>` entries are appended numerically after
/// them).
const ROOT_CHILDREN: [RootChild; 9] = [
    RootChild::File(File::CpuInfo),
    RootChild::Dir(Dir::Kthreads),
    RootChild::File(File::MemInfo),
    RootChild::File(File::Mounts),
    RootChild::Dir(Dir::Net),
    RootChild::SelfLink,
    RootChild::File(File::Tasks),
    RootChild::File(File::Uptime),
    RootChild::File(File::Version),
];

/// `/proc/net`'s children, in `ls` order.
const NET_CHILDREN: [File; 2] = [File::NetArp, File::NetDev];

fn file_name(file: File) -> &'static str {
    match file {
        File::CpuInfo => "cpuinfo",
        File::MemInfo => "meminfo",
        File::Mounts => "mounts",
        File::NetArp => "arp",
        File::NetDev => "dev",
        File::Tasks => "tasks",
        File::Uptime => "uptime",
        File::Version => "version",
        File::Status(_) => "status",
        File::Cmdline(_) => "cmdline",
        File::Stat(_) => "stat",
    }
}

/// The three files every per-task directory has, in `ls` order.
fn target_files(target: Target) -> [File; 3] {
    [
        File::Cmdline(target),
        File::Stat(target),
        File::Status(target),
    ]
}

fn root_child_name(child: RootChild) -> &'static str {
    match child {
        RootChild::File(file) => file_name(file),
        RootChild::Dir(Dir::Net) => "net",
        RootChild::Dir(Dir::Kthreads) => "kthreads",
        // The root's fixed table names only those two directories; a dynamic
        // namespace is never a `RootChild`.
        RootChild::Dir(_) => "?",
        RootChild::SelfLink => "self",
    }
}

fn root_child_node(child: RootChild) -> Arc<dyn VfsNode> {
    match child {
        RootChild::File(file) => Arc::new(ProcFile::new(file)),
        RootChild::Dir(dir) => Arc::new(ProcDir::new(dir)),
        RootChild::SelfLink => Arc::new(ProcLink),
    }
}

/// Parse a numeric path component strictly: digits only, no sign, fits `u64`.
fn parse_id(name: &str) -> Option<u64> {
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

// ─── directory node ────────────────────────────────────────────────────────

/// A synthetic directory. It owns no children: every `lookup`/`readdir` builds
/// fresh nodes, which is what keeps two readers' snapshots apart.
struct ProcDir {
    kind: Dir,
    /// The entry name this node was reached through (a dynamic directory's name
    /// is the decimal id, so it is owned rather than `&'static str`).
    name: String,
}

impl ProcDir {
    fn new(kind: Dir) -> Self {
        let name = match kind {
            Dir::Root => String::from("proc"),
            Dir::Net => String::from("net"),
            Dir::Kthreads => String::from("kthreads"),
            Dir::Pid(pid) => pid.to_string(),
            Dir::Kthread(tid) => tid.to_string(),
        };
        ProcDir { kind, name }
    }
}

impl VfsNode for ProcDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let mut entries: Vec<Arc<dyn VfsNode>> = Vec::new();
        match self.kind {
            Dir::Root => {
                for child in ROOT_CHILDREN {
                    entries.push(root_child_node(child));
                }
                // Live processes, ascending: the shell can never see one through
                // its own `ls` (a foreground `run` owns the console), but
                // userland listing `/proc` from inside its own process does.
                let mut pids = [0u64; process::MAX_PROCESSES];
                let count = process::live_pids(&mut pids);
                pids[..count].sort_unstable();
                for pid in &pids[..count] {
                    entries.push(Arc::new(ProcDir::new(Dir::Pid(*pid))));
                }
            }
            Dir::Net => {
                for file in NET_CHILDREN {
                    entries.push(Arc::new(ProcFile::new(file)));
                }
            }
            Dir::Kthreads => {
                let mut ids = [0u64; scheduler::MAX_TASKS];
                let count = scheduler::kthread_ids(&mut ids);
                ids[..count].sort_unstable();
                for id in &ids[..count] {
                    entries.push(Arc::new(ProcDir::new(Dir::Kthread(*id))));
                }
            }
            Dir::Pid(pid) => {
                for file in target_files(Target::Process(pid)) {
                    entries.push(Arc::new(ProcFile::new(file)));
                }
            }
            Dir::Kthread(tid) => {
                for file in target_files(Target::Kthread(tid)) {
                    entries.push(Arc::new(ProcFile::new(file)));
                }
            }
        }
        Ok(entries)
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        match self.kind {
            Dir::Root => {
                for child in ROOT_CHILDREN {
                    if root_child_name(child) == name {
                        return Ok(root_child_node(child));
                    }
                }
                // `/proc/<pid>`: only a process with a live thread is a
                // directory. A stale or never-used number is `-ENOENT`.
                if let Some(pid) = parse_id(name) {
                    if process::is_live(pid) {
                        return Ok(Arc::new(ProcDir::new(Dir::Pid(pid))));
                    }
                }
                Err(VfsError::NotFound)
            }
            Dir::Net => {
                for file in NET_CHILDREN {
                    if file_name(file) == name {
                        return Ok(Arc::new(ProcFile::new(file)));
                    }
                }
                Err(VfsError::NotFound)
            }
            Dir::Kthreads => {
                if let Some(id) = parse_id(name) {
                    if scheduler::task_info(id).is_some_and(|info| info.group == 0) {
                        return Ok(Arc::new(ProcDir::new(Dir::Kthread(id))));
                    }
                }
                Err(VfsError::NotFound)
            }
            Dir::Pid(pid) => target_file(Target::Process(pid), name),
            Dir::Kthread(tid) => target_file(Target::Kthread(tid), name),
        }
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

/// Look one name up in a per-task directory.
fn target_file(target: Target, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
    for file in target_files(target) {
        if file_name(file) == name {
            return Ok(Arc::new(ProcFile::new(file)));
        }
    }
    Err(VfsError::NotFound)
}

// ─── `/proc/self` link ─────────────────────────────────────────────────────

/// The one dynamic symbolic link: `/proc/self` points at the reader's own entry.
///
/// The target is computed in [`read_link`](VfsNode::read_link), which runs in
/// the reader's context, so two processes see different targets from the same
/// node. A user thread's process id names `/proc/<pid>`; a kernel thread has no
/// pid, so it points into `/proc/kthreads/<tid>`. The ordinary VFS `walk`
/// expands the link, which is why `cat /proc/self/status` works without any
/// special case.
struct ProcLink;

impl VfsNode for ProcLink {
    fn name(&self) -> &str {
        "self"
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn is_symlink(&self) -> bool {
        true
    }

    fn read_link(&self) -> Option<String> {
        Some(self_target())
    }

    fn size(&self) -> u64 {
        self_target().len() as u64
    }

    fn mode(&self) -> u16 {
        // Symbolic link, 0o777 like the ext2 links.
        0o120777
    }

    fn fs_type(&self) -> &'static str {
        "procfs"
    }

    fn nlink(&self) -> u64 {
        1
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn self_target() -> String {
    let group = scheduler::current_group();
    if group != 0 {
        format!("{group}")
    } else {
        format!("kthreads/{}", scheduler::current_id())
    }
}

// ─── file node ─────────────────────────────────────────────────────────────

/// A synthetic file: the renderer plus the render of the current reading pass.
/// The buffer is `None` until the first `size()`/`read()`.
struct ProcFile {
    file: File,
    /// The current reading pass's text. A read from offset 0 replaces it; a
    /// later offset continues it. `None` means "not rendered yet".
    buf: Spinlock<Option<Vec<u8>>>,
}

impl ProcFile {
    fn new(file: File) -> Self {
        ProcFile {
            file,
            buf: Spinlock::new(None),
        }
    }

    /// Render the file's current text. Called with **no** node lock held: it
    /// reaches into the PMM, heap, scheduler, process, net and mount state, each
    /// of which takes its own lock.
    fn render(&self) -> VfsResult<Vec<u8>> {
        match self.file {
            File::Cmdline(target) => render_cmdline(target),
            File::Stat(target) => {
                let mut out = String::with_capacity(128);
                render_stat(&mut out, target)?;
                Ok(out.into_bytes())
            }
            File::Status(target) => {
                let mut out = String::with_capacity(512);
                render_status(&mut out, target)?;
                Ok(out.into_bytes())
            }
            File::CpuInfo => {
                let mut out = String::with_capacity(256);
                render_cpuinfo(&mut out);
                Ok(out.into_bytes())
            }
            File::MemInfo => {
                let mut out = String::with_capacity(512);
                render_meminfo(&mut out);
                Ok(out.into_bytes())
            }
            File::Mounts => {
                let mut out = String::with_capacity(256);
                render_mounts(&mut out);
                Ok(out.into_bytes())
            }
            File::NetArp => {
                let mut out = String::with_capacity(512);
                render_net_arp(&mut out);
                Ok(out.into_bytes())
            }
            File::NetDev => {
                let mut out = String::with_capacity(256);
                render_net_dev(&mut out);
                Ok(out.into_bytes())
            }
            File::Tasks => {
                let mut out = String::with_capacity(1024);
                render_tasks(&mut out);
                Ok(out.into_bytes())
            }
            File::Uptime => {
                let mut out = String::with_capacity(32);
                render_uptime(&mut out);
                Ok(out.into_bytes())
            }
            File::Version => {
                let mut out = String::with_capacity(64);
                // The real crate version (`Cargo.toml` is the single source),
                // machine tag and the constant build number.
                let _ = writeln!(out, "pagh {} (riscv64) #1", env!("CARGO_PKG_VERSION"));
                Ok(out.into_bytes())
            }
        }
    }

    /// Re-render and publish the new snapshot. An entry that disappeared between
    /// `lookup` and this read is `NotFound` (`-ENOENT`), not an empty file.
    fn refresh(&self) -> VfsResult<()> {
        let rendered = self.render()?;
        *self.buf.lock() = Some(rendered);
        Ok(())
    }

    /// Render only if this node has no snapshot yet (a `size()` before the
    /// first `read`, or a read that started past offset 0).
    fn ensure_rendered(&self) -> VfsResult<()> {
        let missing = self.buf.lock().is_none();
        if missing {
            self.refresh()?;
        }
        Ok(())
    }
}

impl VfsNode for ProcFile {
    fn name(&self) -> &str {
        file_name(self.file)
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if offset == 0 {
            // Start of a reading pass: show current state.
            self.refresh()?;
        } else {
            // A continuation keeps the snapshot it started with, and a missing
            // one (a first read at a nonzero offset) is rendered now.
            self.ensure_rendered()?;
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
        // A gone entry has no length; the following `read` is what reports
        // `-ENOENT`, so `fstat` seeing 0 here is the harmless half of the race.
        if self.ensure_rendered().is_err() {
            return 0;
        }
        self.buf.lock().as_ref().map_or(0, |data| data.len() as u64)
    }

    fn mode(&self) -> u16 {
        // Synthetic file bits: a regular file with the 0o644 default `fstat`
        // would otherwise have to invent.
        0o100644
    }

    fn fs_type(&self) -> &'static str {
        // A file is part of the same filesystem as its directory; `statfs` and
        // `fstatfs` on a proc file must answer `procfs`, not "synthetic".
        "procfs"
    }

    fn nlink(&self) -> u64 {
        1
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── per-task renderers ────────────────────────────────────────────────────

/// Everything a per-pid renderer needs from the process table, copied out in one
/// `peek` so the table lock is never held while formatting.
#[derive(Clone, Copy)]
struct ProcessView {
    name: RunName,
    ppid: u64,
    main_tid: u64,
    satp: usize,
    asid: usize,
    cmdline: Cmdline,
}

/// A live process's view, or `None` for a zombie/missing pid.
fn process_view(pid: u64) -> Option<ProcessView> {
    process::peek(pid, |process| {
        if !process.ready || process.threads == 0 {
            return None;
        }
        Some(ProcessView {
            name: process.name,
            ppid: process.ppid,
            main_tid: process.main_tid,
            satp: process.satp,
            asid: process.asid,
            cmdline: process.cmdline,
        })
    })
    .flatten()
}

/// Aggregate view of one process's live threads.
#[derive(Clone, Copy)]
struct Aggregate {
    state_char: char,
    state_word: &'static str,
    switches: u64,
    work: u64,
    live_threads: usize,
    /// The main thread's scheduler row, when it is still live (it can have
    /// exited while `clone` children keep the process alive).
    main: Option<TaskInfo>,
}

fn aggregate(pid: u64, main_tid: u64) -> Aggregate {
    let mut ids = [0u64; scheduler::MAX_TASKS];
    let count = scheduler::group_threads(pid, &mut ids);
    let mut aggregate = Aggregate {
        state_char: 'Z',
        state_word: "zombie",
        switches: 0,
        work: 0,
        live_threads: 0,
        main: None,
    };
    let mut rank = 0u8;
    for id in &ids[..count] {
        let Some(info) = scheduler::task_info(*id) else {
            continue;
        };
        aggregate.live_threads += 1;
        aggregate.switches = aggregate.switches.saturating_add(info.switches);
        aggregate.work = aggregate.work.saturating_add(info.counter);
        let (state_rank, state_char, state_word) = match info.state {
            State::Running => (4u8, 'R', "running"),
            State::Ready => (3, 'R', "ready"),
            State::Blocked => (2, 'S', "blocked"),
            State::Stopped => (1, 'T', "stopped"),
            State::Exited | State::Free => (0, 'Z', "zombie"),
        };
        if state_rank > rank {
            rank = state_rank;
            aggregate.state_char = state_char;
            aggregate.state_word = state_word;
        }
        if info.id == main_tid {
            aggregate.main = Some(info);
        }
    }
    aggregate
}

fn state_char(state: State) -> char {
    match state {
        State::Running | State::Ready => 'R',
        State::Blocked => 'S',
        State::Stopped => 'T',
        State::Exited | State::Free => 'Z',
    }
}

fn state_word(state: State) -> &'static str {
    match state {
        State::Running => "running",
        State::Ready => "ready",
        State::Blocked => "blocked",
        State::Stopped => "stopped",
        State::Exited | State::Free => "zombie",
    }
}

/// `pid (comm) ...` needs a comm without spaces or parentheses; everything else
/// becomes `_` so one field cannot be mistaken for two.
fn comm_text(name: &str) -> String {
    name.chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_graphic() && character != '(' && character != ')' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// `/proc/<pid>/status` and `/proc/kthreads/<tid>/status`.
///
/// Key/value lines, Linux's spelling where the value is real. The process view
/// aggregates the live threads: `State` is the most "awake" state any thread is
/// in, `Threads` counts them, `Switches`/`Work` are sums, and `Tid` is the main
/// thread's scheduler id (a process identity is `Pid`).
fn render_status(out: &mut String, target: Target) -> VfsResult<()> {
    match target {
        Target::Process(pid) => {
            let Some(view) = process_view(pid) else {
                return Err(VfsError::NotFound);
            };
            let aggregate = aggregate(pid, view.main_tid);
            let _ = writeln!(out, "Name:\t{}", view.name.as_str());
            let _ = writeln!(
                out,
                "State:\t{} ({})",
                aggregate.state_char, aggregate.state_word
            );
            let _ = writeln!(out, "Tid:\t{}", view.main_tid);
            let _ = writeln!(out, "Pid:\t{pid}");
            let _ = writeln!(out, "PPid:\t{}", view.ppid);
            let _ = writeln!(out, "Threads:\t{}", aggregate.live_threads);
            let _ = writeln!(out, "Switches:\t{}", aggregate.switches);
            let _ = writeln!(out, "Work:\t{}", aggregate.work);
            match aggregate.main {
                Some(main) => stack_line(out, &main),
                // The main thread exited while clone children keep the process
                // alive: there is no single kernel stack to report.
                None => {
                    let _ = writeln!(out, "Stack:\t-");
                }
            }
            let _ = writeln!(out, "Satp:\t{:#x} (ASID {})", view.satp, view.asid);
        }
        Target::Kthread(tid) => {
            let Some(info) = scheduler::task_info(tid) else {
                return Err(VfsError::NotFound);
            };
            let _ = writeln!(out, "Name:\t{}", info.name.as_str());
            let _ = writeln!(
                out,
                "State:\t{} ({})",
                state_char(info.state),
                state_word(info.state)
            );
            let _ = writeln!(out, "Tid:\t{tid}");
            let _ = writeln!(out, "Pid:\t0");
            let _ = writeln!(out, "PPid:\t0");
            let _ = writeln!(out, "Threads:\t1");
            let _ = writeln!(out, "Switches:\t{}", info.switches);
            let _ = writeln!(out, "Work:\t{}", info.counter);
            stack_line(out, &info);
            // A kernel thread runs on the kernel map; the field is printed
            // rather than assumed so a future per-task `satp` shows up here.
            let _ = writeln!(out, "Satp:\t{:#x} (kernel)", info.satp);
        }
    }
    Ok(())
}

fn stack_line(out: &mut String, info: &TaskInfo) {
    if info.stack_bytes() == 0 {
        // The boot thread runs on the linker's stack, not on PMM frames.
        let _ = writeln!(out, "Stack:\tboot");
    } else {
        let _ = writeln!(out, "Stack:\t{} KiB", info.stack_bytes() / 1024);
    }
}

/// `/proc/<pid>/stat` and `/proc/kthreads/<tid>/stat`, one line.
///
/// The fields are: `id (comm) state ppid threads switches work stack_kib satp`.
/// This is a documented native subset, not Linux's 52-field record: the kernel
/// keeps no per-task CPU-time accounting, so `utime`/`stime`-style columns would
/// be invented zeros. `id` is the pid for a process and the task id for a kernel
/// thread (which has no pid). `stack_kib` is the main thread's kernel stack, `0`
/// for the boot thread's linker stack.
fn render_stat(out: &mut String, target: Target) -> VfsResult<()> {
    match target {
        Target::Process(pid) => {
            let Some(view) = process_view(pid) else {
                return Err(VfsError::NotFound);
            };
            let aggregate = aggregate(pid, view.main_tid);
            let stack_kib = aggregate.main.map_or(0, |main| main.stack_bytes() / 1024);
            let _ = writeln!(
                out,
                "{} ({}) {} {} {} {} {} {} {:#x}",
                pid,
                comm_text(view.name.as_str()),
                aggregate.state_char,
                view.ppid,
                aggregate.live_threads,
                aggregate.switches,
                aggregate.work,
                stack_kib,
                view.satp
            );
        }
        Target::Kthread(tid) => {
            let Some(info) = scheduler::task_info(tid) else {
                return Err(VfsError::NotFound);
            };
            let _ = writeln!(
                out,
                "{} ({}) {} 0 1 {} {} {} {:#x}",
                tid,
                comm_text(info.name.as_str()),
                state_char(info.state),
                info.switches,
                info.counter,
                info.stack_bytes() / 1024,
                info.satp
            );
        }
    }
    Ok(())
}

/// `/proc/<pid>/cmdline`: the bounded argv snapshot, NUL-separated with a
/// trailing NUL (Linux's shape). A kernel thread has no argv, so its file is
/// empty — the same answer Linux gives for a kernel thread.
fn render_cmdline(target: Target) -> VfsResult<Vec<u8>> {
    match target {
        Target::Process(pid) => {
            let Some(view) = process_view(pid) else {
                return Err(VfsError::NotFound);
            };
            Ok(view.cmdline.as_bytes().to_vec())
        }
        Target::Kthread(_) => Ok(Vec::new()),
    }
}

// ─── machine-wide renderers ────────────────────────────────────────────────
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
    let mut infos = [TaskInfo::zeroed(); scheduler::MAX_TASKS];
    let count = scheduler::snapshot(&mut infos);
    let stats = scheduler::stats();
    let _ = writeln!(
        out,
        "tasks: {}/{} live, {} ready, {} ticks, {} switches, {} reaps, {} clock",
        stats.live_tasks,
        scheduler::MAX_TASKS,
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
