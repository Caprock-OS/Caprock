# BETRIEB.md — Loader-Dienst-PD (`programs/lxpd-runtime`)

Loader-Dienst für den **Betrieb**: lädt Treiber-LXPD-Images von Platte über einen **externen**
Blockdriver (fremde PD, nicht Teil von Caprock), verifiziert und stößt Instanziierung an.
Gegenstück zum Boot-Lader (Boot-Archiv): Was nach dem Boot kommt, kommt von Platte — und wird
genauso geprüft, bevor es läuft.

Muster: `programs/mem-server` (Worte-Protokoll, Zustandsmaschine, `/tmp`-Test) und
`programs/lx-shim-demo` (Standalone-Bauweise, gestellt vs. belegt). Deutsch, `no_std` +
`forbid(unsafe_code)`, standalone-Crate mit leerem `[workspace]` (NICHT im Workspace).

## 1. Protokoll

Vier Register je Nachricht (`[u64; 4]`), zwei Richtungen, ein Format (s. `src/protokoll.rs`).

**Richtung A — Lader → Blockdriver (Lader ist Client, Teilmenge von `virtio-blk`):**

```text
[OP, LBA, ANZAHL, 0]
OP_INFO (0) → [STATUS, Kapazität, MaxSektoren, SektorBytes]
OP_READ (1) → [STATUS, GeleseneSektoren, 0, 0]
Status: OK=0, DEVICE=1, BADOP=2, RANGE=3 (byte-identisch zum virtio-blk-Dienst)
```

Die Bytes stehen nie in den Worten: Sie liegen in der geteilten Übertragungsfläche (Staging
wie in `programs/hardware/virtio-blk` — der Treiber füllt seinen Puffer, der Lader kopiert
heraus). Sektornummern und Bytes in dieselben vier Register zu falten hieße, Bytes zu
adressieren, die der Lader nicht hält.

**Richtung B — irgendwer → Lader (Lader ist Server):**

```text
[AUSKUNFT (10), 0, 0, 0] → [KODE, Phase, BildLänge, 0]
[LADEN (11), PartitionsIndex, 0, 0] → [KODE, NeuePdId, 0, 0]
KODE 0 = OK, sonst benannter Ausgang (s. `lade_kode`).
```

**Wiederverwendet oder begründet abgewichen** (gegen `programs/hardware/virtio-blk` gelesen):

- `INFO`/`READ` + Statuscodes: **wiederverwendet** (byte-identisch). Jeder Blockdriver, der das
  virtio-blk-Protokoll spricht, bedient diesen Lader ohne Änderung.
- `SCAN`: **nicht verwendet**. SCAN meldet nur die ERSTE Partition (Zahl, LBA, Größe) — ohne
  Typ-GUID. Der Lader braucht die Typ-Auswahl (LXPD gegen fremd) und liest deshalb Roh-Sektoren
  per READ und parst mit `caprock-part` selbst (dieselben Funktionen, dieselben Regeln).
- `WRITE`/`FLUSH`/`STOP`: **nie gesendet**. Ein Lader schreibt keine Platte — der Client baut
  dafür nicht einmal Worte (was man nicht bauen kann, schickt man nicht versehentlich). LESEN
  genügt, weil der gesamte Ladeweg (GPT → Verzeichnis → Bild → Manifest) nur liest; ein
  Schreibpfad wäre Angriffsfläche ohne Benutzer.
- `ST_NOTABLE`: folgt aus SCAN; ohne SCAN keine Tabelle-als-Status. GPT-Fehler meldet der Lader
  als eigene benannte Absagen.

**Plattenformat** (erster Sektor der LXPD-Partition, `BildVerzeichnis`):

```text
0  Magic "LXIMG2\0\0" (8 B — Fassung 2: mit Eintrag; ohne Eintrag ist kein Vorgänger,
   sondern ein fremdes Format)
8  Eintragslänge u32 LE (Treiber-Eintrag als JSON, 1..=4096)
12 Bildlänge u32 LE (Treiber-Image, 1..=65536)
16 Manifestlänge u32 LE (LXPD-Manifest als JSON, 1..=8192)
20 Flags u32 LE (muss 0 sein — Reservenull, kein „egal")
```

