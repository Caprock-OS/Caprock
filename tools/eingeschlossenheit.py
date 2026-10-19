#!/usr/bin/env python3
"""**Die Eintrittskarte ist nicht „bewiesen richtig", sondern „kann nicht ausbrechen".** (Z28)

================================================================================================
WARUM ES DAS GIBT
================================================================================================

Z28 entscheidet, dass das Syscall-ABI ein Userland-Begriff wird: ein Handler-Modul bearbeitet die
Syscalls eines Gastes, und die Bedingung fuer seine Zulassung ist **Eingeschlossenheit**, nicht
Korrektheit. Der Grund steht in `TODO.md` Z28 und ist im Projekt schon zweimal bezahlt worden:
`caprock-part` und `caprock-fat` sind abhaengigkeitsfrei und `forbid(unsafe_code)`, damit fremde
Plattenbytes nirgends mit Kernprivileg interpretiert werden. Ein falsch gelesener FAT-Eintrag
liefert Unsinn; er wird nicht zu Codeausfuehrung.

**Dieser Waechter ist der ERSTE Schritt, nicht das erste Modul.** Ein Modul, das vor ihm entsteht,
waere ungeprueft und muesste nachtraeglich eingesammelt werden -- und „nachtraeglich einsammeln"
heisst in diesem Projekt regelmaessig „gar nicht".

Geprueft wird, was mechanisch pruefbar ist:

  1. `#![forbid(unsafe_code)]` -- nicht `deny`, nicht ein Kommentar. In sicherem Rust gibt es
     keinen Weg von Bytes zu einem Funktionszeiger; dafuer braucht es `transmute`. Damit ist
     „fuehrt keine fremde Logik aus" mechanisch abgedeckt, ohne einen einzigen Beweis.
  2. **Nur Abhaengigkeiten aus einer benannten Menge.** Eine Crate, die beliebig abhaengen darf,
     holt sich `unsafe` durch die Hintertuer -- `forbid(unsafe_code)` ist eine Eigenschaft EINER
     Crate, Eingeschlossenheit eine des transitiven Schlusses. `caprock-cap` ist der lebende
     Beleg: sie traegt `forbid` und linkt `caprock-slab`, dessen ganzer Zweck `unsafe` ist.
     Erlaubt ist, was **selbst eingeschlossen** ist (dann traegt die Induktion ueber die ganze
     Menge) oder namentlich in `ERLAUBTE_ABHAENGIGKEITEN` steht -- und zwar **als
     Pfad-Abhaengigkeit**: ein `caprock-part = "1.0"` aus einer Registry traegt denselben Namen
     und ist ueber den Namen allein nicht davon zu unterscheiden.
  3. Kein **Bauskript**. Ein `build.rs` laeuft zur Bauzeit auf dem Wirt mit voller Autoritaet und
     wird von `forbid(unsafe_code)` nicht im Geringsten beruehrt. (Diese Zeile steht hier, weil
     ausgerechnet `libcaprock` -- die einzige erlaubte unsafe-Abhaengigkeit -- eines hat.)
  4. Kein `[lints]`, das `unsafe_code` wieder aufweicht.
  5. Die Crate steht in einem **Workspace**. Eine Crate, die nie gebaut wird, hat kein
     `forbid` -- sie hat eine Behauptung. („Ein Test, der nirgends laeuft, ist kein Test.")

================================================================================================
DER MECHANISMUS -- WELCHE CRATES GEMEINT SIND, und warum ausgerechnet so
================================================================================================

Die Zugehoerigkeit steht in der Crate selbst:

    [package.metadata.caprock]
    einschluss = "streng"

Drei Alternativen standen zur Wahl; die Begruendung gehoert hierher, weil sie beim naechsten Umbau
sonst neu erfunden wird:

  * **Eine Liste in diesem Skript** waere eine Textflaeche. Der Identitaets-Waechter hat genau das
    einen Tag lang gehabt: `vspace_map_dma` stand nicht darin, also sah er den DMA-Pfad nie.
    Eine Liste, die neben der Sache herlaeuft, ist schlimmer als keine.
  * **Ein Verzeichnis** (`crates/handler/*`) entschiede nach dem ORT. Handler-Module werden an
    mindestens zwei Orten liegen -- Bibliotheken unter `crates/`, Persoenlichkeits-PDs unter
    `programs/` --, und `programs/` ist ein eigener Workspace. Ein Ort, der die halbe Wahrheit
    traegt, ist die naechste Luecke.
  * **Das Manifest** wandert mit der Crate mit: wer sie verschiebt oder umbenennt, kann die
    Zusage nicht versehentlich vom Code trennen. Und `cargo metadata` gibt `metadata.caprock`
    woertlich heraus -- die Zusage ist damit auch fuer das Zertifikatswerkzeug (ADR 0014) und
    fuer eine spaetere Registertabelle lesbar, ohne dieses Skript zu fragen.

**Der Wert ist eine geschlossene Menge: genau `"streng"`.** Ein zweiter, weicherer Wert waere die
Auffanggruppe, die Z28 ausdruecklich verbietet -- sie erbte die Vereinigung aller Autoritaeten,
und die Klassifikation waere weg, ohne dass es jemand merkt.

**Kein `rolle`-Feld.** Es waere die naheliegende Erweiterung („grenzparser", „syscall-handler")
und wird bewusst nicht gebaut: ein Feld, das nie eingeloest wird, sammelt ungepruefte Werte an,
und der Tag der Einloesung ist der Tag, an dem sie alle falsch sind -- gemessen an den
Manifest-Prioritaeten (3/1/2/2/2), die jahrelang Platzhalter waren und beim Einloesen die
Lade-Suite rissen. Wenn Handler-Module eine Abstufung brauchen, bekommen sie sie an dem Tag, an
dem sie eine Wirkung hat.

================================================================================================
DIE OPT-IN-LUECKE -- und was dagegen steht
================================================================================================

Eine Zusage, die man weglassen kann, laesst sich weglassen. Dagegen steht die **Umkehrung**:

    Wer `#![forbid(unsafe_code)]` schreibt, macht eine Aussage. Diese Aussage ist entweder eine
    Eintrittskarte -- dann ist sie deklariert und wird transitiv geprueft -- oder sie ist
    ausdruecklich KEINE; dann steht sie mit Grund in `AUSDRUECKLICH_NICHT`. Ein Drittes gibt es
    nicht.

Das ist Z28s „keine Auffanggruppe, sondern eine benannte Absage", eine Ebene tiefer angewandt.
Es schliesst die Luecke nicht ganz -- eine nagelneue Crate mit `unsafe` und ohne Marke faellt
durch beide Raster --, aber es macht den lautlosen Fall unmoeglich: eine Crate, die die
Eigenschaft HAT, muss sagen, ob sie sie als Zusage meint.

**Was dieser Waechter ausdruecklich NICHT weiss:** welche Module das Umleitungs-Primitiv
(Z26/A3) tatsaechlich registriert. Eine Registertabelle gibt es heute nicht. Sobald es sie gibt,
gehoert sie hier als zweite Quelle herein -- und dann ist die Frage „ist jedes registrierte Modul
deklariert?" beantwortbar, die heute strukturell offen ist.

================================================================================================
DIE RATSCHEN -- Mengen von NAMEN, nicht Zahlen
================================================================================================

`IDENTITY_DEBTS` war einmal ein `usize`. Eine Ratsche ueber einer Zahl greift gegen Zuwachs, aber
nicht gegen **Austausch** -- und Austausch fuehlt sich beim Umbauen wie Fortschritt an. Beide
Mengen hier sind deshalb Namen mit Grund, und beide werden **gegen die Wirklichkeit gehalten**:
ein Name, den es nicht mehr braucht, ist ein Befund. Sonst bliebe ein totes Zugestaendnis stehen,
gegen das man spaeter lautlos etwas anderes eintauscht.

Rueckgabe: 0 = eingeschlossen · 1 = Befund · 2 = Werkzeugfehler · 3 = KEINE Kandidaten.

Der Code 3 ist kein Schoenheitsfehler, sondern die Lehre aus jedem leeren Pruefer dieses
Projekts: ein Waechter ueber null Crates meldet Entwarnung ueber nichts. Er schweigt dann nicht,
er faellt durch.

Aufruf:
  tools/eingeschlossenheit.py                # pruefen + Selbsttest
  tools/eingeschlossenheit.py --nur-pruefen   # ohne Selbsttest (der Selbsttest ruft sich damit)
  tools/eingeschlossenheit.py --liste         # Bestandsaufnahme aller Crates im Baum
  tools/eingeschlossenheit.py --wurzel DIR    # gegen einen anderen Baum (der Selbsttest tut das)
"""

