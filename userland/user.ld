/* The userland link contract: one process, loaded by the kernel's ELF loader.
 *
 * The image starts at 0x1_0000 — inside the loader's user window
 * (0x10000 .. 0x3fff_0000), well away from the kernel's identity map at
 * 0x8000_0000 and from the MMIO windows at 0x0c00_0000/0x1000_0000.
 *
 * .text and .rodata carry R+X, .data/.bss carry R+W; rust-lld turns that split
 * into two PT_LOAD program headers, which is what exercises the loader's
 * rollback path in the reject matrix (an overlapping second segment fails after
 * the first one was mapped).
 */
ENTRY(_start)

SECTIONS
{
    . = 0x10000;

    .text : ALIGN(4096) {
        KEEP(*(.text.entry))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(4096);
    .data : ALIGN(16) {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    . = ALIGN(4096);
    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
        *(.riscv.attributes)
    }
}