Danach: Eintrag ab Sektor 1, Bild dahinter, Manifest dahinter (je aufgerundet auf Sektoren).
Das Verzeichnis trägt nur Längen — die Bindung leistet der EINTRAG (`DriverEntry` aus
`crates/caprock-lxpd/src/driver.rs`): Herkunft (`source`: `disk` per Unique-GUID oder
Blockbereich — `boot` auf Platte ist ein Widerspruch), Bildbindung (SHA-256 über die exakten
Bildbytes) und Schlüsselbindung (`key_id` + FNV-Zeuge über Roh-Pubkey + Kanonik).

## 2. Ablauf

```text
LEER --suchen(index)--> VERZEICHNIS --bild_lesen()--> BILD --prüfen(manifest_key, pubkey)--> GEPRÜFT
                                                                                              |
                                                              anstoßen() [Loader-Cap + ABI]   v
                                                                                            GELADEN
```

Prüfreihenfolge (billig vor teuer, Form vor Krypto): Verzeichnis → exakte Längen → Eintrag
(`parse_entry`: Fassung, Namen, Herkunft, Hash-/Key-ID-Form) → Herkunftsbindung an DIESE
Partition (Unique-GUID oder Bereich; `boot` abgewiesen) → Manifest-Fakten + echte Signatur →
Container/ELF (dieselben Funktionen wie der Boot-Pfad — kein zweiter Loader) → Bildbindung
SHA-256 (`verify_image`) → Eintrags-Zeuge (`verify_witness` gegen den Root-Pubkey) →
Namensbindung Eintrag ↔ Manifest (`driver` — zwei gültige Dokumente sind noch kein Paar).
Jeder Schritt verlangt die Phase seines Vorgängers (`Ungeprüft` sonst). **Niemals laden ohne
Prüfung.**

Warum SHA-256 und nicht FNV für das Bild: Die Platte darf der Angreifer beschreiben — FNV-1a
ist nicht kollisionsresistent und wäre hier eine Attrappe (ausdrücklich so in
`crates/caprock-lxpd/src/driver.rs` festgehalten). FNV bleibt, wo es hingehört: als
Verfälschungs-Zeuge über Key + Kanonik (Manifest-Signatur, Eintrags-Zeuge) — niemals als
Bildbindung.

## 3. Wer laden darf (gegen `abi`/`microkit`/`loader` gelesen)

- `SYS_LOAD` ist auf eine `Loader`-Cap mit WRITE gegatet; die Cap kommt aus dem System-Manifest
  (`initial_caps`-Bit 0). Der geladene Prozess erhält NUR explizit delegierte Caps.
- Dieser Dienst braucht das Loader-Bit im EIGENEN Manifest-Eintrag. Ohne es scheitert der
  Anstoß mit `KeineLoaderCap`, BEVOR ein Syscall gebaut wird (fail-closed client-seitig, nicht
  erst per `ERR_BADCAP` im Kernel).
- `SYS_LOAD_IMAGE = 36` ist die Bild-Übergabe (ABI-Wahrheit in `caprock_abi::sys`, Spiegel
  in `libcaprock`): `x1` = Loader-Cap-Index (WRITE), `TAG` = Low-Byte Bild-Slot + Bits 8..40
  exakte Bildlänge (Bits 40..64 = 0, sonst Absage), `MSG0` = Programm-ID aus dem Boot-Manifest,
  `MSG1`/`MSG2` = Delegationsliste/Anzahl, `MSG3` = Ressourcenwunsch wie `SYS_LOAD`.
  Vertrauen kommt aus dem Boot-Manifest (Hash-Gleichheit mit dem Eintrag dieser Programm-ID)
  — Manifest-Cap braucht es keine. `KernelAnstoss` ruft diesen Pfad über
  `libcaprock::load_image`; welche Slots Bild und Loader-Cap halten, steht im EIGENEN Manifest
  der PD (`AnstossKontext` — der Dienst rät keine Slots, er reicht sie nur durch). Der Kernel
  rechnet NACH (`load_verified_image` im Parallelstrang): Die PD-Prüfung ist die erste, nicht
  die einzige. Stand dazu in `src/patch.txt` (`UMGESETZT am 2026-09-09` plus Register-Tabelle),
  als Konstante `PATCH_TEXT` im Code, damit der Vertrags-Test ihn belegt.