import os
import re
import shutil
import sys
import tempfile

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover -- Python < 3.11
    sys.exit("FEHLER: dieser Waechter braucht `tomllib` (Python >= 3.11).")

WURZEL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# ------------------------------------------------------------------------------------------------
# Der Mechanismus. Diese drei Namen sind die Schnittstelle nach aussen -- der Selbsttest baut seine
# Attrappen DAMIT, nicht mit Literalen. Wer den Marker umbenennt und den Selbsttest vergisst,
# bekommt keinen stummen Waechter, sondern einen lauten Fehlschlag.
# ------------------------------------------------------------------------------------------------
MARKER_TABELLE = ("package", "metadata", "caprock")
MARKER_SCHLUESSEL = "einschluss"
MARKER_WERTE = {"streng"}  # geschlossen: kein zweiter, weicherer Wert (keine Auffanggruppe)

# ------------------------------------------------------------------------------------------------
# RATSCHE 1 -- Abhaengigkeiten, die selbst NICHT eingeschlossen sind und trotzdem erlaubt bleiben.
#
# Die Menge darf nur fallen. Jeder Eintrag traegt einen Grund, und jeder Eintrag muss tatsaechlich
# gebraucht werden (sonst steht hier ein totes Zugestaendnis, gegen das sich spaeter lautlos etwas
# anderes eintauschen laesst).
#
# **Diese Menge ist dieselbe wie die Allowlist des Zertifikatswerkzeugs** (ADR 0014,
# `tools/sign_trusted.py`). Zwei Definitionen derselben Regel sind ein Riss -- dieses Projekt hat
# ihn schon bezahlt, als zwei Suiten dasselbe Geraet verschieden aufsetzten. Der Waechter haelt
# die beiden deshalb gegeneinander (s. `pruefe_kopplung_adr0014`).
# ------------------------------------------------------------------------------------------------
ERLAUBTE_ABHAENGIGKEITEN = {
    "libcaprock": (
        "die auditierte Syscall-ABI (SVC-Stubs, Panik-Handler, `entry!`). Sie MUSS `unsafe` "
        "enthalten -- ein Syscall ist eine Instruktion, kein Funktionsaufruf -- und sie ist "
        "genau deshalb die einzige Crate der ADR-0014-Allowlist. Sie traegt ausserdem ein "
        "Bauskript und faellt damit gleich zweifach nicht unter `forbid`: der Eintrag ist eine "
        "Schuld, kein Entwurf."
    ),
}

# ------------------------------------------------------------------------------------------------
# RATSCHE 2 -- die benannte ABSAGE. Crates, die `forbid(unsafe_code)` tragen und trotzdem KEINE
# Eintrittskarte sind. Ohne diese Menge waere die Umkehrung oben eine Gaengelei; mit ihr ist sie
# eine Entscheidung, die jemand hingeschrieben hat.
# ------------------------------------------------------------------------------------------------
AUSDRUECKLICH_NICHT = {
    "caprock-cap": (
        "traegt `forbid(unsafe_code)` fuer den EIGENEN Code -- und linkt `caprock-slab`, dessen "
        "Zweck `unsafe` ist (Rohspeicher an Typen binden). Das ist genau der Grund, warum "
        "Eingeschlossenheit eine Eigenschaft des transitiven Schlusses ist und nicht des "
        "Crate-Kopfes. Sie ist auch keine Handler-Kandidatin: sie IST die Cap-Verwaltung."
    ),
}

