//! Storage: the ext2 filesystem, the format policy, and their in-QEMU tests.
//!
//! Layer map:
//!
//! | File | Role |
//! |---|---|
//! | [`ext2`] | ext2 on-disk format, mount, read/write/create/remove/rename |
//! | [`format_policy`] | the only decision that may destroy data: is this device blank? |
//! | [`selftest`] | non-destructive `MemBlock`-backed suite run by the shell's `selftest` |
//!
//! The boot wiring lives in `src/main.rs` (`init_fs`): it asks
//! `drivers::block::get()` for a device, mounts it, and — when the mount fails
//! and only then — asks [`format_policy`] whether formatting a blank device is
//! allowed. The filesystem itself never decides to format anything.
//!
//! There is no journal in M5. Mutations are ordered (data blocks before
//! metadata) and synchronous, but a crash between two writes can leave an
//! operation half-applied; see `src/fs/README.md` for the honest list.

pub mod error;
pub mod ext2;
pub mod format_policy;
pub mod selftest;

pub use error::FsError;
