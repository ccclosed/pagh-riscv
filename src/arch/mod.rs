//! Architecture-specific code.
//!
//! Exactly one architecture lives here today (RV64). The split exists so that a
//! subsystem — memory, traps, the shell's notion of "halt" — asks *this* module
//! for a machine fact instead of reaching into a register itself.

pub mod riscv64;
