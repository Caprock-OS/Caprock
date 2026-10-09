//! `init` — der **Root-Task** (A-2.1).
//!
//! Der seL4-Weg: der Kernel laedt genau ein Startprogramm aus der Startmenge und uebergibt ihm die
//! Wurzel-Caps, die das System-Manifest ihm zuweist. Alles Weitere entsteht von hier aus im
//! Userland.
//!
//! Was dieses `init` tut, und warum jeder Schritt eine Aussage traegt:
//!
//! 1. **Die endowte Notification signalisieren.** Der Kernel hat sonst kein Fenster in einen
//!    isolierten Prozess; das Badge ist der Beleg, dass extern gebauter, aus dem Manifest
//!    ausgewaehlter Code tatsaechlich laeuft.
//! 2. **Die restliche Startmenge laden** -- ueber die eigene Loader-Cap, nicht ueber den Kernel.
//!    Das ist der eigentliche Punkt: der Kernel hat danach ein Programm geladen, das seinerseits
//!    Programme laedt. Jede weitere Faehigkeit ist ab hier ein Userland-Programm.
//!    Die eigene Notification wird dabei an Slot 0 der Kind-PD delegiert -- die Kinder melden sich
//!    also ueber denselben Kanal, und der Kernel sieht am akkumulierten Badge, wer gelaufen ist.
//! 3. **Sich beenden.** Kein Dauer-Park: Stack und TCB-Slot gehen zurueck, der Lebenszyklus ist
//!    vollstaendig. (Ein Root-Task eines echten Systems bliebe stattdessen als Dienst stehen; hier
//!    ist das Ende die pruefbarere Aussage.)
//!
//! **Boot-Argument** (`arg`): `(Anzahl der Programme << 32) | eigener Index`. Bewusst das Minimum
//! -- damit laesst sich "alle ausser mir" ausdruecken, ohne eine Indexverabredung mit dem
//! Testskript. Alles Weitere gehoerte hinter eine Capability, nicht in ein Register.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

/// Charakteristisches Badge ("der Root-Task lief"). Bewusst ein **hohes** Bit: die Badges der
/// Kinder liegen im unteren Wort, und ein akkumuliertes (ODER-verknuepftes) Badge soll beide
/// Aussagen getrennt lesbar lassen statt sie ineinander laufen zu lassen.
pub const ROOT_BADGE: u64 = 1 << 32;

/// A-3.1: die eigene Loader-Cap liess sich loeschen **und** die Autoritaet war danach wirklich weg
/// (ein anschliessendes `SYS_LOAD` wird abgewiesen). Der zweite Teil ist der wichtigere: dass ein
/// Slot leer ist, sagt fuer sich genommen nichts darueber, ob die Faehigkeit erloschen ist.
pub const CDELETE_GONE_BADGE: u64 = 1 << 33;

/// A-3.1: ein Cap, von dem noch Kopien abgeleitet sind, wird **nicht** geloescht -- und bleibt
/// benutzbar. Ein halb entfernter Cap waere schlimmer als gar keiner.
pub const CDELETE_CHILDREN_BADGE: u64 = 1 << 34;

/// Badge, das der Root-Task den Caps seiner Kinder mitgibt. Unteres Wort, damit es sich mit
/// [`ROOT_BADGE`] (hohes Bit) nicht vermischt. Der Kernel-Test liest daran ab, dass ein von
/// **init** geladenes Programm gelaufen ist -- nicht bloss init selbst.
pub const CHILD_BADGE: u64 = 0x4845_4C4F; // "HELO"

