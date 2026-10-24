//! Device drivers.
//!
//! Every driver here is MMIO against an address the device tree reported — the
//! kernel hard-codes no machine layout, which is the one habit worth keeping
//! from the x86 port (there, the equivalent facts came from Limine and PCI).

pub mod uart;