- Die Schlüssel stehen in der PD (Endowment), nie auf dem Draht:
  Zwei Endowments, zwei Rollen — der Manifest-Schlüssel (LXPD-Konvention, Hex-oder-roh wie
  `lx-bind`) für die Manifest-Signatur, der Roh-Pubkey (32 Byte, Key-DB-Form) für den
  Eintrags-Zeugen (bewusst roh verfüttert, nicht sniffend — s. `driver.rs`-Doku: Sniffen auf
  Roh-Bytes wäre nichtdeterministisch). Wer einen Schlüssel per Wort setzen könnte, wählte
  seinen eigenen Prüfer.

## 4. Vertrauensannahmen: was, wenn der Blockdriver lügt?

Der Blockdriver ist eine fremde PD. Jede Lüge hat einen benannten Fänger — oder ist als offene
Annahme benannt:

| Lüge des Treibers | Fänger |
|---|---|
| Defekte Sektoren / Stille (keine Antwort) | `Geraet` — Phase bleibt, kein Anstoß (`defekter_sektor_haelt_jeden_anstoss_auf`) |
| Abgebrochene Reads (kürzer als angefragt) | `Abgebrochen` — „kurz" ist kein „klein" (`abgebrochene_reads_sind_kein_kleines_bild`) |
| Gekipptes Bit im Bild | `BildHashWeichtAb` (SHA-256 gegen den Eintrag — der Eintrag meint ein ANDERES Bild; FNV wäre hier eine Attrappe) (`falscher_hash_weist_ab_als_anderes_bild`) |
| Falsches Bild zum Manifest (Stubzahl passt nicht) | `Container(BadManifest)` trotz gültiger Signatur FÜR SEINE Kanonik (`stubzahl_gegen_manifest_gebunden`) |
| Gültiger Eintrag A + gültiges Manifest B (fremde Zusagen) | `TreiberMismatch` — Namensbindung `driver` (`gemischtes_paar_ist_kein_paar`) |
| Eintrag für andere Partition (GUID/Bereich) | `Container(BadManifest)` — Herkunftsbindung (`fremde_guid_ist_keine_herkunft`, Bereichs-Form in `bereichs_eintrag_als_herkunft`) |
| Fremde Partitionen / falscher Index | Übersprungen bzw. `KeineLxpdPartition` — Fremde sind Normalfall, kein Fehler (`fremde_partition_ist_kein_lxpd`) |
| Kaputte GPT (Kopf- vs. Eintrags-CRC) | `GptKopfCrc` vs. `GptEintraegeCrc` vs. `GptSignatur` — Datenverlust, nicht „unformatiert" |
| Aufgeblähtes Verzeichnis (> 64 KiB Bild) | `BildZuGross` — benannt, nicht abgeschnitten (`bild_zu_gross_wird_benannt_nicht_abgeschnitten`) |
| Bytes, die kein Format tragen | `KeinBildformat` — kein Parse-Versuch auf Zufall |

**Offene Annahmen (benannt, nicht gebaut):**

1. Der FNV-Zeuge (Manifest-Signatur, Eintrags-Zeuge) ist Verfälschungserkennung, kein
   Authentizitätsbeweis im Public-Key-Sinn: Wer den öffentlichen Schlüssel kennt, kann den
   Zeugen nachrechnen UND fälschen (s. `driver.rs`-Doku). Echte Authentizität trüge erst die
   Ed25519-Signatur eines einbettenden Dokuments — im Betrieb gibt es keines (kein
   System-Manifest für Platteninhalte). Was der Zeuge leistet: Bindung an genau diesen
   Root-Schlüssel (`key_id`-Abgleich) plus Erkennung versehentlicher Verfälschung. Wer mehr
   verspricht, lügt.
