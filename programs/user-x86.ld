/* Linker-Skript für extern geladene Caprock-Ring-3-Programme auf **x86_64** (A-1.5).
 *
 * Gleiche Bauart wie `user.ld` (aarch64) — nur die feste Link-Adresse ist eine andere, und das
 * ist keine Geschmacksfrage:
 *
 * Auf x86 hat eine isolierte VSpace **ihr eigenes** Page Directory nur für GiB 0; GiB 1..3 teilen
 * sich alle isolierten Adressräume statisch (`ISO_PD_HIGH`, ohne `US`). `vspace_map_page_at`
 * weist deshalb jede VA ab, die nicht in `[16 MiB, 1 GiB)` liegt. Die aarch64-Adresse
 * 0x6000_0000 liegt dort in GiB 1 — auf x86 wäre sie knapp **über** dem Fenster und ließe sich
 * nicht mappen. Also 0x2000_0000 (512 MiB): weit über dem Kernel-Image, weit unter der Grenze,
 * und mit reichlich Abstand zum Stack-Fenster (`LOADED_STACK_VA`, s. kernel/src/system.rs).
 *
 * W^X: Code (R+X), Rodata (R), Daten (R+W) in GETRENNTEN, 4-KiB-ausgerichteten PT_LOAD-Segmenten.
 * Entry = _start. */

ENTRY(_start)

PHDRS {
    text   PT_LOAD FLAGS(5); /* R + X */
    rodata PT_LOAD FLAGS(4); /* R     */
    data   PT_LOAD FLAGS(6); /* R + W */
    /* **`PT_TLS` ist das Anfangsbild, nicht der Speicher** (T3). Es sagt, welche Bytes ein neuer
       Thread als Startwert seiner thread-lokalen Variablen bekommt; WO er sie hinlegt, steht
       nirgends im Programmkopf und geht auch den Kernel nichts an -- der haelt nur das
       Thread-Pointer-Register (s. `docs/plan-tls.md`). Kopiert wird das Bild vom Programm selbst
       (`libcaprock`), weil es keinen dynamischen Lader gibt und keinen geben soll. */
    tls    PT_TLS  FLAGS(4); /* R     */
}

SECTIONS {
    . = 0x20000000;

    .text : ALIGN(4096) {
        *(.text._start)   /* Entry zuerst */
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(4096) {
        *(.data .data.*)
        *(.got .got.plt)
    } :data

    /* **ALIGN(4096), nicht ALIGN(8)** (2026-08-10) -- und der Grund ist gemessen, nicht
       vorsorglich: `wasmhost` ist das einzige Programm mit einem SCHREIBBAREN Segment und hat
       keine `.data`. lld verwirft die leere `.data`-Ausgabesektion, das RW-PT_LOAD beginnt also
       bei `.bss` -- mit dessen 8-Byte-Ausrichtung, konkret `0x2004_6700`.
       `vspace_map_page_at` weist eine krumme VA beim ALLERERSTEN Aufruf ab (`va % PAGE != 0`, in
       BEIDEN HALs). Das Image war damit auf keiner Architektur ladbar, und der Ladepfad meldete
       den Fehlschlag als „Speicher fuer eine Seitentabelle" -- eine Ursache, die er nicht gemessen
       hatte.
       Kernelseitig die VA abzurunden waere KEINE Behebung, sondern ein W^X-Loch: `.rodata` reicht
       in dieselbe Seite, sie waere erst RO und dann RW gemappt. Die Ausrichtung gehoert dorthin,
       wo das Segment entsteht. Bewacht von `tools/segment-ausrichtung.sh`. */
    .bss : ALIGN(4096) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* --- Thread-lokale Daten (T3) ---------------------------------------------------------
     *
     * **Sie stehen NACH `.bss`, und das ist der Kern der Sache.**
     *
     * Erster Anlauf: `.tdata` vor `.bss` mit `ALIGN(16)` -- da `.data` bei `virtio-blk` und
     * `wasmhost` leer ist und lld sie verwirft, wurde `.tdata` zur ERSTEN Sektion des
     * RW-Segments und gab ihm ihre Ausrichtung (`0x2000_2040`, `0x2004_6700`). Genau die Falle
     * vom 2026-08-10, gefangen von `tools/segment-ausrichtung.sh`.
     *
     * Zweiter Anlauf: `ALIGN(4096)`. Segment wieder gerade -- und **`p_align` des `PT_TLS` damit
     * 4096**. Der Uebersetzer rechnet die local-exec-Adresse gegen die auf `p_align`
     * AUFGERUNDETE Blockgroesse, also `tp - 4096`; der Laufzeitblock hatte 16 Byte. Ergebnis:
     * `FAR=0x2000_2010` bei `tp=0x2000_3010`, exakt eine Seite darunter. Kein Absturz beim Bau,
     * kein Wort im Protokoll -- der Treiber starb still.
     *
     * Dritter und richtiger: **hinter `.bss`**. Dessen `ALIGN(4096)` bestimmt den Segmentanfang
     * wie vor T3, und `.tdata` darf klein ausgerichtet sein. Die beiden Groessen -- Anfang des
     * Segments und Ausrichtung des TLS-Blocks -- waren nie dieselbe Frage; sie hingen nur am
     * selben Feld.
     *
     * **`ALIGN(16)` ist damit die Ausrichtung, die `libcaprock::TLS_ALIGN` spiegeln MUSS.**
     * Laufen sie auseinander, liegt die Variable neben ihrem Speicher -- ohne Fehlermeldung, wenn
     * der Versatz klein ist, und mit einem Fault eine Seite daneben, wenn er gross ist.
     *
     * Die drei Symbole sind der ganze Vertrag:
     *   `__tdata_start` .. `__tdata_end`  das zu kopierende Anfangsbild
     *   `__tdata_start` .. `__tbss_end`   die Gesamtgroesse eines TLS-Blocks */
    .tdata : ALIGN(16) {
        __tdata_start = .;
        *(.tdata .tdata.*)
        __tdata_end = .;
    } :data :tls

    .tbss : {
        *(.tbss .tbss.*)
        *(.tcommon)
        __tbss_end = .;
    } :data :tls

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
