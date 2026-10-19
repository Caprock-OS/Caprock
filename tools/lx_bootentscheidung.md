# Bootloader-Entscheidung: Braucht Caprock einen eigenen Bootloader?

Stand 2026-09-09. Antwort kurz: **Nein — auf keiner der beiden Architekturen.**
GRUB (x86) bzw. vorhandene Firmware-Lader (aarch64) tragen den einzigen Vertrag,
den der Kernel braucht: **Modul-Transport = Archiv an Kernel**
(`loader::set_archive_span`). Alles andere — Partitionstabelle, Dateisystem,
Messkette — gehört nach `tools/` bzw. ins Userland, nicht in einen eigenen Loader.

## 1. Befund: Wie wird heute gebootet?

### x86_64

- **Testpfad:** `qemu-system-x86_64 -kernel <kernel>.mb32 -initrd build/boot-archive-x86.bin`
  (`test-qemu-x86-load.sh`, Funktion `boot()`). QEMUs eingebauter
  Minimal-Multiboot-Lader legt `-initrd`-Dateien als **Multiboot-Module** ab und
  trägt sie in `mods_count`/`mods_addr` ein — genau das liest A-1.1
  (`kernel/src/arch/x86_64/bringup.rs`, `set_archive_span(mods[0].start, mods[0].len())`).
  `-device loader` wäre hier falsch: es schriebe nur Bytes an eine Adresse,
  ohne den Kernel zu informieren (AGENTS.md, Mitteilung 3).
- **Echt-Hardware-Pfad:** `tools/mkgrubiso.sh` baut ein GRUB-El-Torito-ISO aus
  Kernel + Boot-Archiv (`multiboot /boot/kernel.mb32`, `module /boot/boot-archive.bin`).
  Belegt 2026-08-01 (TODO.md Z11a): ein echter Bootloader liefert nachweislich
  **dasselbe** wie QEMUs `-kernel`/`-initrd` — Speicherplan, ein Modul,
  `mbmod : ALL PASS`, `archive : 2 Modul(e) -> ALL PASS`, Root-Task läuft,
  `SELFTEST COMPLETE`. Einziger Unterschied: die Ladeadresse des Moduls, wie erwartet.
  Die Sorge wegen `Flags = 0` im Multiboot-Header war unbegründet — GRUB liefert
  Speicherplan und Module auch ungefragt.
- **Was GRUB leistet:** Speicherplan (Multiboot-Info) + Modul-Transport.
- **Was der Kernel braucht:** genau das — nicht mehr. Der Modulbereich wird
  **vor** der ersten Allokation aus der Freiliste ausgeschnitten (`mbmod`-Zeile),
  danach gilt nur noch `loader::set_archive_span(base, len)` /
  `loader::read_archive()`. Kein Dateisystem, keine Shell, keine Treiber im Loader.

### aarch64

- **Testpfad:** `qemu-system-aarch64 -kernel <kernel>.elf -device loader,file=build/boot-archive.bin,addr=0x13F000000`
  (`test-qemu.sh`, `boot_once()`). Eintritt direkt auf EL1, `x0` = DTB-Zeiger
  (`kernel/src/arch/aarch64/boot.rs`); das Archiv liegt per **Verabredung mit dem
  Testaufbau** im statischen Fenster (`loader::MOD_BASE`, `MOD_WINDOW` = 16 MiB,
  vom `PhysAllocator` ausgenommen). `set_archive_span` ist hier ungesetzt —
  `archive_span()` fällt auf das ARM-Fenster zurück, auf x86 auf `(0, 0)` („kein Archiv").
- **Echt-Hardware-Pfad:** offen — QEMU-`virt` + `-device loader` gibt es auf Blech nicht.
  Das ist aber **kein Loader-Problem im Sinne von „eigener Bootloader"**, sondern ein
  fehlender Übergabevertrag: Wer legt das Archiv wohin und sagt dem Kernel die Adresse?
- **Was der Kernel braucht:** auch hier nur den Transport + die Adresse —
  das ARM-Gegenstück zu `set_archive_span`, z. B. über einen DTB-`chosen`-Knoten
  statt der festen `MOD_BASE`-Verabredung.

## 2. Z7 (Attestierung/messbarer Boot): Erzwingt das einen eigenen Loader?

Nein. Die Messkette ist **von außen** gebaut, nicht vom Loader neu erfunden:

- **Kette:** Firmware misst Bootloader (PCR Extend + TCG Event Log),
  Bootloader misst Kernel + Modul (GRUB2 mit TPM-Unterstützung kann genau das:
  `linux`/`multiboot`/`module`-Befehle extenden PCRs und protokollieren ins Event Log),
  Kernel misst Programme (existiert bereits: `kernel_code_hash`-Bindung des
  Manifests an `[__text_start, __rodata_end)` — `loader.rs`/`tools/kernel_hash.py` —
  plus `binary_hash`/`manifest_hash`-Bindung in TrustedSAS-Zertifikaten, ADR 0014).
- Ein eigener minimaler Loader müsste TPM-Treiber, Event-Log-Format, PCR-Belegung
  und Quote-Protokoll **neu bauen und auditieren** — für exakt die zwei Dinge,
  die GRUB bereits kann und die der Kernel ohnehin selbst prüft (Signatur,
  Image-Bindung, Anti-Downgrade). Das ist die teuerste Art, nichts zu gewinnen.
- Was für Z7/B-6.1 **wirklich fehlt**, ist die Anbindung, nicht der Loader:
  PCR-Belegung dokumentieren, TCG Event Log vom Boot an den Kernel durchreichen
  (als zweites Multiboot-Modul oder DTB-Übergabe — wieder nur Transport),
  Attestierungsdienst als **Userland-PD** (Quote anfordern, Messung signieren),
  Tenant-Protokoll dafür. Das ist Userland + Werkzeugseite, kein Bootsektor-Code.

## 3. Z11a zweite Hälfte („ein Sektor ist kein Archiv"): Bootloader oder tools/?

Eindeutig **`tools/` + Userland**, nicht Bootloader. Beleglage:

- Partitionstabelle und FAT16 sind bereits **kernfreie** Crates
  (`caprock-part`, `caprock-fat`, `forbid(unsafe_code)`, host-getestet über
  `tools/host-tests.sh`), das Abbild baut `tools/mkgpt.py`, die Gegenprobe liest
  `tools/checkfat.py` — unabhängig vom Kernel am Abbild nach (`A-6.4`).
- Der Plattentreiber läuft seit A-5.1 als **geladenes Userland-Programm**,
  Blockdienst (A-6.1) und Dateisystem-PD (A-6.3) ebenso — alle aus dem Archiv,
  alle über ihren Kanal bedient.
- Der Bootloader liefert nur die **Startmenge** (Multiboot-Module); das Manifest
  nennt sie, der Kernel prüft, dass genau das ankam. Nachladen über Platte/Netz
  gibt es erst, wenn die Treiber laufen — und die Treiber laufen bereits.
  Was fehlt, ist „alles darüber" (Laufzeit-Nachladen über Blockdienst/fs-PD
  bis zum Archiv-Format), also Ladepfad im Kernel + Werkzeuge, kein Bootloader.