# ------------------------------------------------------------------------------------------------
# Erkennung im Quelltext
# ------------------------------------------------------------------------------------------------
# **Zeilenanfangs verankert und ohne Kommentare** -- beides absichtlich. `tools/sign_trusted.py`
# sucht das Attribut mit einem freien `re.search` ueber den ROHEN Dateiinhalt; damit erfuellt schon
# die Erwaehnung `#![forbid(unsafe_code)]` in einem Doku-Kommentar die Bedingung. In
# `crates/caprock-loader/src/lib.rs` steht genau so eine Erwaehnung (Zeile 3) -- dort zufaellig
# zusaetzlich zum echten Attribut. Ein Waechter, der Prosa mitzaehlt, meldet Erfolg, sobald jemand
# ihn erklaert.
RX_FORBID = re.compile(r"^\s*#!\[\s*forbid\s*\(\s*unsafe_code\s*\)\s*\]", re.M)
RX_DENY = re.compile(r"^\s*#!\[\s*deny\s*\(\s*unsafe_code\s*\)\s*\]", re.M)
# `\bunsafe\b\s*` statt `\bunsafe\s+`: `unsafe{` ohne Leerzeichen ist gueltiges Rust und rutscht
# durch die zweite Fassung. Das `\b` hinter `unsafe` verhindert, dass `unsafefn` mitzaehlt.
RX_UNSAFE = re.compile(r"\bunsafe\b\s*(fn|impl|trait|extern|\{)")


def entkommentiert(text: str) -> str:
    """Block- und Zeilenkommentare weg. String-Literale bleiben stehen -- ein `unsafe fn` in einem
    String waere ein Fehlalarm, und ein Fehlalarm ist laut. Ein uebersehenes `unsafe` waere still."""
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


class Crate:
    def __init__(self, wurzel, manifest_pfad):
        self.manifest_pfad = manifest_pfad
        self.dir = os.path.dirname(manifest_pfad)
        self.rel = os.path.relpath(self.dir, wurzel)
        with open(manifest_pfad, "rb") as f:
            self.toml = tomllib.load(f)
        pkg = self.toml.get("package", {})
        self.name = pkg.get("name", "<ohne Namen>")
        self.hat_bauskript = os.path.exists(os.path.join(self.dir, "build.rs")) or "build" in pkg
        # Marke
        t = self.toml
        for schluessel in MARKER_TABELLE:
            t = t.get(schluessel, {}) if isinstance(t, dict) else {}
        self.marke = t.get(MARKER_SCHLUESSEL) if isinstance(t, dict) else None
        # Abhaengigkeiten, inkl. dev/build und target-spezifischer Tabellen. Die Angabe wird
        # MITGENOMMEN, nicht nur der Name: eine Registry-Abhaengigkeit `caprock-part = "1.0"`
        # traegt denselben Namen wie unsere Crate und waere ueber den Namen allein nicht davon zu
        # unterscheiden -- die klassische Namensverwechslung in der Lieferkette.
        self.dep_angaben = {}
        for tabelle in ("dependencies", "dev-dependencies", "build-dependencies"):
            self.dep_angaben.update(self.toml.get(tabelle, {}))
        for ziel in self.toml.get("target", {}).values():
            for tabelle in ("dependencies", "dev-dependencies", "build-dependencies"):
                self.dep_angaben.update(ziel.get(tabelle, {}))
        self.deps = set(self.dep_angaben)
        # Wurzelmodule: was rustc als Crate-Wurzel sieht. Nur dort wirkt ein inneres Attribut.
        self.wurzelmodule = []
        lib = self.toml.get("lib", {})
        if "path" in lib:
            self.wurzelmodule.append(os.path.join(self.dir, lib["path"]))
        elif os.path.exists(os.path.join(self.dir, "src/lib.rs")):
            self.wurzelmodule.append(os.path.join(self.dir, "src/lib.rs"))
        bins = self.toml.get("bin", [])
        if bins:
            for b in bins:
                if "path" in b:
                    self.wurzelmodule.append(os.path.join(self.dir, b["path"]))
                elif os.path.exists(os.path.join(self.dir, "src/main.rs")):
                    self.wurzelmodule.append(os.path.join(self.dir, "src/main.rs"))
        elif os.path.exists(os.path.join(self.dir, "src/main.rs")):
            self.wurzelmodule.append(os.path.join(self.dir, "src/main.rs"))
        # **Nur, was es wirklich gibt.** `Verification/concurrency/loom` nennt in `[lib] path` eine
        # Datei, die erst `tools/loom-verify.sh` erzeugt. Ein Waechter, der daran mit einer
        # Ausnahme abstuerzt, ist unbrauchbar; ein Kandidat ohne vorhandenes Wurzelmodul faellt
        # dagegen als `kein-wurzelmodul` durch -- genau richtig, denn dort kann kein Attribut wirken.
        self.wurzelmodule = sorted({p for p in self.wurzelmodule if os.path.exists(p)})
        # Quelltext
        self.quellen = []
        for r, verz, dateien in os.walk(os.path.join(self.dir, "src")):
            verz[:] = [d for d in verz if d not in ("target", ".git")]
            for fn in dateien:
                if fn.endswith(".rs"):
                    self.quellen.append(os.path.join(r, fn))
        self.quellen.sort()

    def ist_pfad_abhaengigkeit(self, name):
        angabe = self.dep_angaben.get(name)
        return isinstance(angabe, dict) and "path" in angabe

    def _lies(self, pfad):
        with open(pfad, encoding="utf-8", errors="replace") as f:
            return entkommentiert(f.read())

    @property
    def forbid(self):
        """Jedes Wurzelmodul traegt das Attribut. Ein Crate mit lib.rs UND main.rs braucht beide --
        das Attribut gilt je Crate-Wurzel, nicht je Verzeichnis."""
        if not self.wurzelmodule:
            return False
        return all(RX_FORBID.search(self._lies(p)) for p in self.wurzelmodule)

    @property
    def deny_statt_forbid(self):
        return any(
            RX_DENY.search(self._lies(p)) and not RX_FORBID.search(self._lies(p))
            for p in self.wurzelmodule
        )

    def unsafe_stellen(self):
        treffer = []
        for p in self.quellen:
            n = len(RX_UNSAFE.findall(self._lies(p)))
            if n:
                treffer.append((p, n))
        return treffer

    def lints_lockern(self):
        for tabelle in (self.toml.get("lints", {}) or {}).values():
            if not isinstance(tabelle, dict):
                continue
            wert = tabelle.get("unsafe_code")
            if wert is None:
                continue
            stufe = wert.get("level") if isinstance(wert, dict) else wert
            if stufe != "forbid":
                return str(stufe)
        return None


class Befund:
    def __init__(self, code, crate, text):
        self.code, self.crate, self.text = code, crate, text

    def __str__(self):
        return f"[{self.code}] {self.crate}: {self.text}"


