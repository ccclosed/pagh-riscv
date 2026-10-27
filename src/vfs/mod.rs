//! The virtual filesystem: the node trait every mounted filesystem implements,
//! the mount table, and path resolution (including symbolic links).
//!
//! Design rules that the rest of the kernel relies on:
//!
//! * **One trait, no filesystem-specific types.** [`VfsNode`] is what the shell
//!   and (later) the syscall layer program against; the ext2 implementation lives
//!   in [`crate::fs::ext2`] and is reached only through this trait. Mutating
//!   operations default to `NotSupported`, so a read-only node (a future `/proc`,
//!   `/dev`) needs no code.
//! * **Paths are UTF-8 `String`s.** The shell reads lines of text and the kernel
//!   has no `OsStr`; a name that is not UTF-8 in a foreign image is displayed
//!   lossily by ext2 rather than being unrepresentable.
//! * **The tree is mounted at `/mnt`, inside the synthetic root.** The root
//!   directory is a kernel-owned `VfsNode` with no backing store; [`mount_at`]
//!   splices a filesystem's root node into it under one name. Resolution is
//!   relative to the synthetic root, so `/mnt/x` walks the mount and `/` answers
//!   with the (usually empty) root directory.
//! * **Two resolution primitives, on purpose.** [`lookup_path`] returns the node
//!   a path names *without* following a final symbolic link (`lstat`, `readlink`,
//!   `rm`); [`walk`] follows links with a loop budget (`cat`, `cd`, `write`).
//!   The split exists because collapsing them is exactly how `rm link` starts
//!   deleting the link's target instead of the link.
//! * **`..` never escapes a mount.** `/mnt/..` resolves to the mounted root, not
//!   to `/`, because the mount wrapper forwards to the filesystem's own root and
//!   a mounted ext2 root's `..` points at itself. This is the documented v1
//!   simplification (the x86 tree behaves the same way); it is why cleanup code
//!   must not rely on `..` to leave a mount.
//!
//! Everything here is `core` + `alloc` and prints nothing; the in-QEMU suite
//! (`src/fs/selftest.rs`) drives it over a RAM block device, and a real disk is
//! checked by the boot path and the shell commands (see `src/vfs/README.md`).

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::Spinlock;

/// Path resolution result.
pub type VfsResult<T> = Result<T, VfsError>;

/// Error model shared by every node and every caller.
///
/// The variants are deliberately POSIX-flavoured but *not* errno values: the
/// kernel has no userland yet (M5), and the shell prints these names. When the
/// syscall layer arrives, the mapping to Linux errnos is one match arm in that
/// layer, not a change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    /// A path component or directory entry does not exist.
    NotFound,
    /// The node type does not implement the operation (e.g. `read` on a link).
    NotSupported,
    /// A malformed request: empty name, `..` used as a file name, a path that
    /// does not start with `/` where one must, ...
    InvalidArgument,
    /// The device or the filesystem underneath reported a failure.
    IoError,
    /// A directory entry with that name is already there.
    AlreadyExists,
    /// `rmdir`/`rename` over a directory that still has entries.
    NotEmpty,
    /// The path names a directory where a file was required.
    IsADirectory,
    /// A component of the path is not a directory.
    NotADirectory,
    /// The filesystem has no free block or inode left.
    NoSpace,
    /// An on-disk structure failed validation (bad magic, broken `rec_len`
    /// chain, an inode number past the table, ...).
    Corrupt,
    /// A directory entry name exceeded ext2's 255-byte limit.
    NameTooLong,
}

impl VfsError {
    /// One-line, allocation-free description for shell diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            VfsError::NotFound => "not found",
            VfsError::NotSupported => "operation not supported",
            VfsError::InvalidArgument => "invalid argument",
            VfsError::IoError => "i/o error",
            VfsError::AlreadyExists => "already exists",
            VfsError::NotEmpty => "directory not empty",
            VfsError::IsADirectory => "is a directory",
            VfsError::NotADirectory => "not a directory",
            VfsError::NoSpace => "no space left",
            VfsError::Corrupt => "filesystem is corrupt",
            VfsError::NameTooLong => "name too long",
        }
    }
}

