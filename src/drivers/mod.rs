//! Device drivers.
//!
//! Every driver here is MMIO against an address the device tree reported — the
//! kernel hard-codes no machine layout, which is the one habit worth keeping
//! from the x86 port (there, the equivalent facts came from Limine and PCI).
//!
//! Two exceptions state their reason in place: the console UART
//! ([`uart::BASE`]) and fw_cfg ([`fw_cfg::BASE`]) are needed before the tree is
//! parsed, and both fail loudly rather than silently when the address is wrong
//! (no output; a rejected signature).

pub mod block;
pub mod framebuffer;
pub mod fw_cfg;
pub mod ramfb;
pub mod selftest;
pub mod uart;
pub mod virtio_mmio;
pub mod virtio_net;
