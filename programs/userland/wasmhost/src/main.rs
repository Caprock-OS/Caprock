//! `wasmhost` — eine **WASM-Laufzeit als gewoehnliche UserLand-PD** (Z15/W1).
//!
//! ================================================================================================
//! WARUM DAS HIER UND NICHT IM KERN LIEGT
//! ================================================================================================
//!
//! Gemessen am 2026-08-09: die Engine (`wasmi 0.31`, `no_std`, `opt-level="z"`, LTO) ist
//! **216 KiB `.text` + 17 KiB `.rodata`** — so gross wie der ganze Mikrokern (212 KiB). Genau
//! deshalb steht sie hier: fuer jeden anderen Mandanten waechst die TCB um **null**, und wer kein
//! WASM ausfuehrt, traegt kein Byte davon.
//!
//! Der Heap dagegen ist klein: 5 KiB fuer ein triviales Modul, 1 233 KiB fuer ein realistisches
//! Rust-Modul — und das meiste davon ist der **Linearspeicher des Gastes**, gehoert also ohnehin
//! ihm. Der Preis der Engine ist ihr Code, nicht ihr Speicher.
//!
//! ================================================================================================
//! WAS DIESE PD BELEGT — UND WAS DIE EINZELNEN AUSSAGEN VONEINANDER UNTERSCHEIDET
//! ================================================================================================
//!
//! „Eine WASM-Engine startet" ist keine Aussage; das tut sie auch, wenn nichts stimmt. Gemeldet
//! werden deshalb **vier unterscheidbare Tatsachen**, jede ueber eine eigen gebadgte Kopie
//! derselben Notification (dasselbe Muster wie `init` bei A-3.1):
//!
//! | Badge | Aussage |
//! |---|---|
//! | `WASM_INST` | ein Modul wurde geladen und instanziiert |
//! | `WASM_RESULT` | die exportierte Funktion lieferte den **gerechneten** Wert, nicht irgendeinen |
//! | `WASM_REJECT` | ein **mutiertes** Modul wurde abgewiesen |
//! | `WASM_TRAP` | ein Zugriff ausserhalb des Linearspeichers ergab einen **WASM-Trap** |
//!
//! **Der vierte ist der eigentliche Punkt, und er belegt sich selbst.** Wenn der Trap die PD
//! mitgerissen haette, koennte sie ihn nicht mehr melden — das Badge kommt nur an, wenn die PD
//! den Trap ueberlebt hat. Die Sandbox haelt also **innerhalb** der PD, und die PD-Isolation ist
//! die zweite Linie, nicht die erste. Ohne diesen Fall waere die Zeile eine Aussage darueber,
//! dass eine Engine laeuft, und keine ueber Isolation.
//!
//! Der dritte trennt „die Engine fuehrt aus" von „die Engine **prueft**": ein Interpreter, der
//! jedes Byte akzeptiert, ist keine Sandbox.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};

// --- Badges (Bits 40..43; 32..34 sind vergeben, s. `loader.rs` und `init`) --------------------
pub const WASM_INST: u64 = 1 << 40;
pub const WASM_RESULT: u64 = 1 << 41;
pub const WASM_REJECT: u64 = 1 << 42;
pub const WASM_TRAP: u64 = 1 << 43;
/// **Die Sprechprobe dieser PD** — Bit 44 ist das Badge, mit dem der Lader die Notification aus
/// dem Manifest gemuenzt hat (`CLIENT_NTFN_BADGE`). Ein `signal` auf Slot 1 setzt es **ohne jede
/// Cap-Operation**: es belegt „diese PD laeuft und kann signalisieren", und zwar bevor irgendein
/// `ccopy` oder irgendein WASM-Schritt stattgefunden hat.
///
/// Ohne sie kollabierten vier grundverschiedene Fehlerbilder in dasselbe Schweigen: die PD lief
/// nicht, `ccopy` schlug fehl, `signal` schlug fehl, oder die Engine kam nicht durch. Genau deshalb
/// meldete die Pruefzeile **Abwesenheit** („keine WASM-PD in der Startmenge") statt eines
/// Fehlschlags -- ein Schweigen, das wie ein bestandener Test aussieht.
pub const WASM_LEBT: u64 = 1 << 44;
/// **Die zweite Sprechprobe: geht `ccopy` ueberhaupt?** Eine Kopie mit eigenem Badge, gemacht
/// BEVOR die Engine laeuft. Kommt dieses Badge an und die vier anderen nicht, liegt es an der
/// Engine; kommt es nicht an, liegt es am Cap-Pfad. Das trennt die beiden Haelften, die sich
/// vorher gegenseitig verdeckt haben.
pub const WASM_CCOPY: u64 = 1 << 45;
/// Slot fuer die `ccopy`-Sprechprobe (s. `N_INST` fuer die Begruendung der Slot-Wahl).
const N_PROBE: u64 = 10;