def crates_finden(wurzel):
    gefunden = []
    for r, verz, dateien in os.walk(wurzel):
        # **`.claude` gehoert dazu, und der Grund ist gemessen.** Dort liegen die Arbeitsbaeume
        # der Agenten -- eigenstaendige Checkouts, oft auf ALTEN Commits. Beim ersten Lauf hat der
        # Selbsttest deshalb `sel4lake-cap` und `sel4lake-loader` gefunden: Crates aus der Zeit vor
        # der Umbenennung nach Caprock, in einem Baum, der gar nicht dieser Baum ist. Ein Waechter,
        # der in fremde Checkouts hineinsieht, urteilt ueber einen anderen Stand als den, der
        # gebaut wird -- dieselbe Klasse wie eine Messung, die nicht weiss, welches Artefakt sie
        # gemessen hat.
        verz[:] = [
            d
            for d in verz
            if d not in ("target", ".git", "build", "node_modules", ".claude")
        ]
        if "Cargo.toml" in dateien:
            pfad = os.path.join(r, "Cargo.toml")
            try:
                c = Crate(wurzel, pfad)
            except Exception as e:  # pragma: no cover
                raise SystemExit(f"FEHLER: {pfad} nicht lesbar: {e}")
            if "package" in c.toml:
                gefunden.append(c)
    gefunden.sort(key=lambda c: c.name)
    return gefunden


def workspace_mitglieder(wurzel):
    """Jedes Verzeichnis, das in irgendeinem Workspace dieses Baums als Mitglied steht."""
    import glob as globmod

    mitglieder = set()
    for r, verz, dateien in os.walk(wurzel):
        verz[:] = [d for d in verz if d not in ("target", ".git", "build")]
        if "Cargo.toml" not in dateien:
            continue
        try:
            with open(os.path.join(r, "Cargo.toml"), "rb") as f:
                t = tomllib.load(f)
        except Exception:
            continue
        for m in t.get("workspace", {}).get("members", []):
            for treffer in globmod.glob(os.path.join(r, m)):
                mitglieder.add(os.path.abspath(treffer))
    return mitglieder


def pruefe_kopplung_adr0014(wurzel):
    """Die Allowlist des Zertifikatswerkzeugs MUSS dieselbe Menge sein wie hier.

    Ein Anker, der ins Leere liest, ist keiner: findet die Zeile sich nicht, ist DAS der Befund --
    nicht Schweigen."""
    pfad = os.path.join(wurzel, "tools", "sign_trusted.py")
    if not os.path.exists(pfad):
        return Befund("kopplung-anker", "tools/sign_trusted.py",
                      "das Zertifikatswerkzeug (ADR 0014) fehlt -- die Kopplungspruefung liest "
                      "ins Leere. Wurde es verschoben, gehoert der Pfad hier nachgezogen.")
    with open(pfad, encoding="utf-8") as f:
        text = f.read()
    m = re.search(r"^ALLOWLIST\s*=\s*\{([^}]*)\}", text, re.M)
    if not m:
        return Befund("kopplung-anker", "tools/sign_trusted.py",
                      "die Zeile `ALLOWLIST = {...}` gibt es nicht mehr -- der Anker dieser "
                      "Pruefung ist weg, also urteilt sie ueber nichts.")
    dort = set(re.findall(r'"([^"]+)"', m.group(1)))
    hier = set(ERLAUBTE_ABHAENGIGKEITEN)
    if dort != hier:
        return Befund(
            "kopplung-adr0014", "tools/sign_trusted.py",
            f"die Allowlist des Zertifikatswerkzeugs ist {sorted(dort)}, die erlaubten "
            f"Abhaengigkeiten hier sind {sorted(hier)}. Zwei Definitionen derselben Regel sind "
            "ein Riss; wenn sie auseinandergehen SOLLEN, gehoert der Grund hierher.")
    return None