/// **C2: der DMA-Pool ist ein Argument des LADENS, keine Konstante im Kernel** (2026-08-26).
///
/// Der Kernel gab bis dahin jeder Treiber-PD dieselben 16 KiB. Die Groesse gehoert aber zum
/// Geraet: ein Blockgeraet mit einer Queue braucht anderes als eine Netzkarte mit Ringen. Und sie
/// gehoert **nicht ins Manifest** — das beschreibt den Bootzustand und wird signiert; wer zur
/// Laufzeit einen Treiber startet, muss die Zahl nennen koennen, ohne ein Dokument neu zu
/// unterschreiben.
///
/// Diese Tabelle ist die Politik **dieses** Boot-Taskmanagers. Ein Laufzeit-Treibermanager laese
/// sie aus einer Konfiguration; hier steht sie im Programm, und der Archivindex ist die einzige
/// Kennung, die ein Startprogramm ohne Manifestzugriff hat. Wer nicht darin steht, bekommt die
/// Vorgabe (`0`).
///
/// **Die Indizes sind eine Eigenschaft des Testarchivs**, nicht des Systems: 2 = `virtio-blk`,
/// 4 = `virtio-net` (s. `test-qemu-x86-load.sh`). Steht dort etwas anderes, bekommt es die
/// genannte Groesse — schaden kann das nicht, denn der Wunsch wird nur auf dem Pfad einer
/// Geraetezuteilung ueberhaupt gelesen.
const POOL: [(u64, u32); 2] = [(2, 8), (4, 32)];

/// Der DMA-Wunsch fuer den Archivindex `i`, in Seiten (`0` = Vorgabe).
fn pool_fuer(i: u64) -> u32 {
    let mut k = 0;
    while k < POOL.len() {
        if POOL[k].0 == i {
            return POOL[k].1;
        }
        k += 1;
    }
    0
}

/// **C2-Negativfall aus Ring 3**: eine Anforderung ueber `DRIVER_DMA_MAX_PAGES` hinaus wird mit
/// EIGENEM Code abgewiesen — und nicht gekuerzt. Eine stillschweigend halbierte DMA-Region ist ein
/// Geraet, das ueber ihr Ende hinausschreibt; dieser Fehler ist still, deshalb ist die Absage die
/// Aussage. Bit 35, neben den beiden `CDELETE`-Badges.
pub const POOL_REFUSED_BADGE: u64 = 1 << 35;

/// **B3-Negativfall aus Ring 3**: `BIND_IRQ` ohne `Irq`-Cap wird abgewiesen.
///
/// Das ist der Riegel selbst, nicht seine Beschreibung. Vor B3 nahm `bind_irq` rohe Zahlen und
/// behauptete in seinem Doku-Kommentar, „ueber die IRQ-Cap autorisiert" zu sein — eine Signatur
/// kann das nicht tragen, und *ein Waechter prueft die EXISTENZ eines Grundes, nie seine
/// WAHRHEIT*. Diese PD haelt keine `Irq`-Cap; bekaeme sie `OK`, waere jede PD des Systems
/// berechtigt, sich fremde Geraeteinterrupts zustellen zu lassen. Bit 36.
pub const IRQ_UNAUTHORIZED_BADGE: u64 = 1 << 36;

