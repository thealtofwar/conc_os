/* Link script for conc_os guest images.
 *
 * Guests are loaded by the hypervisor at guest-physical 0x10000 with an
 * identity-mapped 64-bit address space.  Everything below 0x10000 is owned
 * by the hypervisor (page tables, GDT, boot parameters, request mailbox).
 */
ENTRY(_start)

SECTIONS
{
    . = 0x10000;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
    }

    . = ALIGN(4096);
    .data : {
        *(.data .data.*)
        *(.got .got.*)
    }

    . = ALIGN(4096);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment)
    }
}