def pruefen(wurzel):
    """Gibt (befunde, kandidaten, alle, anzahl_pruefungen) zurueck. Wirft nichts."""
    befunde = []
    alle = crates_finden(wurzel)
    mitglieder = workspace_mitglieder(wurzel)
    n = 0

    # Marken einsammeln -- und ungueltige Werte sofort melden (geschlossene Wertemenge).
    kandidaten = []
    for c in alle:
        if c.marke is None:
            continue
        if c.marke not in MARKER_WERTE:
            befunde.append(Befund(
                "unbekannter-wert", c.name,
                f"`{MARKER_SCHLUESSEL} = {c.marke!r}` -- erlaubt ist genau {sorted(MARKER_WERTE)}. "
                "Ein zweiter, weicherer Wert waere die Auffanggruppe, die Z28 ausschliesst: sie "
                "erbte die Vereinigung aller Autoritaeten."))
            continue
        kandidaten.append(c)
    namen_kandidaten = {c.name for c in kandidaten}
    namen_lokal = {c.name for c in alle}

    if not kandidaten:
        # Kein Befund, sondern ein eigener Ausgang: s. Rueckgabecode 3.
        return befunde, kandidaten, alle, n

    for c in kandidaten:
        # (1) forbid
        n += 1
        if not c.wurzelmodule:
            befunde.append(Befund("kein-wurzelmodul", c.name,
                                  "kein Wurzelmodul gefunden (weder `src/lib.rs`/`src/main.rs` "
                                  "noch ein `path` in `[lib]`/`[[bin]]`) -- der Waechter kann "
                                  "nicht sagen, wo ein inneres Attribut ueberhaupt wirkte."))
        elif not c.forbid:
            if c.deny_statt_forbid:
                befunde.append(Befund(
                    "nur-deny", c.name,
                    "traegt `#![deny(unsafe_code)]` statt `forbid`. `deny` laesst sich in jedem "
                    "Modul mit `#[allow(unsafe_code)]` zuruecknehmen -- die Eigenschaft haengt "
                    "dann an der Disziplin des naechsten Beitrags. `forbid` laesst sich nicht "
                    "zuruecknehmen; genau darin besteht der Unterschied."))
            else:
                befunde.append(Befund(
                    "kein-forbid", c.name,
                    "traegt kein `#![forbid(unsafe_code)]` im Wurzelmodul ("
                    + ", ".join(os.path.relpath(p, wurzel) for p in c.wurzelmodule) + ")."))
        # (2) unsafe im Quelltext -- der Compiler faengt es ohnehin; dieser Waechter muss aber
        #     auch dann sprechfaehig sein, wenn niemand baut.
        n += 1
        stellen = c.unsafe_stellen()
        if stellen:
            befunde.append(Befund(
                "unsafe-im-quelltext", c.name,
                "enthaelt `unsafe`: " + ", ".join(
                    f"{os.path.relpath(p, wurzel)} ({k}x)" for p, k in stellen)))
        # (3) Bauskript
        n += 1
        if c.hat_bauskript:
            befunde.append(Befund(
                "bauskript", c.name,
                "hat ein Bauskript (`build.rs`). Es laeuft zur Bauzeit auf dem WIRT, mit voller "
                "Autoritaet, und `forbid(unsafe_code)` beruehrt es nicht einmal. Ein "
                "eingeschlossenes Modul hat keins."))
        # (4) Lints
        n += 1
        stufe = c.lints_lockern()
        if stufe is not None:
            befunde.append(Befund(
                "lints-lockern", c.name,
                f"setzt `unsafe_code` in `[lints]` auf `{stufe}` -- eine Hintertuer neben dem "
                "Attribut."))
        # (5) Abhaengigkeiten
        n += 1
        fremd, verwechselbar = [], []
        for d in sorted(c.deps):
            if d not in namen_kandidaten and d not in ERLAUBTE_ABHAENGIGKEITEN:
                fremd.append(d)
                continue
            # **„Heisst wie ein eingeschlossenes Modul" ist nicht „ist eines".** Ohne die
            # Pfadbedingung genuegte `caprock-part = "1.0"` aus einer Registry, um die
            # Transitivitaet vorzutaeuschen -- die klassische Namensverwechslung in der
            # Lieferkette, und sie sieht im Manifest voellig harmlos aus. Die Bedingung gilt fuer
            # BEIDE Zweige: auch der Allowlist-Eintrag ist ein Name aus diesem Baum.
            if d in namen_lokal and not c.ist_pfad_abhaengigkeit(d):
                verwechselbar.append(d)
        if fremd:
            befunde.append(Befund(
                "fremde-abhaengigkeit", c.name,
                f"haengt von {fremd} ab -- weder selbst eingeschlossen noch in "
                "`ERLAUBTE_ABHAENGIGKEITEN` benannt. Eine Crate, die beliebig abhaengen darf, "
                "holt sich `unsafe` durch die Hintertuer."))
        if verwechselbar:
            befunde.append(Befund(
                "namensverwechslung", c.name,
                f"{verwechselbar} traegt den Namen eines eingeschlossenen Moduls, ist aber KEINE "
                "Pfad-Abhaengigkeit. Der Name allein sagt nichts darueber, welcher Quelltext "
                "gelinkt wird."))
        # (6) wird ueberhaupt gebaut?
        n += 1
        if os.path.abspath(c.dir) not in mitglieder:
            befunde.append(Befund(
                "nicht-im-workspace", c.name,
                f"steht in keinem Workspace dieses Baums ({c.rel}) -- sie wird nie uebersetzt, "
                "und dann ist `forbid` eine Behauptung statt eines Compilerfehlers."))

    # (7) Ratsche 1: kein totes Zugestaendnis.
    n += 1
    gebraucht = set()
    for c in kandidaten:
        gebraucht |= c.deps
    veraltet = sorted(set(ERLAUBTE_ABHAENGIGKEITEN) - gebraucht)
    if veraltet:
        befunde.append(Befund(
            "ratsche-veraltet", "ERLAUBTE_ABHAENGIGKEITEN",
            f"{veraltet} steht in der Menge und wird von keinem eingeschlossenen Modul "
            "gebraucht. Ein totes Zugestaendnis ist der Platz, gegen den spaeter lautlos etwas "
            "anderes eingetauscht wird -- die Ratsche darf nur fallen."))

    # (8) Die Umkehrung: forbid ohne Marke.
    n += 1
    unbezeichnet = []
    for c in alle:
        if c.name in namen_kandidaten or not c.forbid:
            continue
        if c.name in AUSDRUECKLICH_NICHT:
            continue
        unbezeichnet.append(c.name)
    if unbezeichnet:
        befunde.append(Befund(
            "unbezeichnet", ", ".join(sorted(unbezeichnet)),
            "traegt `#![forbid(unsafe_code)]`, ist aber weder als eingeschlossen deklariert noch "
            "in `AUSDRUECKLICH_NICHT` abgesagt. Beides ist erlaubt, keins von beidem ist "
            "stillschweigend: sonst laesst sich die Marke bei einem Handler-Modul einfach "
            "weglassen."))

    # (9) Ratsche 2: die Absage muss noch stimmen.
    n += 1
    namen_alle = {c.name: c for c in alle}
    schlecht = []
    for name in sorted(AUSDRUECKLICH_NICHT):
        c = namen_alle.get(name)
        if c is None:
            schlecht.append(f"{name} (gibt es nicht mehr)")
        elif name in namen_kandidaten:
            schlecht.append(f"{name} (ist inzwischen deklariert -- die Absage widerspricht ihr)")
        elif not c.forbid:
            schlecht.append(f"{name} (traegt kein `forbid` mehr -- die Absage laeuft ins Leere)")
    if schlecht:
        befunde.append(Befund(
            "absage-veraltet", "AUSDRUECKLICH_NICHT",
            "; ".join(schlecht) + ". Ein Eintrag, dessen Anker weg ist, urteilt ueber nichts."))

    # (10) Kopplung an ADR 0014.
    n += 1
    b = pruefe_kopplung_adr0014(wurzel)
    if b:
        befunde.append(b)

    return befunde, kandidaten, alle, n


# ------------------------------------------------------------------------------------------------
# Selbsttest
# ------------------------------------------------------------------------------------------------
# **In beide Richtungen, an einer KOPIE.** Und jede Attrappe wird ueber die Konstanten oben gebaut
# (`MARKER_TABELLE`, `MARKER_SCHLUESSEL`, `MARKER_WERTE`, die Befundcodes), nicht ueber Literale:
# wer den Mechanismus aendert, bekommt einen lauten Fehlschlag statt eines stummen Waechters. Der
# Selbsttest von `tools/mangel-stellen.sh` verbog einmal eine Konstante, die es nach einem Umbau
# nicht mehr gab -- er waere stumm geworden, ohne dass es jemand merkt.
KOPIER_ENDUNGEN = (".rs", ".toml", ".ld", ".json")