/// Filesystem capacity snapshot for a future `statfs`: block counts are in units
/// of `block_size`. Filesystems that track usage (ext2) report it; synthetic
/// nodes keep the [`VfsNode::fs_stat`] default of `None`.
#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    pub block_size: u64,
    pub blocks_total: u64,
    pub blocks_free: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
}

/// A node in the filesystem tree. Implemented by ext2's directory/file/link
/// nodes and by the synthetic root and mount wrapper.
pub trait VfsNode: Send + Sync {
    /// The node's own name (not a path). Directories report the entry they were
    /// reached through; the mounted filesystem's root reports `"/"` internally
    /// and is presented under the mount name by [`MountNode`].
    fn name(&self) -> &str;

    fn is_directory(&self) -> bool;

    /// Capacity of the filesystem behind this node, when it tracks one.
    fn fs_stat(&self) -> Option<FsStat> {
        None
    }

    /// Read up to `buf.len()` bytes at `offset`. Returns the byte count read;
    /// 0 means "at or past end of file", never an error.
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    /// Write `buf` at `offset`, growing the file as needed. Returns bytes written.
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    /// Resize a regular file. Implementations must zero newly exposed bytes.
    fn truncate(&self, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Err(VfsError::NotSupported)
    }

    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotFound)
    }

    /// Size in bytes (`i_size` for ext2; the target length for a symlink).
    fn size(&self) -> u64 {
        0
    }

    /// Filesystem-unique inode number, or 0 for synthetic nodes.
    fn fs_ino(&self) -> u64 {
        0
    }

    /// Is this node a symbolic link? [`walk`] follows only nodes for which this
    /// is `true`; a link node's `read`/`write` are `NotSupported` on purpose.
    fn is_symlink(&self) -> bool {
        false
    }

    /// The target of a symbolic link, when [`is_symlink`](Self::is_symlink).
    fn read_link(&self) -> Option<String> {
        None
    }

    /// Number of hard links (`st_nlink`).
    fn nlink(&self) -> u64 {
        1
    }

    /// Raw ext2 `i_mode` bits (type + permissions), or 0 for synthetic nodes.
    fn mode(&self) -> u16 {
        0
    }

    // ── mutating directory operations ──
    //
    // They live on the *directory* node and default to `NotSupported`, so the
    // ext2 directory is the only implementation in M5.

    /// Create a child directory named `name`.
    fn create_dir(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    /// Create an empty child regular file named `name`.
    fn create_file(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    /// Create a symbolic link named `name` whose target is `target` (stored
    /// verbatim, never resolved at creation time).
    fn create_symlink(&self, _name: &str, _target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    /// Create the hard link `name` to the existing inode behind `target`.
    fn link(&self, _name: &str, _target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    /// Remove the child entry `name` (a file, or an empty directory).
    fn remove(&self, _name: &str) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// Rename the entry `old_name` of this directory to `new_name` inside
    /// `new_parent`. Both directories must belong to the same filesystem; the
    /// default is `NotSupported` (a foreign node cannot be renamed through here).
    fn rename(
        &self,
        _old_name: &str,
        _new_parent: &Arc<dyn VfsNode>,
        _new_name: &str,
    ) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// Flush buffered state. The ext2 writer is synchronous, so this is a no-op.
    fn sync(&self) {}

    /// Downcast hook used by [`rename`], where the destination parent has to be
    /// recognised as a directory of the same filesystem.
    ///
    /// Every concrete node implements this as `self`. The mount wrapper forwards
    /// it to the inner filesystem node, so a destination reached through the
    /// mount point is still recognised.
    fn as_any(&self) -> &dyn core::any::Any;
}

// ─── synthetic root ────────────────────────────────────────────────────────

/// The kernel-owned root of the tree: a directory whose children are mounts.
struct RootDirectory {
    children: Spinlock<Vec<Arc<dyn VfsNode>>>,
}

impl VfsNode for RootDirectory {
    fn name(&self) -> &str {
        "/"
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.lock().clone())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        for child in self.children.lock().iter() {
            if child.name() == name {
                return Ok(Arc::clone(child));
            }
        }
        Err(VfsError::NotFound)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A mounted filesystem, presented under its mount-point name.
///
/// The inner node is the filesystem's root (which calls itself `"/"`); this
/// wrapper gives it the mount name and forwards every operation. `lookup`,
/// `readdir` and the mutating methods are forwarded too, so path resolution
/// below the mount point never sees the wrapper again.
struct MountNode {
    name: String,
    inner: Arc<dyn VfsNode>,
}

impl VfsNode for MountNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        self.inner.is_directory()
    }

    fn fs_stat(&self) -> Option<FsStat> {
        self.inner.fs_stat()
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read(offset, buf)
    }

    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.inner.write(offset, buf)
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.inner.truncate(size)
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        self.inner.readdir()
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.lookup(name)
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn fs_ino(&self) -> u64 {
        self.inner.fs_ino()
    }

    fn is_symlink(&self) -> bool {
        self.inner.is_symlink()
    }

    fn read_link(&self) -> Option<String> {
        self.inner.read_link()
    }

    fn nlink(&self) -> u64 {
        self.inner.nlink()
    }

    fn mode(&self) -> u16 {
        self.inner.mode()
    }

    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_dir(name)
    }

    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_file(name)
    }

    fn create_symlink(&self, name: &str, target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_symlink(name, target)
    }

    fn link(&self, name: &str, target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.link(name, target)
    }

    fn remove(&self, name: &str) -> VfsResult<()> {
        self.inner.remove(name)
    }

    fn rename(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> VfsResult<()> {
        self.inner.rename(old_name, new_parent, new_name)
    }

    fn sync(&self) {
        self.inner.sync()
    }

    /// Forwarded so a rename *into* the mount point (destination parent `/mnt`)
    /// resolves to the filesystem's own directory node for the downcast in
    /// `ext2::Ext2Dir::rename`.
    fn as_any(&self) -> &dyn core::any::Any {
        self.inner.as_any()
    }
}

// ─── mount table ───────────────────────────────────────────────────────────

static VFS_ROOT: Spinlock<Option<Arc<dyn VfsNode>>> = Spinlock::new(None);
/// Typed handle to the same node as [`VFS_ROOT`], so [`mount_at`] can splice into
/// the root's children without downcasting a `dyn VfsNode`.
static ROOT_DIR: Spinlock<Option<Arc<RootDirectory>>> = Spinlock::new(None);

/// Build the synthetic root. Idempotent: a second call keeps the existing tree
/// (a mount that is already attached must not be dropped by re-initialisation).
pub fn init() {
    if VFS_ROOT.lock().is_some() {
        return;
    }
    let root = Arc::new(RootDirectory {
        children: Spinlock::new(Vec::new()),
    });
    *ROOT_DIR.lock() = Some(Arc::clone(&root));
    *VFS_ROOT.lock() = Some(root as Arc<dyn VfsNode>);
}

/// The synthetic root node, when [`init`] has run.
pub fn root_node() -> VfsResult<Arc<dyn VfsNode>> {
    VFS_ROOT.lock().clone().ok_or(VfsError::NotFound)
}

/// Attach `node` at an absolute mount point.
///
/// Only **single-component** mount points are supported (`"/mnt"`, not
/// `"/a/b"`): the node is spliced into the synthetic root under that name,
/// wrapped so it presents the mount name. Re-mounting the same name replaces the
/// previous entry (that is how the `ramdisk` test command re-mounts a fresh RAM
/// filesystem over `/mnt`).
pub fn mount_at(path: &str, node: Arc<dyn VfsNode>) -> VfsResult<()> {
    let name = match path.strip_prefix('/') {
        Some(rest) => rest.trim_end_matches('/'),
        None => return Err(VfsError::InvalidArgument),
    };
    if name.is_empty() || name.contains('/') {
        return Err(VfsError::InvalidArgument);
    }
    let root = ROOT_DIR
        .lock()
        .clone()
        .ok_or_else(|| VfsError::InvalidArgument)?;
    let mount = Arc::new(MountNode {
        name: name.to_string(),
        inner: node,
    }) as Arc<dyn VfsNode>;
    let mut children = root.children.lock();
    children.retain(|c| c.name() != name);
    children.push(mount);
    Ok(())
}

/// Detach the mount at absolute path `path`, if present. Used by the `ramdisk`
/// test command before replacing the device underneath `/mnt`.
pub fn unmount_at(path: &str) -> VfsResult<()> {
    let name = path.strip_prefix('/').ok_or(VfsError::InvalidArgument)?;
    let name = name.trim_end_matches('/');
    let root = ROOT_DIR
        .lock()
        .clone()
        .ok_or_else(|| VfsError::InvalidArgument)?;
    root.children.lock().retain(|c| c.name() != name);
    Ok(())
}

// ─── path resolution ───────────────────────────────────────────────────────

/// Symbolic-link hops allowed in one resolution (Linux's `SYMLOOP_MAX` is 40;
/// eight is enough for M5 and keeps a self-referential link from spinning).
const SYMLOOP_MAX: usize = 8;

/// Join `path` onto the shell's current working directory. Lexical only: the
/// result is an absolute path string, not a canonical one, so `.`/`..` corners
/// stay visible to [`walk`] (which resolves them against the tree, links
/// included).
pub fn resolve(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let mut out = String::with_capacity(cwd.len() + 1 + path.len());
    if cwd.is_empty() {
        out.push('/');
    } else {
        out.push_str(cwd);
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(path);
    out
}

/// Lexically normalise an absolute path: drop `.` and empty components, and fold
/// `..` against the preceding component.
///
/// Unlike [`resolve`] this is meant for *display* and for keeping the shell's
/// cwd honest (`cd /mnt/demo; cd ..` must leave `pwd` at `/mnt`, not at
/// `/mnt/demo/..`). It is still lexical: it does not touch the filesystem, so it
/// cannot see symlinks — callers resolve the path through [`walk`] first and use
/// this only for the name to report.
///
/// `..` never leaves a mount: the mounted root's parent is itself (the
/// documented v1 simplification), so when the normalised prefix is a mount
/// point, `..` keeps it.
pub fn normalize(path: &str) -> String {
    if !path.starts_with('/') {
        return path.to_string();
    }
    // One snapshot of the mount names; the shell is the only caller and this is
    // not a hot path.
    let mounts: Vec<String> = match ROOT_DIR.lock().clone() {
        Some(root) => root
            .children
            .lock()
            .iter()
            .map(|c| c.name().to_string())
            .collect(),
        None => Vec::new(),
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if comp == ".." {
            if parts.is_empty() {
                continue; // `/..` is `/`
            }
            let mut current = String::from("/");
            current.push_str(&parts.join("/"));
            if mounts
                .iter()
                .any(|m| current.strip_prefix('/') == Some(m.as_str()))
            {
                continue; // do not climb out of a mount
            }
            parts.pop();
        } else {
            parts.push(comp);
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Split an absolute path into `(parent, leaf)` for the create/remove/rename
/// family. The leaf must name a real entry — `""`, `"."` and `".."` are refused
/// because none of them can be created or removed.
///
/// The parent part is returned as a string so the caller can run it through
/// [`walk`]; a path with no `/` after the root (`/x`) yields `("/", "x")`.
pub fn parent_and_leaf(path: &str) -> VfsResult<(String, String)> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
    let trimmed = path.trim_end_matches('/');
    let Some(idx) = trimmed.rfind('/') else {
        return Err(VfsError::InvalidArgument);
    };
    let leaf = &trimmed[idx + 1..];
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return Err(VfsError::InvalidArgument);
    }
    let parent = if idx == 0 { "/" } else { &trimmed[..idx] };
    Ok((parent.to_string(), leaf.to_string()))
}

/// Split an absolute path into components (skipping empty ones), reversed so the
/// walker can `pop` the next component off the end.
fn components(path: &str) -> Vec<String> {
    let mut out: Vec<String> = path
        .split('/')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect();
    out.reverse();
    out
}

/// Resolve an absolute path *without* following a final symbolic link.
///
/// This is the `lstat`/`unlink`/`readlink` primitive: the returned node is the
/// link itself when the last component names one. Intermediate links are still
/// followed — a path cannot be walked through a link that is not expanded.
pub fn lookup_path(path: &str) -> VfsResult<Arc<dyn VfsNode>> {
    walk(path, false)
}

/// Resolve an absolute path, following symbolic links (`follow_final` decides
/// whether the last component is expanded too).
///
/// The walk is iterative with an explicit component stack: a target like
/// `../../x` or `/mnt/y` splices its components in front of the remaining ones.
/// `SYMLOOP_MAX` bounds the expansions, so a link cycle is `InvalidArgument`
/// rather than an infinite loop.
pub fn walk(path: &str, follow_final: bool) -> VfsResult<Arc<dyn VfsNode>> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
    let root = root_node()?;
    let mut cur = Arc::clone(&root);
    let mut stack = components(path);
    let mut hops = 0usize;

    while let Some(comp) = stack.pop() {
        if comp == "." {
            continue;
        }
        if comp == ".." {
            // A directory's `..` entry is the honest answer (`lookup`), except at
            // the tree's root, whose `..` does not exist and which therefore
            // stays put.
            if !cur.is_directory() {
                return Err(VfsError::NotADirectory);
            }
            match cur.lookup("..") {
                Ok(parent) => cur = parent,
                Err(VfsError::NotFound) => {}
                Err(e) => return Err(e),
            }
            continue;
        }
        if !cur.is_directory() {
            return Err(VfsError::NotADirectory);
        }
        let node = cur.lookup(&comp)?;
        let is_last = stack.is_empty();
        if node.is_symlink() && (!is_last || follow_final) {
            hops = hops.saturating_add(1);
            if hops > SYMLOOP_MAX {
                return Err(VfsError::InvalidArgument);
            }
            let target = node.read_link().ok_or(VfsError::Corrupt)?;
            if target.is_empty() {
                return Err(VfsError::NotFound);
            }
            if target.starts_with('/') {
                cur = Arc::clone(&root);
            }
            // A relative target is relative to the directory that contains the
            // link, which is `cur` right now (the link was looked up *in* it).
            //
            // `components` returns the path *reversed* (its `pop` is the next
            // component to process), and `stack` uses the same convention — so
            // the target's components are pushed back in reverse order. Pushing
            // them in the order `components` hands them out is what puts the
            // first component of the target on top; the earlier loop popped
            // while pushing, which reversed the target twice and made every
            // symlink resolve to a rotated path.
            let expansion = components(&target);
            for c in expansion {
                stack.push(c);
            }
            continue;
        }
        cur = node;
    }
    Ok(cur)
}

// ─── operations shared by callers ──────────────────────────────────────────

/// Replace a regular file's contents with `bytes` (POSIX `O_TRUNC` semantics).
///
/// Writing at offset 0 is *not* replacement: a shorter text over a longer file
/// would leave the old tail behind. Truncation lives here, in one place, because
/// every writer that forgot it grew the same bug.
pub fn replace_contents(node: &Arc<dyn VfsNode>, bytes: &[u8]) -> VfsResult<usize> {
    node.truncate(0)?;
    if bytes.is_empty() {
        return Ok(0);
    }
    node.write(0, bytes)
}

/// Rename `old_path` to `new_path`, both absolute, following links only in the
/// parent components (never the entries being moved).
///
/// Both parents must resolve to directories on the same filesystem; the
/// filesystem's own rename then decides whether the target may be replaced
/// (empty directory over empty directory, file over file). Cross-filesystem
/// renames are `NotSupported` — a copy is not a rename.
pub fn rename(old_path: &str, new_path: &str) -> VfsResult<()> {
    let (old_parent_path, old_leaf) = parent_and_leaf(old_path)?;
    let (new_parent_path, new_leaf) = parent_and_leaf(new_path)?;
    let old_parent = walk(&old_parent_path, true)?;
    let new_parent = walk(&new_parent_path, true)?;
    if !old_parent.is_directory() || !new_parent.is_directory() {
        return Err(VfsError::NotADirectory);
    }
    old_parent.rename(&old_leaf, &new_parent, &new_leaf)
}
