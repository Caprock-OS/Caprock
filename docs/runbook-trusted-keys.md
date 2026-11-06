# Runbook — TrustedSAS-Schlüssel (Setup / Rotation / Revocation)

Bezug: [ADR 0014](adr/0014-trusted-sas-certificates.md), [ext-28-Bericht](phase-reports/ext-28-trusted-certificates-report.md).

**Grundprinzip:** Die TrustedSAS-Key-DB (`kernel/src/trusted_keys.rs`) enthält **ausschließlich
öffentliche** Schlüssel + `key_id` und ist **in den Kernel kompiliert**. Sie ist **nur** durch
erneutes Generieren + Neukompilieren (= **Firmware-/Kernel-Update**) änderbar — es gibt **keinen**
Syscall, der sie modifiziert. Private Schlüssel liegen **ausschließlich** lokal unter `keys/`
(per `.gitignore` ausgeschlossen) und gelangen **nie** ins Repo oder den Kernel.

> Die Test-Schlüsselverwahrung unter `keys/` ist eine Entwicklungs-Vereinfachung. In Produktion
> tritt an ihre Stelle ein HSM / Secure-Boot-Schlüsselspeicher; der Ablauf bleibt identisch.

## Werkzeuge

| Tool | Zweck |
|---|---|
| `tools/gen_trusted_key.py` | Ed25519-Keypair erzeugen + Key-DB (`trusted_keys.rs`) generieren |
| `tools/gen_manifest_key.py` | System-Manifest-Root-Schlüssel (A-1.3): Paar + `manifest_keys.rs` |
| `tools/dev-keys.sh` | **EIN** Bootstrap: sichert beide Paare (ensure) + Konsistenz-Prüfung, meldet Rebuild |
| `tools/sign_trusted.py` | Programm auditieren (Unsafe-Allowlist) + hashen + signieren → Zertifikat |

Voraussetzung: `python-cryptography` (Host). Kein RNG/Privatschlüssel im Kernel.

## A0. Standard-Bootstrap (alle Suiten)

Die Test-/Lade-Suiten rufen **eine** Stelle auf, statt die Schlüssel-Logik je Suite zu
duplizieren:

```sh
tools/dev-keys.sh          # rc 0 ok; rc 2 nicht entscheidbar
# stdout: DEVKEYS_REBUILD=1 → Kernel MUSS neu gebaut werden (frische Key-DB)
```

`dev-keys.sh` macht: ensure `gen_manifest_key.py --ensure` + TrustedSAS-Schlüssel mit
Übereinstimmungs-Prüfung (`check_trusted_key.py`), Beiseitelegen bei Mismatch, Neu-Erzeugen, und
meldet `DEVKEYS_REBUILD=1`, wenn sich eine Key-DB geändert hat. Sie gehört **vor** den Kernel-Build
(die DB wird in den Kernel kompiliert). Der manuelle Ablauf unten bleibt für Rotation/Revocation gültig.

## A. Erst-Setup (neuer Root-Schlüssel)

```sh
tools/gen_trusted_key.py --name <root-name>      # z. B. trusted-prod
```
- Schreibt `keys/<root-name>.ed25519` (privat, `chmod 0600`, gitignored) + `keys/<root-name>.pub`.
- Regeneriert `kernel/src/trusted_keys.rs` aus **allen** `keys/*.pub` (ein `TrustedKey`-Eintrag je
  Schlüssel; `key_id == SHA-256(pubkey)[..16]`).
- Den **privaten** Schlüssel sicher verwahren/sichern; er ist nicht wiederherstellbar.
- Kernel neu bauen (`./build.sh`) → der neue PubKey ist Teil der Vertrauensbasis.

`trust_audit()` prüft beim Laden u. a. `key_id == fingerprint(pubkey)` + Eindeutigkeit; ein
inkonsistenter DB-Eintrag fällt sofort auf (`loadhw : FAILURES`).

## B. Ein TrustedSAS-Programm signieren

```sh
tools/sign_trusted.py \
  --crate programs/trusted/<name> \
  --elf programs/build/target/aarch64-caprock-user/release/<name>.elf \
  --program-id <id> --version <v> --policy internal-test \
  --key keys/<root-name>.ed25519 \
  --out certs/<name>.cert
```
- `--program-id`/`--version` **müssen** zum Archiv-Eintrag passen (Identitäts-Bindung; sonst lehnt
  der Kernel ab).
- Schlägt der Unsafe-Audit fehl (Programm nicht `#![forbid(unsafe_code)]` oder `unsafe` außerhalb
  der Allowlist `{libcaprock}`), entsteht **kein** Zertifikat (Exit ≠ 0).
- Das Zertifikat ins Boot-Archiv legen: `mkarchive.py`-Spec `id:name:0:version:elf::certs/<name>.cert`
  (Domäne `0` = TrustedSAS; leeres Manifest-Feld → kein Manifest).

## C. Rotation (geplanter Schlüsselwechsel)

1. Neues Keypair erzeugen: `tools/gen_trusted_key.py --name <root-name>-v2`.
2. Alle aktiven TrustedSAS-Programme mit dem **neuen** Schlüssel **neu signieren** (Schritt B).
3. Optional: den alten Schlüssel als `revoked` markieren (Schritt D) — bis dahin akzeptiert die DB
   **beide** (überlappendes Rollout).
4. Kernel neu bauen + ausrollen (Firmware-Update). `keys/*.pub` beider Schlüssel bleiben in der DB,
   bis der alte revoziert/entfernt wird.

## D. Revocation (kompromittierter Schlüssel)

Sofort:
1. In `kernel/src/trusted_keys.rs` den betroffenen Eintrag auf `revoked: true` setzen
   **— oder —** `keys/<name>.pub` entfernen und `tools/gen_trusted_key.py --regen-db` ausführen
   (DB ohne den Schlüssel neu schreiben).
2. Kernel neu bauen + ausrollen (Firmware-Update).
3. Alle mit dem kompromittierten Schlüssel signierten Programme mit einem gültigen Schlüssel
   **neu signieren**, bevor/sobald sie wieder geladen werden.

Effekt: `verify_image` lehnt **jedes** Zertifikat dieser `key_id` ab (`key.revoked` bzw. Key nicht
mehr in der DB) → `LoaderError::Unverified`. Da die DB read-only + kompiliert ist, ist die Revocation
erst mit dem Kernel-Update wirksam — bewusst: die Vertrauensbasis ändert sich **nur** über den
Firmware-Pfad, nie zur Laufzeit.

## E. Anti-Downgrade

`MIN_VERSION: &[(program_id, min_version)]` in `trusted_keys.rs` (firmware-gepflegt, derzeit leer)
setzt je `program_id` eine Untergrenze. Nach dem Fix einer verwundbaren Version dort die neue
Mindestversion eintragen → der Kernel lehnt ältere (auch korrekt signierte) Zertifikate ab.