# Eine Abhaengigkeit, die sich an JEDES Manifest anhaengen laesst: die Zieltabelle gibt es dort
# garantiert noch nicht, `cfg(any())` trifft nie zu.
FREMDE_DEP = "\n[target.'cfg(any())'.dependencies]\n{name} = \"1\"\n"


# **Lesen und Schreiben getrennt** -- und das ist keine Kosmetik. Die erste Fassung schrieb
# `open(p, "w").write(RX.sub(..., open(p).read()))`: Python wertet das Objekt vor dem Argument
# aus, also TRUNKIERT das `open(p, "w")` die Datei, und gelesen wurde eine leere. Der Selbsttest
# meldete trotzdem gruen -- eine leere Datei traegt naemlich auch kein `forbid`. Ein Selbsttest,
# der aus dem falschen Grund besteht, ist genau die Form, gegen die er geschrieben ist; gefangen
# hat es erst der Nachbarfall (`deny` statt `forbid`), der dann den falschen Code meldete.
def lesen(pfad):
    with open(pfad, encoding="utf-8") as f:
        return f.read()


def schreiben(pfad, text):
    with open(pfad, "w", encoding="utf-8") as f:
        f.write(text)


def anhaengen(pfad, text):
    with open(pfad, "a", encoding="utf-8") as f:
        f.write(text)


def baum_kopieren(quelle, ziel):
    """Nur Manifeste, Quelltext und Bauskripte -- der Rest des Baums geht die Pruefung nichts an
    und kostet Sekunden."""
    for r, verz, dateien in os.walk(quelle):
        verz[:] = [d for d in verz if d not in ("target", ".git", "build", "docs", "Verification")]
        for fn in dateien:
            if not fn.endswith(KOPIER_ENDUNGEN) and fn != "build.rs":
                continue
            q = os.path.join(r, fn)
            z = os.path.join(ziel, os.path.relpath(q, quelle))
            os.makedirs(os.path.dirname(z), exist_ok=True)
            shutil.copy2(q, z)
    # Die Kopplungspruefung liest das Zertifikatswerkzeug -- ohne es waere die Kopie strukturell rot.
    for extra in ("tools/sign_trusted.py",):
        q = os.path.join(quelle, extra)
        if os.path.exists(q):
            z = os.path.join(ziel, extra)
            os.makedirs(os.path.dirname(z), exist_ok=True)
            shutil.copy2(q, z)


def _marke_schreiben(pfad, wert):
    """Setzt/entfernt die Marke in einem Manifest -- ueber die Konstanten, nicht ueber ein Literal."""
    text = lesen(pfad)
    kopf = "[" + ".".join(MARKER_TABELLE) + "]"
    text = re.sub(re.escape(kopf) + r"\n" + re.escape(MARKER_SCHLUESSEL) + r"\s*=\s*\"[^\"]*\"\n",
                  "", text)
    if wert is not None:
        text = text.rstrip("\n") + f"\n\n{kopf}\n{MARKER_SCHLUESSEL} = \"{wert}\"\n"
    schreiben(pfad, text)