2. Kein Anti-Downgrade: Ein älterer, gültig signierter Eintrag + älteres Bild bestehen jede
   Prüfung (Rollback auf bekannte-verwundbare Treiber). Eine Monotonie (Version/Fassung im
   Eintrag gegen gespeicherten Stand) ist offen.
3. Die Kern-Seite prüft NACH (`load_verified_image` im Patch): Die PD-Prüfung ist die erste,
   nicht die einzige. Ein Kern, der der PD glaubte, machte aus jeder kompromittierten PD einen
   Loader.
4. Der Treiber sieht die gelesenen Bytes (er liefert sie). Vertraulichkeit der Images gegen den
   eigenen Blockdriver gibt es nicht — nur Integrität (SHA-256) und Herkunft (Zeuge).
5. Staging-Schranken (8 Sektoren je Anfrage, 64 KiB Bild, 8 KiB Manifest, 4 KiB Eintrag) sind
   Dienst-Politik: Größere Treiber brauchen einen Nachfolger dieses Verzeichnisses, kein
   stilles Aufbohren.

## 5. Lizenz-Hinweis

AGPL-3.0-or-later wie das Programm `virtio-blk` (ebenfalls eine PD): Diese Crate linkt
`caprock-lxpd`/`caprock-loader` (Workspace-Lizenz, AGPL). Eine permissive Lizenz wäre ein
falsches Versprechen — die Grenz-Crates (`caprock-part` u. a.) bleiben davon unberührt.

## 6. Nachweis

Isolierter `cargo test` in einer `/tmp`-Kopie, Manifest-Umschreibung wie
`tools/host-tests.sh` (`mit_deps`): Crate-Quellen (einschliesslich `src/bin/lxpdrv.rs`) +
`caprock-part` + `caprock-lxpd` + `caprock-loader` (Dependenz von `caprock-lxpd`) +
`libcaprock` (Produktions-Anstoss `KernelAnstoss` → `load_image`) werden kopiert (kein Pfad
zurück in den Workspace — sonst zöge `.cargo/config.toml` das Custom-Target wieder herein),
25 Tests:

```sh
rm -rf /tmp/lxpd-rt && mkdir -p /tmp/lxpd-rt/{rt/src,rt/src/bin,part/src,lxpd/src,loader/src,sdk/src}
cp programs/lxpd-runtime/src/* /tmp/lxpd-rt/rt/src/          # lib.rs, protokoll.rs, patch.txt
cp programs/lxpd-runtime/src/bin/* /tmp/lxpd-rt/rt/src/bin/  # lxpdrv.rs (Gast-PD)
cp crates/caprock-part/src/lib.rs /tmp/lxpd-rt/part/src/
cp crates/caprock-lxpd/src/*.rs /tmp/lxpd-rt/lxpd/src/
cp crates/caprock-loader/src/*.rs /tmp/lxpd-rt/loader/src/
cp programs/libcaprock/src/lib.rs /tmp/lxpd-rt/sdk/src/
# Manifeste neu schreiben (Namen + Pfad-Dependenzen, je mit leerem [workspace]):
# rt → part + lxpd + sdk (plus `[[bin]] lxpdrv`); lxpd → loader; part/loader/sdk ohne
# Dependenzen. Dann AUS dem /tmp-Verzeichnis (sonst zöge die Repo-`.cargo/config.toml` das
# Custom-Target herein):
cd /tmp/lxpd-rt/rt && cargo test --release --lib
```

