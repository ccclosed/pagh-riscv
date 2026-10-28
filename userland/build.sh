#!/bin/sh
# Build the two checked-in RISC-V userland fixtures with the kernel's toolchain.
#
# The kernel embeds both ELFs (include_bytes!), so a clean checkout builds
# without this script. Re-run it only when hello.rs / fault.rs / user.ld change,
# and commit the regenerated ELFs.
set -eu
cd "$(dirname "$0")"

TOOLCHAIN="${TOOLCHAIN:-nightly-2026-07-12}"
TARGET=riscv64gc-unknown-none-elf
RUSTC="rustup run $TOOLCHAIN rustc --target $TARGET"
FLAGS="-C panic=abort -C opt-level=2 -C debuginfo=0 -C relocation-model=static -C force-frame-pointers=no"

build() {
    src=$1
    out=$2
    $RUSTC $FLAGS --crate-type bin \
        -C link-arg=-Tuser.ld \
        -C link-arg=--no-dynamic-linker \
        -C linker=rust-lld \
        -o "$out" "$src"
    echo "built $out"
}

build hello.rs hello.elf
build fault.rs fault.elf
build spin.rs spin.elf
