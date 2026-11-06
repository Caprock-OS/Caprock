#!/usr/bin/env python3
"""Caprock — TrustedSAS-Root-Schluessel erzeugen + Kernel-Key-DB generieren (ext-28, ADR 0014).

Erzeugt ein Ed25519-Schluesselpaar. Der **private** Schluessel bleibt lokal unter `keys/` (gitignored,
NIE im Repo/Kernel). Der **oeffentliche** Schluessel + die Key-ID (= SHA-256(pubkey)[..16]) werden in
die in den Kernel kompilierte, read-only Key-DB `kernel/src/trusted_keys.rs` geschrieben. Die DB ist
NICHT per Syscall aenderbar -- Erweiterung/Rotation/Revocation nur durch erneutes Generieren +
Neukompilieren (= Firmware-/Kernel-Update).

Aufruf:  tools/gen_trusted_key.py [--name trusted-test] [--policy internal-test]
         tools/gen_trusted_key.py --regen-db        # nur trusted_keys.rs aus keys/*.pub neu schreiben
"""
import argparse
import hashlib
import os
import sys

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KEYS_DIR = os.path.join(ROOT, "keys")
DB_FILE = os.path.join(ROOT, "kernel", "src", "trusted_keys.rs")


def raw_priv(sk: Ed25519PrivateKey) -> bytes:
    return sk.private_bytes(
        serialization.Encoding.Raw,
        serialization.PrivateFormat.Raw,
        serialization.NoEncryption(),
    )


def raw_pub(sk: Ed25519PrivateKey) -> bytes:
    return sk.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )


def key_id(pub: bytes) -> bytes:
    return hashlib.sha256(pub).digest()[:16]


def gen(name: str) -> bool:
    """Erzeugt das Paar, falls es fehlt. Gibt zurueck, ob es ERZEUGT wurde."""
    os.makedirs(KEYS_DIR, exist_ok=True)
    priv_path = os.path.join(KEYS_DIR, f"{name}.ed25519")
    pub_path = os.path.join(KEYS_DIR, f"{name}.pub")
    if os.path.exists(priv_path):
        return False
    sk = Ed25519PrivateKey.generate()
    priv = raw_priv(sk)
    pub = raw_pub(sk)
    # Privaten Schluessel restriktiv schreiben (nur Besitzer).
    fd = os.open(priv_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(priv)
    with open(pub_path, "wb") as f:
        f.write(pub)
    print(f"erzeugt: {priv_path} (privat, gitignored) + {pub_path}")
    print(f"  key_id = {key_id(pub).hex()}")
    return True


def rust_bytes(b: bytes) -> str:
    return "[" + ", ".join(f"0x{x:02x}" for x in b) + "]"


def regen_db():
    """trusted_keys.rs aus allen keys/*.pub schreiben."""
    pubs = sorted(f for f in os.listdir(KEYS_DIR) if f.endswith(".pub")) if os.path.isdir(KEYS_DIR) else []
    if not pubs:
        sys.exit("keine keys/*.pub gefunden -- zuerst `gen_trusted_key.py` ausfuehren")
    entries = []
    for pf in pubs:
        name = pf[:-4]
        pub = open(os.path.join(KEYS_DIR, pf), "rb").read()
        if len(pub) != 32:
            sys.exit(f"{pf}: ungueltige PubKey-Laenge {len(pub)}")
        kid = key_id(pub)
        entries.append(
            f"    // {name}\n"
            f"    TrustedKey {{ key_id: {rust_bytes(kid)}, "
            f"pubkey: {rust_bytes(pub)}, revoked: false }},"
        )
    body = "\n".join(entries)
    src = (
        "//! AUTOGENERIERT von tools/gen_trusted_key.py -- NICHT von Hand editieren (ext-28, ADR 0014).\n"
        "//!\n"
        "//! Read-only TrustedSAS-Root-Key-DB: ausschliesslich OEFFENTLICHE Schluessel; in den Kernel\n"
        "//! kompiliert, NICHT per Syscall aenderbar. Erweiterung/Rotation/Revocation nur durch erneutes\n"
        "//! Generieren + Neukompilieren (Firmware-/Kernel-Update). Private Schluessel liegen NIE hier.\n"
        "\n"
        "use caprock_trust::TrustedKey;\n"
        "\n"
        "/// Akzeptierte oeffentliche Root-Schluessel (per `key_id` referenziert).\n"
        "pub static TRUSTED_KEYS: &[TrustedKey] = &[\n"
        f"{body}\n"
        "];\n"
        "\n"
        "/// Anti-Downgrade: minimal akzeptierte Version je `program_id` (firmware-gepflegt).\n"
        "/// Leer = keine Untergrenze (jede gueltig signierte Version wird akzeptiert).\n"
        "pub static MIN_VERSION: &[(u32, u32)] = &[];\n"
    )
    with open(DB_FILE, "w") as f:
        f.write(src)
    print(f"geschrieben: {DB_FILE} ({len(entries)} Schluessel)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", default="trusted-test")
    ap.add_argument("--regen-db", action="store_true", help="nur die DB neu schreiben")
    ap.add_argument("--ensure", action="store_true", help="Paar nur erzeugen, wenn es fehlt")
    args = ap.parse_args()
    if not args.regen_db:
        erzeugt = gen(args.name)
        if not erzeugt and not args.ensure:
            priv_path = os.path.join(KEYS_DIR, f"{args.name}.ed25519")
            sys.stderr.write(f"gen_trusted_key: {priv_path} existiert bereits -- nichts erzeugt\n")
    regen_db()


if __name__ == "__main__":
    main()
