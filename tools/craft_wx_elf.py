#!/usr/bin/env python3
"""Craft the F-rv-2 W^X ELF: overlapping `PT_LOAD` permissions on one page.

    craft_wx_elf.py <in.elf> <out.elf> --mode overlap|segment

Both variants start from a normal static RV64 executable (the repository's
`userland/hello.elf`) and change only program-header fields, so the crafted file
is still a well-formed ELF apart from the permission trick:

* ``--mode segment`` — the first `PT_LOAD` (R|X) is rewritten to R|W|X. One
  header asks for a writable+executable page; the loader must refuse it with the
  same W^X rule `mmap`/`mprotect` use.
* ``--mode overlap`` — the second `PT_LOAD` (R, `rodata`) is moved onto the
  first segment's text page as an extra writable segment: the unused
  `GNU_STACK` program header is turned into a `PT_LOAD` with `PF_R|PF_W`,
  `p_filesz = 0`, `p_memsz = 0x1000`, so it only adds the write permission to
  the text page. Neither header alone is W+X; the union of the two covering
  headers used to be merged into one W+X leaf. The rest of the image is
  untouched, so the program still prints its greeting and exits 42 on a kernel
  that accepts it.

Before 0.24.1 the loader unioned the permission bits of every `PT_LOAD` covering
a page (`src/user/elf.rs`), so both files loaded and the first page became
writable+executable. The regression test is the reject matrix in
`src/user/selftest.rs`; this script is the live harness's input (and the PoC
source copied from the audit report, which had not built one).

Exit status: 0 on success, 2 on usage/ELF-shape errors.
"""

from __future__ import annotations

import argparse
import struct
import sys

PT_LOAD = 1
PF_X = 1
PF_W = 2
PF_R = 4


def u16(data: bytes, off: int) -> int:
    return struct.unpack_from("<H", data, off)[0]


def u32(data: bytes, off: int) -> int:
    return struct.unpack_from("<I", data, off)[0]


def u64(data: bytes, off: int) -> int:
    return struct.unpack_from("<Q", data, off)[0]


def craft(data: bytearray, mode: str) -> bytearray:
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise SystemExit("not a little-endian ELF64")
    e_phoff = u64(data, 32)
    e_phentsize = u16(data, 54)
    e_phnum = u16(data, 56)
    loads: list[int] = []
    for index in range(e_phnum):
        base = e_phoff + index * e_phentsize
        if u32(data, base) == PT_LOAD:
            loads.append(base)
    if len(loads) < 2:
        raise SystemExit("need at least two PT_LOAD headers, found %d" % len(loads))
    first, second = loads[0], loads[1]
    if mode == "segment":
        flags = u32(data, first + 4)
        struct.pack_into("<I", data, first + 4, flags | PF_W | PF_X)
        return data
    if mode == "overlap":
        text_vaddr = u64(data, first + 16)
        text_page = text_vaddr & ~0xFFF
        # An unused header (the fixture's `GNU_STACK`) becomes the overlapping
        # writable `PT_LOAD`: same page as the text segment, no file bytes, one
        # zero-filled page. p_offset stays page-congruent with the new vaddr and
        # p_align becomes 0x1000, which is what the loader's alignment rule
        # wants for a real PT_LOAD.
        spare = None
        for index in range(e_phnum):
            base = e_phoff + index * e_phentsize
            if base not in loads:
                spare = base
                break
        if spare is None:
            raise SystemExit("no spare program header to turn into the overlap")
        struct.pack_into("<I", data, spare, PT_LOAD)
        struct.pack_into("<I", data, spare + 4, PF_R | PF_W)
        struct.pack_into("<Q", data, spare + 8, 0x2000)
        struct.pack_into("<Q", data, spare + 16, text_page)
        struct.pack_into("<Q", data, spare + 24, text_page)
        struct.pack_into("<Q", data, spare + 32, 0)
        struct.pack_into("<Q", data, spare + 40, 0x1000)
        struct.pack_into("<Q", data, spare + 48, 0x1000)
        return data
    raise SystemExit("unknown mode %r" % mode)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", help="base ELF (e.g. userland/hello.elf)")
    parser.add_argument("output", help="crafted ELF to write")
    parser.add_argument(
        "--mode",
        choices=("overlap", "segment"),
        default="overlap",
        help="which W^X bypass to craft (default: overlap)",
    )
    args = parser.parse_args()
    with open(args.input, "rb") as handle:
        data = bytearray(handle.read())
    crafted = craft(data, args.mode)
    with open(args.output, "wb") as handle:
        handle.write(crafted)
    print("craft_wx_elf: mode=%s wrote %s" % (args.mode, args.output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