/// Cap-Slot der Loader-Cap (Slot-Konvention des Kernel-Glue, `kernel::loader::endow_from_manifest`).
const LOADER_SLOT: u64 = 0;
/// Cap-Slot der endowten Notification. Ihr Badge ist [`ROOT_BADGE`] -- gesetzt vom Kernel beim
/// Endowment, nicht von uns.
const NTFN_SLOT: u64 = 1;
/// Freie Slots fuer eigene, ANDERS GEBADGTE Kopien derselben Notification (A-3.2, `CCOPY`).
/// Ohne die waeren alle Signale dieses Programms ununterscheidbar: das Badge steckt in der Cap.
const NTFN_CHILDREN_SLOT: u64 = 3;
const NTFN_GONE_SLOT: u64 = 4;
/// **Die vorgebadgte Kopie, die an jedes Kind delegiert wird** (2026-08-25).
///
/// Bis dahin trug `SYS_LOAD` selbst ein Badge-Argument und praegte je Aufruf eine Ableitung. Mit
/// der Mehrfachdelegation waere ein Badge fuer acht Caps ein Parameter mit acht Bedeutungen --
/// also badgt dieses Programm **einmal** selbst (`CCOPY` ist genau dafuer da) und delegiert
/// danach den Slot. Eine Kopie fuer alle Kinder reicht, weil [`CHILD_BADGE`] fuer alle dasselbe
/// ist; wer je Kind ein eigenes Etikett will, badgt je Kind.
const NTFN_CHILD_SLOT: u64 = 5;
/// Eigene gebadgte Kopie fuer den C2-Negativfall (s. [`POOL_REFUSED_BADGE`]).
///
/// **Eine eigene Cap und kein Argument**: `SYS_SIGNAL` verodert das Badge der benutzten **Cap** in
/// `pending`; das Wort im Aufruf spielt keine Rolle. Der erste Anlauf rief
/// `signal(NTFN_SLOT, POOL_REFUSED_BADGE)` — angekommen ist damit `ROOT_BADGE`, und die Pruefzeile
/// meldete `zu-gross-abgewiesen=false`, obwohl die Absage korrekt gekommen war. Genau die Falle,
/// die im Register unter „das Badge steckt in der Cap" steht.
const NTFN_POOL_SLOT: u64 = 6;
/// Eigene gebadgte Kopie fuer den B3-Negativfall (s. [`IRQ_UNAUTHORIZED_BADGE`]).
///
/// **Slot 8 und nicht 7**: Slot 7 traegt seit B2 die `Irq`-Cap einer Treiber-PD. Diese PD ist
/// keine, der Slot waere also frei — aber zwei Bedeutungen fuer eine Slotnummer sind genau die
/// Form, an der die vier versteckten Politiken aus A-5.4 gehangen haben.
const NTFN_IRQ_SLOT: u64 = 8;
/// Der Slot, den dieser Negativfall als `Irq`-Cap ANBIETET — und der **leer** ist. Das ist die
/// gepruefte Sache: nicht „eine falsche Cap", sondern **keine**.
const KEINE_IRQ_CAP_SLOT: u64 = 9;
/// Own badged copy for the strand-5 handover report (s. [`handover_stub`]).
/// Slot 10: 0/1 loader/ntfn, 3/4 CCOPY probes, 5 child delegation, 6 pool
/// probe, 8 irq probe, 9 empty irq offer -- 2 and 7 stay free, 10 is next.
/// aarch64-only (the report step is, too).
#[cfg(target_arch = "aarch64")]
const NTFN_HO_SLOT: u64 = 10;
/// Rechte-Bitmaske fuer die Kopien (R+W+X; wird ohnehin mit den Rechten des Originals geschnitten).
const RWX: u64 = 7;

libcaprock::entry!(run);

/// Strand 5: firmware-handover report (receive/parse/attest). Local stub
/// module until strand 4 lands; see its header for the divergence list.
/// aarch64-only: the x86 path is wired by the UEFI-stub strand.
#[cfg(target_arch = "aarch64")]
mod handover_stub;