## 4. Entscheidung

| Arch | Eigener Bootloader? | Begründung |
|---|---|---|
| **x86_64** | **Nein. GRUB reicht.** | `mkgrubiso.sh` belegt seit 2026-08-01: GRUB liefert Speicherplan + Modul = alles, was `set_archive_span` braucht. Ein eigener Loader duplizierte GRUB für genau unseren Fall und müsste danach trotzdem TPM, ISO9660/El-Torito und Firmware-Macken tragen. |
| **aarch64** | **Nein. Kein eigener Loader; vorhandene Firmware-Lader (U-Boot/EDK2/GRUB-EFI) + definierter Übergabevertrag.** | Der heutige `-device loader`+`MOD_BASE`-Weg ist ein Test-Hack, kein Produktweg — aber die Lücke ist der Vertrag (Adresse übergeben), nicht der Lader. Auf Blech gehört das Archiv per U-Boot/UEFI geladen und die Spanne übergeben (DTB-`chosen`, analog `set_archive_span`), nicht per neuem C/ASM-Lader. |

Was **konkret fehlt** (Lückenliste, kein Bootloader):

1. **x86-Werkzeugseite härten:** `tools/mkgrubiso.sh` *ist* der GRUB-Ersatz für
   genau unseren Fall (Kernel + Module → bootbares Image, keine Allzweck-Shell) —
   es braucht keinen Nachfolger, sondern Abnahme (ISO-Boot in der Reihe fahren,
   `--no-archive`-Negativfall eingeschlossen).
2. **aarch64-Übergabevertrag für echte Hardware:** `MOD_BASE`-Verabredung durch
   eine gemeldete Spanne ersetzen (DTB-`chosen`-Knoten o. ä., Rückfall bleibt das
   Fenster) — Spiegelbild zu `set_archive_span` auf x86.
3. **Z7-Anbindung (B-6.1):** PCR-Belegung + Event-Log-Durchreichung (Transport,
   kein Loader-Code), Attestierungs-PD im Userland, Tenant-Protokoll.
4. **Z11a-Werkzeugseite:** Laufzeit-Nachladen über Blockdienst/fs-PD bis zum
   Archiv-Format ausbauen (`mkgpt.py`/`checkfat.py` sind da; der Plattentreiber
   als Userland-PD auch) — „ein Sektor ist kein Archiv" wird im Ladepfad gelöst,
   nicht im Bootsektor.

## 5. Nachweis

- Diese Datei ist die Entscheidung mit Befund (Quellen: `test-qemu-x86-load.sh`
  `boot()`, `test-qemu.sh` `boot_once()`, `tools/mkgrubiso.sh`, `tools/mkarchive.py`,
  `kernel/src/loader.rs` `set_archive_span`/`archive_span`, `kernel/src/arch/aarch64/boot.rs`,
  `TODO.md` Z11a/Z7, `todo-B-verlaesslichkeit.md` B-6.1, `docs/plan-betriebsbereit.md` Stufe 3).
- `tools/lx_bootcheck.sh --help` / ohne Argumente prüft den Befund am Baum nach
  (Multiboot-Magic-Lage im `mb32`, Archiv-Werkzeuge, GRUB-Werkzeuge, QEMU-Befehlszeilen
  beider Arches) — Demo ohne Boot, ohne Schreiben an bestehenden Dateien.
- **Ausdrücklich nicht gebaut:** kein eigener Loader, kein C/ASM-Monster.
  Ein `tools/lx_mkboot.sh` als minimaler GRUB-Ersatz wäre erst gerechtfertigt,
  wenn GRUB den Zweidinge-Vertrag (Kernel + ein Modul) nachweislich nicht trägt —
  das Gegenteil ist seit 2026-08-01 belegt.