`--lib` mit Absicht: Die Gast-PD (`[[bin]] lxpdrv`, `#![no_main]`) baut für den Host, aber der
Test-Harness brächte ein zweites `_start` (Linker-Fehler, kein Test-Fehler — dieselbe Form wie
bei jeder anderen PD). Geprüft wird die Bin über den PD-Bau
(`cd programs && cargo build --release --target x86_64-caprock-user.json --manifest-path
lxpd-runtime/Cargo.toml`, s. `tools/lxpd-e2e.sh`), nicht über den Harness.

Erwartung: alle 25 Tests grün — 19 Dienst-Tests in `lib.rs` (sechzehn alte Gatter-/Prüf-Pfade
plus neu: `vertrag_load_image_36_und_tag_belegung` pinnt Nummer und TAG-Belegung gegen den
`libcaprock`-Spiegel und den `UMGESETZT`-Stand von `patch.txt`; `slot_kontext_laueft_bis_zum_
anstoss_durch` belegt, dass der Manifest-Kontext unverändert bis zum Anstoss läuft;
`geprueftes_bild_erst_nach_pruefen` belegt die Vorlage für die `LOAD_IMAGE`-Übergabe) plus 6
Protokoll-Tests in `protokoll.rs`. Daneben laufen die 5 `pack_tag`-Tests von `libcaprock`
selbst (eigene `/tmp`-Kopie, `cargo test --release`: Rundweg, Slot-/High-Bits-/Len-0-Absage,
ABI-Nummer). Die exakte Zählung steht im Ergebnisbericht, nicht hier (eine Zahl, die hier
stünde und dort nicht stimmte, wäre ein Beleg, der keiner ist).

Was der Host-Test NICHT fährt: den Syscall selbst. `KernelAnstoss::lade_bild` baut `int 0x80`
(`x86_64`) bzw. `svc #0` (`aarch64`) — auf dem Host liefe das ins Leere. Belegt ist, dass der
Kontext unverändert ankommt (Fake) und das Tag-Wort stimmt (`pack_tag`); der Rest ist
QEMU-Seite (Lade-Suite mit LXPD-Partition: Treiber-PD erscheint, `root: ALL PASS` bleibt).

Bau-Grenze: `libcaprock` trägt einen Panik-Handler nur auf freistehenden Zielen
(`target_os = "none"`, s. `programs/*-caprock-user.json`). Auf dem Host gäbe es sonst E0152
(zweiter `panic_impl` neben `std` — auch als blosse Dependenz, denn `cfg(test)` gilt dort
nicht für sie). PD-nah geprüft: `cargo check --target x86_64-unknown-none` ist grün.

## 7. ARM status (strand 8, LXPD-ARM — English by language rule)

The BOOT path reaches first driver start on aarch64 (`tools/lxpd_arm_fahrt1.sh`:
verify-before-load plus `[7]lxpd-minelf gestartet` plus vollzahl in the guest log).
What that covers, and where this service's E2E stops on ARM:

* Covered: manifest/driver checks are arch-neutral (`caprock-lxpd` builds for the
  ARM kernel target; Ed25519 manifest verify + sha256 image bind run before any
  load, on both arches). The ARM image-basis rule (user window
  `[0x40200000, 0x80000000)`, enforced by the HAL guard, surfaced as Code 10) is
  proven by the vehicle's negative boot.
* NOT covered — the boundary a started driver hits on ARM: there is no device-PD
  path on ARM yet (strand 2: no MSI-X, no `bind_irq`, no `offer_driver_device`
  from the ARM boot). A driver that starts therefore holds no device, no DMA
  window, no IRQ. First start is proven; first INTERRUPT is not.
* This service (`lxpdrv`) is an x86_64 PD today: it builds for
  `x86_64-caprock-user.json`, its `KernelAnstoss` staging and the E2E partitions
  (`tools/lxpd-e2e.sh`) are x86-only. An `aarch64-caprock-user.json` build, an
  ARM block driver to read from, and a `SYS_LOAD_IMAGE` proof on ARM are all
  open (block E2E is explicitly out of strand 8).
* Container-form (`LXPD` v1) loading ends in `TransportNur` on both arches until
  the `bind_elf`/JSON side lands — the boot hook starts ELF modules only.