def selbsttest(wurzel):
    print("-- Selbsttest (an einer Kopie, in beide Richtungen) --")
    fehler = 0
    tmp = tempfile.mkdtemp(prefix="einschluss-selbsttest-")
    try:
        kopie = os.path.join(tmp, "baum")
        baum_kopieren(wurzel, kopie)

        def lauf(kopie=kopie):
            return pruefen(kopie)

        # -- (0) Positivkontrolle. Ohne sie sagt jeder rote Ausgang unten nichts: er koennte am
        #        Kopiervorgang liegen statt an der Mutation.
        befunde, kandidaten, _, _ = lauf()
        _, kandidaten_echt, _, _ = pruefen(wurzel)
        if len(kandidaten) != len(kandidaten_echt) or befunde:
            print("  FEHLER: die unveraenderte KOPIE ist nicht gruen "
                  f"({len(kandidaten)} statt {len(kandidaten_echt)} Kandidaten, "
                  f"{len(befunde)} Befunde). Damit ist jeder rote Ausgang unten wertlos.")
            for b in befunde:
                print(f"          {b}")
            return 1
        print(f"  ok      : die unveraenderte Kopie ist gruen ({len(kandidaten)} Kandidaten)")

        opfer = sorted(kandidaten, key=lambda c: c.name)[0]
        opfer_manifest = os.path.join(kopie, opfer.rel, "Cargo.toml")
        opfer_wurzelmodul = os.path.join(kopie, os.path.relpath(opfer.wurzelmodule[0], wurzel))

        def erwarte(titel, code, sichern, mutieren):
            nonlocal fehler
            gesichert = sichern()
            try:
                mutieren()
                befunde, _, _, _ = lauf()
                codes = {b.code for b in befunde}
                if code in codes:
                    print(f"  ok      : {titel} -> [{code}]")
                else:
                    print(f"  BEFUND  : {titel} -> erwartet [{code}], bekommen "
                          f"{sorted(codes) or 'GAR NICHTS'}. Der Waechter kann diesen Fall nicht "
                          "sehen.")
                    fehler = 1
            finally:
                gesichert()

        def datei_sichern(pfad):
            alt = lesen(pfad)

            def zurueck():
                schreiben(pfad, alt)
            return zurueck

        # -- (1) forbid weg -> muss durchfallen.
        erwarte(
            f"{opfer.name} ohne `forbid(unsafe_code)`", "kein-forbid",
            lambda: datei_sichern(opfer_wurzelmodul),
            lambda: schreiben(opfer_wurzelmodul, RX_FORBID.sub("", lesen(opfer_wurzelmodul))))

        # -- (2) `deny` statt `forbid` -> muss durchfallen, und zwar UNTERSCHEIDBAR. `deny` ist
        #        der bequeme Tippfehler, und er ist genau der, den man nicht sehen darf: er sieht
        #        aus wie die Eigenschaft und ist in jedem Modul mit `#[allow]` zuruecknehmbar.
        erwarte(
            f"{opfer.name} mit `deny` statt `forbid`", "nur-deny",
            lambda: datei_sichern(opfer_wurzelmodul),
            lambda: schreiben(opfer_wurzelmodul,
                              RX_FORBID.sub("#![deny(unsafe_code)]", lesen(opfer_wurzelmodul))))

        # -- (3) `unsafe` im Quelltext -> muss durchfallen. Ohne Leerzeichen, weil genau diese
        #        Schreibweise am freieren Muster des Zertifikatswerkzeugs vorbeigeht.
        erwarte(
            f"{opfer.name} mit `unsafe{{` (ohne Leerzeichen)", "unsafe-im-quelltext",
            lambda: datei_sichern(opfer_wurzelmodul),
            lambda: anhaengen(opfer_wurzelmodul, "\nfn __selbsttest() { unsafe{ } }\n"))

        # -- (4) nicht gelistete Abhaengigkeit -> muss durchfallen.
        #        **In einer Zieltabelle**, nicht in `[dependencies]`: die Manifeste haben die
        #        Tabelle schon, und ein zweites `[dependencies]` ist kein Befund, sondern ein
        #        TOML-Syntaxfehler -- der Selbsttest haette dann den Parser gemessen und nicht die
        #        Regel. Nebenertrag: so laeuft auch der Zweig, der zielspezifische Tabellen liest.
        erwarte(
            f"{opfer.name} mit nicht gelisteter Abhaengigkeit", "fremde-abhaengigkeit",
            lambda: datei_sichern(opfer_manifest),
            lambda: anhaengen(opfer_manifest, FREMDE_DEP.format(name="wegwerf-fremd")))

        # -- (5) Bauskript -> muss durchfallen.
        def bauskript_an():
            schreiben(os.path.join(kopie, opfer.rel, "build.rs"), "fn main() {}\n")

        def bauskript_weg():
            def zurueck():
                p = os.path.join(kopie, opfer.rel, "build.rs")
                if os.path.exists(p):
                    os.remove(p)
            return zurueck

        erwarte(f"{opfer.name} mit `build.rs`", "bauskript", bauskript_weg, bauskript_an)

        # -- (6) Marke weg -> die UMKEHRUNG muss anschlagen (sonst laesst sie sich weglassen).
        erwarte(
            f"{opfer.name} ohne Marke (`{MARKER_SCHLUESSEL}`)", "unbezeichnet",
            lambda: datei_sichern(opfer_manifest),
            lambda: _marke_schreiben(opfer_manifest, None))

        # -- (7) unbekannter Markenwert -> geschlossene Wertemenge, keine Auffanggruppe.
        erwarte(
            f"{opfer.name} mit `{MARKER_SCHLUESSEL} = \"weich\"`", "unbekannter-wert",
            lambda: datei_sichern(opfer_manifest),
            lambda: _marke_schreiben(opfer_manifest, "weich"))

        # -- (8) Die Ratsche: ein Name, den niemand braucht, muss auffallen. Die Menge wird IM
        #        LAUFENDEN PROZESS verbogen -- also genau das Objekt, das der Waechter liest.
        #        Wer sie umbenennt, bekommt hier einen `NameError`, keinen stummen Test.
        ERLAUBTE_ABHAENGIGKEITEN["wegwerf-unbenutzt"] = "Selbsttest"
        try:
            befunde, _, _, _ = lauf()
            if "ratsche-veraltet" in {b.code for b in befunde}:
                print("  ok      : ein unbenutzter Name in ERLAUBTE_ABHAENGIGKEITEN -> "
                      "[ratsche-veraltet]")
            else:
                print("  BEFUND  : ein unbenutzter Name in ERLAUBTE_ABHAENGIGKEITEN faellt nicht "
                      "auf -- ein totes Zugestaendnis bliebe stehen.")
                fehler = 1
            # -- (9) und die GEGENRICHTUNG: derselbe Name, jetzt tatsaechlich gebraucht, muss
            #        durchgehen. Ohne diesen Fall waere der Erlaubnispfad nie gelaufen -- und ein
            #        Zweig, den nichts ausloest, ist kein Zweig.
            zurueck = datei_sichern(opfer_manifest)
            try:
                anhaengen(opfer_manifest, FREMDE_DEP.format(name="wegwerf-unbenutzt"))
                befunde, _, _, _ = lauf()
                codes = {b.code for b in befunde}
                # **Erwartet wird GENAU `kopplung-adr0014`, nicht „gar nichts".** Der eingefuegte
                # Name steht in dieser Menge und NICHT in der Allowlist des Zertifikatswerkzeugs
                # -- die Kopplungspruefung MUSS das sehen. Das ist die zweite Sprechprobe fuer
                # umsonst: `fremde-abhaengigkeit` schweigt (der Erlaubnispfad traegt), und die
                # Kopplung schlaegt an (sie ist nicht bloss dekorativ). Jeder ANDERE Code hier
                # hiesse, dass der Erlaubnispfad nicht traegt.
                if codes == {"kopplung-adr0014"}:
                    print("  ok      : eine gelistete Abhaengigkeit geht durch (der Erlaubnispfad "
                          "ist gelaufen) -- und die ADR-0014-Kopplung meldet die Abweichung")
                else:
                    print(f"  BEFUND  : erwartet genau ['kopplung-adr0014'], bekommen "
                          f"{sorted(codes) or 'GAR NICHTS'}. Entweder traegt der Erlaubnispfad "
                          "nicht, oder die Kopplung an ADR 0014 urteilt nicht mehr.")
                    fehler = 1
            finally:
                zurueck()
        finally:
            del ERLAUBTE_ABHAENGIGKEITEN["wegwerf-unbenutzt"]

        # -- (10) `[lints]` weicht `unsafe_code` auf -> die Hintertuer neben dem Attribut.
        erwarte(
            f"{opfer.name} mit `[lints.rust] unsafe_code = \"allow\"`", "lints-lockern",
            lambda: datei_sichern(opfer_manifest),
            lambda: anhaengen(opfer_manifest, "\n[lints.rust]\nunsafe_code = \"allow\"\n"))

        # -- (11) Die Crate steht in keinem Workspace -> sie wird nie uebersetzt, und dann ist
        #         `forbid` eine Behauptung statt eines Compilerfehlers.
        ws = os.path.join(kopie, "tests", "Cargo.toml")
        erwarte(
            f"{opfer.name} aus dem Workspace genommen", "nicht-im-workspace",
            lambda: datei_sichern(ws),
            lambda: schreiben(ws, lesen(ws).replace('"services/trusted/aggressor",', "")))

        # -- (12) Namensverwechslung: derselbe Name, aber aus einer Registry statt aus dem Baum.
        #         `fs` ist hier das Opfer, weil `aggressor-t` gar keine eingeschlossene
        #         Abhaengigkeit hat -- ein Fall, der am gewaehlten Opfer nicht vorkommt, waere
        #         nicht gemessen, sondern nur behauptet.
        fs_manifest = os.path.join(kopie, "programs", "trusted", "fs", "Cargo.toml")
        erwarte(
            "fs mit `caprock-part` aus einer Registry statt aus dem Baum", "namensverwechslung",
            lambda: datei_sichern(fs_manifest),
            lambda: schreiben(fs_manifest, re.sub(
                r'caprock-part = \{[^}]*\}', 'caprock-part = "1"', lesen(fs_manifest))))

        # -- (13) Ratsche 2: eine Absage, deren Anker weg ist, urteilt ueber nichts.
        absage_opfer = os.path.join(kopie, "crates", "caprock-cap", "Cargo.toml")
        erwarte(
            "caprock-cap nachtraeglich deklariert (die Absage widerspricht ihr)", "absage-veraltet",
            lambda: datei_sichern(absage_opfer),
            lambda: _marke_schreiben(absage_opfer, sorted(MARKER_WERTE)[0]))

        # -- (13) KEINE Kandidaten -> eigener Ausgang, nicht Entwarnung. Der wichtigste Fall:
        #         ein Waechter ueber null Crates gibt Entwarnung ueber nichts.
        gesichert = []
        for c in kandidaten:
            p = os.path.join(kopie, c.rel, "Cargo.toml")
            gesichert.append((p, lesen(p)))
            _marke_schreiben(p, None)
        try:
            befunde, kandidaten2, _, _ = lauf()
            # **Gemessen wird der RUECKGABECODE, nicht die Kandidatenzahl.** `abnahme.sh` liest
            # `$?` und nichts sonst; ein Waechter, der intern „0 Kandidaten" weiss und trotzdem 0
            # zurueckgibt, ist ueber den Exit-Code nicht von Erfolg zu unterscheiden.
            rc = main(["--nur-pruefen", "--wurzel", kopie])
            if kandidaten2 or rc != 3:
                print(f"  BEFUND  : ohne jede Marke bleiben {len(kandidaten2)} Kandidaten und der "
                      f"Rueckgabecode ist {rc} statt 3 -- ein leerer Lauf wuerde als Erfolg "
                      "gebucht.")
                fehler = 1
            else:
                print("  ok      : ohne jede Marke bleiben 0 Kandidaten, und der Rueckgabecode "
                      "ist 3 -- keine Entwarnung ueber nichts")
        finally:
            for p, alt in gesichert:
                schreiben(p, alt)

        # -- (14) und wieder gruen: der Waechter schlaegt nicht grundlos an. Ohne diesen Fall
        #         waere „rot bei jeder Mutation" auch dann erfuellt, wenn er IMMER rot ist.
        befunde, _, _, _ = lauf()
        if befunde:
            print("  BEFUND  : nach dem Zuruecknehmen aller Mutationen ist die Kopie nicht wieder "
                  "gruen -- der Selbsttest hat etwas liegen lassen:")
            for b in befunde:
                print(f"            {b}")
            fehler = 1
        else:
            print("  ok      : nach dem Zuruecknehmen wieder gruen -- kein blinder Alarm")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return fehler


