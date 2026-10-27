//! The error model shared by the filesystem layer.
//!
//! Every failure in `src/fs/**` is one of these values; nothing panics on
//! on-disk input. The variants are mapped to the VFS's [`crate::vfs::VfsError`]
//! at the node adapters (one match arm), and to shell text there too, so this
//! enum is the only place that knows ext2's failure taxonomy.
//!
//! It lives in its own file rather than in `fs/mod.rs` on purpose: the host-side
//! in-QEMU suite (`src/fs/selftest.rs`) exercises `fs/ext2/**` and
//! `fs/format_policy.rs` against a tiny `fs` shim, and a shim cannot import
//! `fs/mod.rs` without dragging in the in-kernel self-tests.

/// Errors produced by the filesystem subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The on-disk superblock is missing or invalid (bad magic, unsupported
    /// block size, impossible geometry).
    BadSuperBlock,
    /// A bitmap had no free bit left: no space or no inode.
    OutOfSpace,
    /// A path component / directory entry / inode was not found.
    NotFound,
    /// An entry with the same name already exists (or a directory is not empty).
    AlreadyExists,
    /// The underlying block device returned an error (or a short transfer).
    IoError,
    /// A directory entry name exceeded 255 bytes.
    NameTooLong,
    /// A structural invariant was violated (corrupt/out-of-range on-disk field).
    Corrupt,
    /// A write would exceed the 32-bit `i_size` limit of the format.
    FileTooBig,
    /// A directory operation needed an empty directory and found entries.
    NotEmpty,
    /// The path names a directory where a file was required.
    IsDirectory,
    /// A component of the path is not a directory.
    NotDirectory,
    /// A malformed request (bad name, renaming a directory into its own tree,
    /// type mismatch in `rename`, ...).
    InvalidArgument,
}

impl FsError {
    /// Stable lowercase name for the serial log.
    pub fn as_str(self) -> &'static str {
        match self {
            FsError::BadSuperBlock => "bad superblock",
            FsError::OutOfSpace => "out of space",
            FsError::NotFound => "not found",
            FsError::AlreadyExists => "already exists",
            FsError::IoError => "i/o error",
            FsError::NameTooLong => "name too long",
            FsError::Corrupt => "filesystem is corrupt",
            FsError::FileTooBig => "file too big",
            FsError::NotEmpty => "directory not empty",
            FsError::IsDirectory => "is a directory",
            FsError::NotDirectory => "not a directory",
            FsError::InvalidArgument => "invalid argument",
        }
    }
}