/// Cap-Slot der **eigenen**, aus dem Manifest endowten Notification.
///
/// Nicht Slot 0: dort liegt die vom Root-Task **delegierte** Cap, und die ist eine reine
/// Signal-Cap (`cap_mint(.., Rights::WRITE, ..)`). `ccopy` kann Rechte nicht verstaerken, also
/// schlaegt jede Kopie davon fehl — still, weil `melde` dann nichts tut. Die Manifest-Endowment-Cap
/// in Slot 1 hat RWX (`install_notification_cap_badged(.., Rights::RWX, ..)`), und genau von ihr
/// leitet `init` seine eigenen gebadgten Kopien ab.
const NTFN: u64 = 1;

/// Slots fuer die eigen gebadgten Kopien — **ab 6**, und das ist keine Willkuer.
///
/// Der Loader endowt nach festen Slots: 0 = Loader-Cap, 1 = Notification (aus dem Manifest),
/// 2 = Endpoint, 3..5 = Geraete-Caps. Die erste Fassung legte die Kopien nach 1..4 und traf damit
/// die vom Manifest endowte Notification in Slot 1 — `ccopy` in einen belegten Slot schlaegt fehl,
/// `melde` tat nichts, und die Berichtszeile meldete **Abwesenheit** („keine WASM-PD in der
/// Startmenge") statt eines Fehlschlags. Genau die Form, gegen die dieses Projekt seine Prueferregel
/// hat: ein Schweigen, das wie ein bestandener Test aussieht.
const N_INST: u64 = 6;
const N_RESULT: u64 = 7;
const N_REJECT: u64 = 8;
const N_TRAP: u64 = 9;
/// **Nur Schreibrecht.** Die delegierte Notification eines Kindes ist eine SIGNAL-Cap
/// (`cap_mint(.., Rights::WRITE, ..)`), und `ccopy` kann Rechte nicht **verstaerken**. Die erste
/// Fassung forderte `RWX` -- wie `init`, dessen Cap aber aus dem Loader-Endowment stammt und
/// tatsaechlich RWX hat. Die Folge war still: `ccopy` schlug fehl, `melde` tat nichts, und die
/// Zeile meldete „keine WASM-PD in der Startmenge" -- also Abwesenheit statt Fehlschlag.
/// Gefunden hat es eine Sonde, die keine Cap braucht (ein Fault an `0xDEAD_0000`).
const W: u64 = 7; // wie `init`: die Maske wird GESCHNITTEN, nicht verstaerkt

// ================================================================================================
// DER HEAP — und warum er statisch ist
// ================================================================================================
//
// Eine geladene PD bekommt ihren Speicher beim Laden: die PT_LOAD-Segmente und 16 KiB Stack. Es
// gibt **keinen** Weg, zur Laufzeit mehr zu bekommen (`SYS_MAP` bildet nur ab, was die PD schon
// haelt) -- das ist Z14 Stufe 1 und steht noch aus.
//
// Bis dahin ist der Heap ein Feld im `.bss`, das der Lader als Segment mitanlegt. Das ist keine
// Notloesung, sondern genau das Modell, das WASM ohnehin hat: der Linearspeicher eines Gastes
// steht bei der Instanziierung fest. Was damit **nicht** geht, ist `memory.grow` -- und das steht
// als W3 in `TODO.md`, statt hier stillschweigend zu fehlen.
const HEAP_BYTES: usize = 1 << 21; // 2 MiB -- gemessen: 1 233 KiB Hoechststand
                                   // realistisches Modul, plus Luft fuer die Engine selbst
static mut ARENA: [u8; HEAP_BYTES] = [0; HEAP_BYTES];
static mut NEXT: usize = 0;

/// **Bump-Allokator ohne Freigabe.** Das ist ehrlich und nicht faul: dieser Prozess laedt EIN
/// Modul, laesst es laufen und endet. Ein Allokator mit Freiliste waere Code, dessen einzige
/// Wirkung waere, dass er ungetestet mitfaehrt.
///
/// `dealloc` ist absichtlich leer. Wer hier eine Freiliste einbaut, muss vorher `HEAP_HOCH`
/// messen -- sonst tauscht er eine bekannte Grenze gegen eine unbekannte.
struct Bump;

/// Hoechststand, damit die Grenze **gemessen** und nicht geraten ist.
static mut HEAP_HOCH: usize = 0;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = (NEXT + l.align() - 1) & !(l.align() - 1);
        if p + l.size() > HEAP_BYTES {
            return core::ptr::null_mut(); // -> `handle_alloc_error` -> Panik -> Abbruch
        }
        NEXT = p + l.size();
        if NEXT > HEAP_HOCH {
            HEAP_HOCH = NEXT;
        }
        (&raw mut ARENA).cast::<u8>().add(p)
    }
    unsafe fn dealloc(&self, _p: *mut u8, _l: Layout) {}
}

#[global_allocator]
static A: Bump = Bump;

// ================================================================================================
// DIE GASTMODULE
// ================================================================================================
//
// Von Hand geschrieben, nicht erzeugt -- damit die Bytes im Quelltext stehen und die MUTATION
// eine Zeile ist statt eines Werkzeugs. Ein Testmodul, das ein Build-Schritt erzeugt, ist ein
// zweiter Bauweg fuer eine Aussage, die aus einer Handvoll Bytes besteht.

