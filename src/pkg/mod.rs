//! Native package installer (MVP): a `.ppkg` downloaded over plain HTTP,
//! checked against the repository index's size and SHA-256, then installed
//! under `/mnt`.
//!
//! The format lives in `pagh-programs/docs/package-format.md`; the shell's
//! `pkg` command is the only caller. This module is deliberately not apt: one
//! flat index, one ELF payload per package, no dependency solver, no TLS and no
//! signatures yet. See [`sha256`] for the streaming checksum.

pub mod sha256;
