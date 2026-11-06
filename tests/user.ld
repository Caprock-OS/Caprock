/* Linker-Skript für extern geladene Caprock-EL0-Programme (ext-26).
 *
 * ET_EXEC, an einer KANONISCHEN festen VA gelinkt (0x6000_0000, in GiB 1). Der Loader lädt das
 * Programm an BELIEBIGE allokierte Physadressen und mappt seine PT_LOAD-Segmente per
 * vspace_map_page_at(va -> pa) in die isolierte VSpace (VA != PA, nur diese private VSpace).
 *
 * W^X: Code (R+X), Rodata (R), Daten (R+W) liegen in GETRENNTEN, 4-KiB-ausgerichteten PT_LOAD-
 * Segmenten, damit der Loader sie mit unterschiedlichen Rechten mappen kann. Entry = _start. */

ENTRY(_start)

PHDRS {
    text   PT_LOAD FLAGS(5); /* R + X */
    rodata PT_LOAD FLAGS(4); /* R     */
    data   PT_LOAD FLAGS(6); /* R + W */
}

SECTIONS {
    . = 0x60000000;

    .text : ALIGN(4096) {
        *(.text._start)   /* Entry zuerst */
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(4096) {
        *(.data .data.*)
    } :data

    .bss : ALIGN(8) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
        *(.ARM.exidx .ARM.exidx.*)
    }
}