/// `main() -> i32`, gibt `42`. Der Wert ist im Modul, nicht im Programm — deshalb ist der
/// Vergleich unten eine Aussage ueber die Ausfuehrung und nicht ueber eine Konstante.
#[rustfmt::skip]
const MODUL_GUT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // \0asm, Version 1
    0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f,       // Typ: () -> i32
    0x03, 0x02, 0x01, 0x00,                         // Funktion 0 hat Typ 0
    0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00, 0x00, // Export "main" = Funktion 0
    0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b, // Rumpf: i32.const 42; end
];
const ERWARTET: i32 = 42;

/// Dasselbe Modul mit **einem** verdorbenen Byte im Code-Abschnitt: `0x41` (`i32.const`) wird zu
/// `0xd3` — ein Opcode, den es nicht gibt. Ein Interpreter, der das ausfuehrt statt abzuweisen,
/// ist keine Sandbox.
#[rustfmt::skip]
const MODUL_KAPUTT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f,
    0x03, 0x02, 0x01, 0x00,
    0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00, 0x00,
    0x0a, 0x06, 0x01, 0x04, 0x00, 0xd3, 0x2a, 0x0b, // <-- 0x41 -> 0xd3
];

/// Ein Modul mit **einer** Speicherseite (64 KiB), das bei `0x10000` liest — also genau ein Byte
/// hinter dem Ende. Erwartet wird ein WASM-Trap, **kein** Seitenfehler der PD.
#[rustfmt::skip]
const MODUL_UEBERGRIFF: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f,       // () -> i32
    0x03, 0x02, 0x01, 0x00,
    0x05, 0x03, 0x01, 0x00, 0x01,                   // Speicher: 1 Seite, kein Maximum
    0x07, 0x08, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x00, 0x00,
    // i32.const 0x10000; i32.load align=2 offset=0; end
    0x0a, 0x0b, 0x01, 0x09, 0x00, 0x41, 0x80, 0x80, 0x04, 0x28, 0x02, 0x00, 0x0b,
];

// ================================================================================================

fn melde(slot: u64, badge: u64) {
    // Die Kopie traegt das Badge; das Nachrichtenwort spielt bei SIGNAL keine Rolle (s. `init`).
    if libcaprock::ccopy(NTFN, slot, W, badge) == libcaprock::result::OK {
        libcaprock::signal(slot, 0);
    }
}

/// Ein Modul laden, instanziieren und `main` rufen. `None`, wenn irgendein Schritt scheitert --
/// **welcher**, unterscheidet der Aufrufer ueber die Badges, nicht diese Funktion.
fn fahre(wasm: &[u8]) -> Option<i32> {
    use wasmi::{Engine, Linker, Module, Store};
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).ok()?;
    let mut store = Store::new(&engine, ());
    let linker = <Linker<()>>::new(&engine);
    let inst = linker.instantiate(&mut store, &module).ok()?.start(&mut store).ok()?;
    let f = inst.get_typed_func::<(), i32>(&store, "main").ok()?;
    f.call(&mut store, ()).ok()
}

#[no_mangle]
pub extern "C" fn _start(_arg: usize) -> ! {
    // 0a. **Sprechprobe ohne jede Cap-Operation.** Slot 1 traegt die vom Manifest endowte
    //     Notification mit ihrem Lader-Badge; ein `signal` darauf belegt „ich laufe und kann
    //     signalisieren". Muss VOR allem anderen stehen -- eine Sprechprobe hinter dem
    //     geprueften Pfad ist keine.
    libcaprock::signal(NTFN, 0);

    // 0b. **Sprechprobe fuer den Cap-Pfad.** Eine Kopie mit eigenem Badge, bevor die Engine
    //     ueberhaupt anlaeuft. Damit ist „ccopy geht nicht" von „die Engine kommt nicht durch"
    //     unterscheidbar -- vorher sahen beide gleich aus.
    melde(N_PROBE, WASM_CCOPY);

    // 1. Das gute Modul: instanziieren und rechnen lassen.
    if let Some(r) = fahre(MODUL_GUT) {
        melde(N_INST, WASM_INST);
        if r == ERWARTET {
            melde(N_RESULT, WASM_RESULT);
        }
    }

    // 2. Das mutierte Modul MUSS scheitern. Ein `Some` hier waere der Befund.
    if fahre(MODUL_KAPUTT).is_none() {
        melde(N_REJECT, WASM_REJECT);
    }

    // 3. Der Uebergriff MUSS als WASM-Trap enden -- und dass diese PD danach noch signalisieren
    //    kann, ist der Beleg, dass der Trap sie nicht mitgerissen hat. Waere die Sandbox
    //    durchlaessig, faultete die PD hier, und das Badge kaeme nie an.
    if fahre(MODUL_UEBERGRIFF).is_none() {
        melde(N_TRAP, WASM_TRAP);
    }

    libcaprock::exit();
}