def liste(wurzel):
    alle = crates_finden(wurzel)
    mitglieder = workspace_mitglieder(wurzel)
    print(f"{'Crate':24s} {'Marke':8s} {'forbid':7s} {'unsafe':7s} {'build.rs':9s} Abhaengigkeiten")
    for c in alle:
        n_unsafe = sum(k for _, k in c.unsafe_stellen())
        ws = "" if os.path.abspath(c.dir) in mitglieder else "  (in keinem Workspace)"
        print(f"{c.name:24s} {str(c.marke or '-'):8s} {str(c.forbid):7s} {n_unsafe:<7d} "
              f"{str(c.hat_bauskript):9s} {sorted(c.deps)}{ws}")
    return 0


def main(argv):
    wurzel = WURZEL
    if "--wurzel" in argv:
        wurzel = os.path.abspath(argv[argv.index("--wurzel") + 1])
    if "--liste" in argv:
        return liste(wurzel)

    print("== Eingeschlossenheit (Z28): forbid(unsafe_code) + benannte Abhaengigkeiten ==")
    befunde, kandidaten, alle, n = pruefen(wurzel)

    if not kandidaten:
        print(f"  {len(alle)} Crates im Baum, davon 0 mit "
              f"`[{'.'.join(MARKER_TABELLE)}] {MARKER_SCHLUESSEL} = \"streng\"`.")
        for b in befunde:
            print(f"  BEFUND  : {b}")
        print()
        print("== KEINE KANDIDATEN -- das ist KEINE Entwarnung ==")
        print("   Ein Waechter ueber null Crates hat nichts geprueft. Entweder ist der Marker weg")
        print("   (dann ist der Mechanismus kaputt), oder es gibt wirklich kein eingeschlossenes")
        print("   Modul mehr (dann ist die Eintrittskarte fuer Z28 weg). Beides ist ein Befund.")
        return 3

    for c in kandidaten:
        print(f"  Kandidat: {c.name:22s} ({c.rel})")
    for b in befunde:
        print(f"  BEFUND  : {b}", file=sys.stderr)
    print(f"  {len(kandidaten)} eingeschlossene Module, {len(alle)} Crates im Baum, "
          f"{n} Pruefungen")
    print(f"  Ratschen: ERLAUBTE_ABHAENGIGKEITEN {sorted(ERLAUBTE_ABHAENGIGKEITEN)} · "
          f"AUSDRUECKLICH_NICHT {sorted(AUSDRUECKLICH_NICHT)}")

    fehler = 1 if befunde else 0
    if "--nur-pruefen" not in argv:
        fehler |= selbsttest(wurzel)

    if fehler:
        print("== EINGESCHLOSSENHEIT: BEFUND ==", file=sys.stderr)
        return 1
    print("== Eingeschlossenheit: jedes deklarierte Modul kann nicht ausbrechen ==")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