fn run(arg: usize) -> ! {
    let index = (arg & 0xffff_ffff) as u64;
    let count = (arg >> 32) as u64;

    // 1. "Ich laufe."
    libcaprock::signal(NTFN_SLOT, ROOT_BADGE);

    // 2. Die uebrige Startmenge laden. Fehler werden NICHT stillschweigend uebergangen: was nicht
    //    laedt, meldet sich als gesetztes Bit im Badge -- sonst saehe ein leerer Lauf aus wie ein
    //    erfolgreicher.
    // **Vor dem ersten Laden**: die Kopie, die delegiert wird. Schlaegt sie fehl, wird ohne
    // Delegation geladen -- die Kinder laufen dann, koennen sich aber nicht melden, und genau das
    // steht danach im `failed`-Wort statt in einem stillen Ausbleiben.
    let child_ok = libcaprock::ccopy(NTFN_SLOT, NTFN_CHILD_SLOT, RWX, CHILD_BADGE)
        == libcaprock::result::OK;
    let mut failed = 0u64;
    let mut i = 0u64;
    while i < count {
        if i != index {
            // Die eigene Notification (Slot 1) an Slot 0 der Kind-PD delegieren.
            // Die eigene Notification delegieren, aber mit EIGENEM Badge fuer das Kind -- sonst
            // signalisierten alle Kinder unter dem Badge des Root-Tasks und waeren nicht
            // auseinanderzuhalten (das Badge steckt in der Cap, nicht in der Nachricht).
            // Slot 0 der neuen PD -- die Loader-ABI-Konvention L2, jetzt ausgeschrieben statt
            // im Kernel verdrahtet.
            let d: &[(u8, u8)] = if child_ok {
                &[(NTFN_CHILD_SLOT as u8, 0)]
            } else {
                &[]
            };
            // `0` = Vorgabebudget (2026-08-26). Der Root-Task laedt die **Startmenge**, und die
            // ist per Entwurf klein; wer zur Laufzeit eine Treiberumgebung startet, nennt hier
            // seine Zahl. Eine Vorgabe, die `init` schon anhebt, waere ein Budget fuer alle --
            // also wieder die Konstante, nur an einer anderen Stelle.
            if libcaprock::load(LOADER_SLOT, i, d, 0, pool_fuer(i)) != libcaprock::result::OK {
                failed |= 1 << (i.min(30) + 1);
            }
        }
        i += 1;
    }
    if failed != 0 {
        libcaprock::signal(NTFN_SLOT, failed);
    }

    // 3. Zwei eigene, unterschiedlich gebadgte Kopien der Notification anlegen (A-3.2, `CCOPY`).
    //    Ohne sie koennte dieses Programm dem Kernel nur EINE Tatsache melden -- naemlich die, die
    //    im Badge seiner endowten Cap steht. Mit ihnen wird jedes Teilergebnis unterscheidbar.
    let ok_children = libcaprock::ccopy(NTFN_SLOT, NTFN_CHILDREN_SLOT, RWX, CDELETE_CHILDREN_BADGE);
    let ok_gone = libcaprock::ccopy(NTFN_SLOT, NTFN_GONE_SLOT, RWX, CDELETE_GONE_BADGE);

    // 3b. C2 aus Ring 3: ein DMA-Pool ueber der Obergrenze wird mit EIGENEM Code abgewiesen.
    //
    //     **Vor** dem Loeschen der Loader-Cap -- danach gaebe es `ERR_BADCAP`, und das waere
    //     dieselbe Antwort fuer zwei verschiedene Lagen. Der Index ist gleichgueltig: die Absage
    //     faellt am Rand des Dispatch, bevor das Archiv ueberhaupt angesehen wird.
    if libcaprock::ccopy(NTFN_SLOT, NTFN_POOL_SLOT, RWX, POOL_REFUSED_BADGE)
        == libcaprock::result::OK
        && libcaprock::load(
            LOADER_SLOT,
            0,
            &[],
            0,
            libcaprock::DRIVER_DMA_MAX_PAGES + 1,
        ) == libcaprock::result::ERR_DMA_TOO_LARGE
    {
        libcaprock::signal(NTFN_POOL_SLOT, 0);
    }

    // 3c. B3 aus Ring 3: `BIND_IRQ` ohne `Irq`-Cap wird ABGEWIESEN.
    //
    //     **Vor** dem Loeschen der Loader-Cap, aus demselben Grund wie 3b -- und der Slot ist
    //     LEER, nicht falsch belegt: geprueft wird, dass der Kernel eine Cap VERLANGT, nicht dass
    //     er Typen unterscheidet. Das Typurteil faellt derselbe `let ObjectKind::Irq else`, aber
    //     die interessante Lage ist die haeufige: eine PD, die einfach keine hat.
    //
    //     `ERR_BADCAP` und nicht bloss „ungleich OK": ein `ERR_BADSYS` saehe hier genauso aus und
    //     hiesse, dass der Syscall gar nicht existiert -- der Negativfall waere dann gruen, weil
    //     nichts gebaut ist. Genau die Form, gegen die die Sprechprobe steht.
    //
    //     **ZWEI Lagen, und die zweite ist die, die den Riegel wirklich trifft.** Ein LEERER Slot
    //     wird schon von der generischen Cap-Aufloesung abgewiesen -- das belegt „man braucht
    //     ueberhaupt eine Cap", aber nichts ueber `BIND_IRQ`. Erst eine Cap, die die PD wirklich
    //     HAELT und die keine `Irq`-Cap ist, kommt bis in den Zweig und wird dort am Typ
    //     abgewiesen. Nur die erste zu pruefen hiesse, ein Gatter zu messen, das woanders steht --
    //     dieselbe Form wie eine Sprechprobe an einer Ausnahme statt am gepruefeten Pfad.
    if libcaprock::ccopy(NTFN_SLOT, NTFN_IRQ_SLOT, RWX, IRQ_UNAUTHORIZED_BADGE)
        == libcaprock::result::OK
        // (a) gar keine Cap -- die generische Aufloesung weist ab
        && libcaprock::bind_irq(KEINE_IRQ_CAP_SLOT, NTFN_SLOT, 0)
            == libcaprock::result::ERR_BADCAP
        // (b) eine gehaltene Cap vom FALSCHEN Typ -- der Zweig selbst weist ab
        && libcaprock::bind_irq(NTFN_SLOT, NTFN_SLOT, 0) == libcaprock::result::ERR_BADCAP
    {
        libcaprock::signal(NTFN_IRQ_SLOT, 0);
    }

    // 4. A-3.1 aus Ring 3 pruefen -- hier und nicht im Kernel, weil genau der Weg geprueft werden
    //    soll, den ein echter Dienst nimmt: ueber die ABI, aus einer isolierten PD heraus.
    //
    //    4a. Die eigene Notification (Slot 1) hat jetzt abgeleitete Kopien. Sie zu loeschen MUSS
    //        scheitern, und sie MUSS danach weiter funktionieren -- das Signal ueber die KOPIE ist
    //        der Beweis fuer beides in einem (die Kopie zeigt auf dasselbe Objekt).
    if ok_children == libcaprock::result::OK
        && libcaprock::cdelete(NTFN_SLOT) == libcaprock::result::ERR_HASCHILDREN
    {
        libcaprock::signal(NTFN_CHILDREN_SLOT, 0);
    }
    //    4b. Die Loader-Cap dagegen hat keine Ableitungen -- sie laesst sich loeschen. Und danach
    //        ist die Faehigkeit weg: ein weiteres `SYS_LOAD` wird abgewiesen. Erst das zusammen ist
    //        die Aussage; ein geraeumter Slot allein waere Buchhaltung.
    if ok_gone == libcaprock::result::OK
        && libcaprock::cdelete(LOADER_SLOT) == libcaprock::result::OK
        && libcaprock::load(LOADER_SLOT, 0, &[], 0, 0) != libcaprock::result::OK
    {
        libcaprock::signal(NTFN_GONE_SLOT, 0);
    }

    // 4d. Strand 5: firmware-handover report (receive/parse/attest, never
    //    silent). Runs on kernel-mapped pages, needs no caps, and never blocks
    //    the duties above: a missing handover yields word 0 (no attestation
    //    bit), which the kernel reports as absent -- loudly, never silently.
    //    aarch64-only (the x86 path is wired by the UEFI-stub strand).
    #[cfg(target_arch = "aarch64")]
    {
        let word = handover_stub::run();
        if libcaprock::ccopy(NTFN_SLOT, NTFN_HO_SLOT, RWX, word)
            == libcaprock::result::OK
        {
            libcaprock::signal(NTFN_HO_SLOT, 0);
        }
    }

    // 5. Fertig.
    libcaprock::exit();
}
