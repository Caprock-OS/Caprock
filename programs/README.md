# Caprock — externe Programme (ext-26)

Dieser **eigene** Cargo-Workspace (getrennt vom Kernel) baut die **extern geladenen** Caprock-
Programme: statisch gelinkte `ET_EXEC`-ELF64-Binaries, die der generische Binary-Loader des Kernels
(ADR 0011) zur Laufzeit lädt — **nicht** Teil des Kernel-Images.

## Bauen

```sh
cd programs && cargo build --release
```

Erzeugt die ELFs unter `programs/build/target/aarch64-caprock-user/release/<name>.elf`. Eigene
Target-Spec (`aarch64-caprock-user.json`) + Linker-Skript (`user.ld`, festgelinkt an VA
`0x6000_0000`, getrennte **W^X**-`PT_LOAD`-Segmente) via `.cargo/config.toml`.

Die ELFs werden anschließend mit `tools/mkarchive.py` (im Repo-Root) in ein Boot-Archiv gelegt und
von QEMU `-device loader` in das reservierte RAM-Fenster geladen (siehe `test-qemu.sh`).

## Struktur

| Verzeichnis | Inhalt |
|---|---|
| `libcaprock/` | Minimal-SDK (Syscall-Stubs + Panik-Handler) — gemeinsame, **nicht** gegenseitige Abhängigkeit aller Programme. |
| `userland/` | UserLand-Programme (EL0-isoliert, **keine** Hardware-/Management-Rechte). |
| `hardware/` | HardwareLand-Programme (EL0-isoliert, Hardware-Caps + ein Trusted-Partner). |
| `trusted/` | TrustedSAS-Programme — **geladen EL0-isoliert** (nicht EL1), behalten aber die Trust-Stufe (dürfen PdControl/Loader-Caps halten). |

**Alle** geladenen Programme laufen **EL0-isoliert** (hardware-getrennte VSpace). Die Domäne legt
nur die **Cap-Autorität** fest (welche Cap-Typen die PD halten darf), nicht den Privilegienlevel.

## Ein Programm hinzufügen

1. `programs/<domäne>/<name>/` als Cargo-Bin anlegen (`[[bin]] name = "<name>"`).
2. `dependencies: libcaprock = { path = "../../libcaprock" }`.
3. Entry definieren. Empfohlen (und für TrustedSAS **Pflicht**, s. u.): eine **sichere**
   `fn run(arg: usize) -> !` + `libcaprock::entry!(run);`. Das Makro erzeugt die `_start`-Glue (das
   `#[no_mangle]`-Attribut ist in aktuellem Rust *unsafe*) in der auditierten SDK-Schicht, sodass das
   Programm selbst `#![forbid(unsafe_code)]` bleiben kann. (Der Kernel setzt SP, übergibt Boot-Info
   in `x0`.)
4. In `programs/Cargo.toml` als Member eintragen.
5. In `tools/mkarchive.py`-Aufruf (test-qemu.sh) als Archiv-Eintrag `id:name:domain:version:elf`
   (TrustedSAS zusätzlich `:manifest:cert` — siehe [`trusted/README.md`](trusted/README.md)).

## TrustedSAS-Programme: Zertifizierung

TrustedSAS-Binaries (`trusted/`) werden vom Kernel **nur mit gültigem Ed25519-Zertifikat** geladen
(ext-28, [ADR 0014](../docs/adr/0014-trusted-sas-certificates.md)). Sie müssen vollständig
`#![forbid(unsafe_code)]` sein (Allowlist `{libcaprock}`) und mit `tools/sign_trusted.py` signiert
werden. Ablauf + Schlüsselverwaltung: [`trusted/README.md`](trusted/README.md) +
[`docs/runbook-trusted-keys.md`](../docs/runbook-trusted-keys.md).
