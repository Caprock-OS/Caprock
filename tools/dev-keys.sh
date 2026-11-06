#!/usr/bin/env bash
# tools/dev-keys.sh — das EINE Key-Bootstrap fuer alle signierenden Suiten.
#
# Stellt beide Schluesselpaare sicher (System-Manifest + TrustedSAS) und haelt sie
# konsistent zu den in den Kernel kompilierten Key-DBs (`kernel/src/{manifest,trusted}_keys.rs`).
# Geprueft wird die UEBEREINSTIMMUNG, nicht nur die Existenz: ein privater Schluessel, der
# nicht zum eingebetteten oeffentlichen passt, wird beiseitegelegt und neu erzeugt.
#
# Ruft idempotent beide Generatoren auf (`gen_manifest_key.py --ensure` bzw.
# `gen_trusted_key.py --ensure`) — Vorhandene Schluessel bleiben unangetastet.
#
# Rueckgabe / Ausgabe:
#   Exit 0  Erfolg (Schluessel sichergestellt).
#   Exit 2  nicht entscheidbar (z. B. cryptography fehlt) — niemals 0.
#   stdout: DEVKEYS_REBUILD=1|0 — 1, wenn sich eine Key-DB geaendert hat
#           (Kernel MUSS neu gebaut werden, damit sie eingebettet ist).
# Optionen: --quiet (nur der Marker), --selbsttest (Idempotenz-/Selbstcheck).
set -uo pipefail
cd "$(dirname "$0")/.."

MANIFEST_DB=kernel/src/manifest_keys.rs
TRUSTED_DB=kernel/src/trusted_keys.rs
TRUSTKEY=keys/trusted-test.ed25519

QUIET=0
[ "${1-}" = "--quiet" ] && QUIET=1

say() { [ "$QUIET" -eq 0 ] && echo "$*"; }

if [ "${1-}" = "--selbsttest" ]; then
    bash -n "$0" || { echo "SELBSTTEST: FEHLER: Syntax"; exit 1; }
    command -v python3 >/dev/null || { echo "SELBSTTEST: FEHLER: python3 fehlt"; exit 1; }
    for w in tools/gen_manifest_key.py tools/gen_trusted_key.py tools/check_trusted_key.py; do
        [ -e "$w" ] || { echo "SELBSTTEST: FEHLER: $w fehlt"; exit 1; }
    done
    python3 -c "import cryptography" 2>/dev/null \
        || { echo "SELBSTTEST: FEHLER: python3-cryptography fehlt"; exit 1; }
    # Idempotenz: erster Lauf sichert (rc 0); zweiter Lauf darf KEINEN Rebuild mehr melden.
    FIRST="$( "$0" )" || { echo "SELBSTTEST: FEHLER: erster Lauf rc!=0"; exit 1; }
    SECOND="$( "$0" )" || { echo "SELBSTTEST: FEHLER: zweiter Lauf rc!=0"; exit 1; }
    case "$SECOND" in
        *DEVKEYS_REBUILD=1*) echo "SELBSTTEST: FAILURES (zweiter Lauf meldet Rebuild)"; exit 1 ;;
    esac
    echo "SELBSTTEST: ALL PASS"
    exit 0
fi

sha() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }

python3 -c "import cryptography" 2>/dev/null || {
    echo "dev-keys: FEHLT das Python-Paket 'cryptography' (signieren geht ohne nicht)" >&2
    exit 2
}

BEFORE_MAN="$(sha "$MANIFEST_DB")"
BEFORE_TRU="$(sha "$TRUSTED_DB")"

# --- System-Manifest-Schluessel ---------------------------------------------------
say "== Manifest-Schluessel =="
python3 tools/gen_manifest_key.py --ensure || {
    echo "dev-keys: gen_manifest_key.py FEHLGESCHLAGEN" >&2; exit 2; }

# --- TrustedSAS-Schluessel (mit Uebereinstimmungs-Pruefung) ----------------------
say "== TrustedSAS-Schluessel =="
python3 tools/check_trusted_key.py
rc=$?
if [ "$rc" -eq 2 ]; then
    echo "dev-keys: TrustedSAS-Schluessel nicht pruefbar (nicht entscheidbar)" >&2
    exit 2
fi
if [ "$rc" -ne 0 ]; then
    if [ -f "$TRUSTKEY" ]; then
        BEISEITE="$TRUSTKEY.passt-nicht-$(date +%Y%m%d-%H%M%S)"
        mv "$TRUSTKEY" "$BEISEITE"
        [ -f "$TRUSTKEY.pub" ] && mv "$TRUSTKEY.pub" "$BEISEITE.pub"
        say "  (alter Schluessel beiseitegelegt: $BEISEITE)"
    fi
    python3 tools/gen_trusted_key.py --name trusted-test --ensure >/dev/null 2>&1 || {
        echo "dev-keys: gen_trusted_key.py FEHLGESCHLAGEN" >&2; exit 2; }
    python3 tools/check_trusted_key.py || {
        echo "dev-keys: auch nach Neuerzeugen passt der Schluessel nicht" >&2; exit 2; }
fi

# --- Rebuild-Marker ---------------------------------------------------------------
AFTER_MAN="$(sha "$MANIFEST_DB")"
AFTER_TRU="$(sha "$TRUSTED_DB")"
if [ "$BEFORE_MAN" != "$AFTER_MAN" ] || [ "$BEFORE_TRU" != "$AFTER_TRU" ]; then
    echo "DEVKEYS_REBUILD=1"
else
    echo "DEVKEYS_REBUILD=0"
fi
exit 0
