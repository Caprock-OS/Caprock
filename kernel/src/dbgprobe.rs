//! **Z6b: die Debugger-Sonde — architekturneutral.**
//!
//! Bis zum 2026-08-20 lag sie im x86-Hochlauf und mass am dortigen Worker. Der geprüfte
//! Mechanismus ist aber arch-neutral (Scheduler, Caps, ABI, Finalisierung), also gehoert die Sonde
//! es auch — dasselbe Argument wie bei [`crate::dbgmem`] und wie bei `dmatests`: **eine Sonde, die
//! nur ein Zweig faehrt, laesst die andere ungeprueft**, und ungeprueft ist nicht „vermutlich
//! gruen".
//!
//! Sie bringt ihr **eigenes Ziel** mit: eine isolierte PD mit einem EL0-Traeger, der einen
//! Rundenzaehler in eine ihm gemappte Seite schreibt. Der Kernel liest die Seite ueber ihre
//! Physadresse. Damit ist der Fortschritt eine **Wirkung des Ziels** statt eines Wertes, den der
//! gemessene Pfad selbst setzt — die Regel, an der die vergiftete Mangel-Marke und die
//! `park`-Zeile gescheitert sind.
//!
//! Was hier gemessen wird, steht in der Berichtszeile; die tragende Aussage ist
//! `vorher-undebuggbar` (Z6b §0).

use crate::system;
use caprock_hal::println;
use core::sync::atomic::{AtomicBool, Ordering};

/// Urteil der `dbg`-Zeile.
/// Der Ausgang dieser Sonde — **dreiwertig** (2026-08-25, s. `crate::befund`).
static DBG_OK: crate::befund::AtomicBefund = crate::befund::AtomicBefund::neu();

/// Das Urteil, fuer die Hochlaufwege.
pub fn urteil() -> crate::befund::Befund {
    DBG_OK.lesen()
}

/// **Der Traeger des Ziels: ein EL0-Rundlauf, der einen Zaehler hochzaehlt.**
///
/// `#[link_section = ".user_text"]` ist keine Formalie — ein Ring-3-Einsprung in `.text` ist aus
/// Ring 3 nicht ausfuehrbar und faultet **an seiner eigenen Einsprungadresse**; im Protokoll steht
/// dann ein Fault und eine Zeile aus lauter Nullen, was wie ein kaputter Mechanismus aussieht und
/// eine fehlende Zeile ist. Und kein Helferaufruf im Rumpf, sonst laege DER in `.text`.
#[link_section = ".user_text"]
extern "C" fn traeger(arg: usize) -> ! {
    let p = arg as *mut u64;
    loop {
        // SAFETY: `arg` ist die VA einer Seite, die dem Thread RW gemappt wurde (identisch, also
        // VA == PA). Kein anderer Schreiber.
        unsafe { p.write_volatile(p.read_volatile().wrapping_add(1)) };
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
    }
}

/// **Der Zielthread der Stopp-Latenzmessung.** Ein Kernel-Rundlauf, der nichts tut ausser zu
/// laufen -- gestartet auf einem FREMDEN Kern, damit „laeuft gerade" ueberhaupt eintreten kann,
/// waehrend der Bericht seinen eigenen Kern haelt.
extern "C" fn latenz_ziel(_arg: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

fn poke(pa: u64, v: u64) {
    // SAFETY: frisch allozierte, identisch abgebildete RAM-Seite.
    unsafe { core::ptr::write_volatile(pa as *mut u64, v) };
}
fn peek(pa: u64) -> u64 {
    // SAFETY: wie `poke`.
    unsafe { core::ptr::read_volatile(pa as *const u64) }
}

/// **Die Sonde.** Von beiden Hochlaufwegen gerufen.
pub fn messen(prio: u8) {
    use caprock_sched::ThreadId;
    let warten = || {
        let t0 = caprock_hal::timer::ticks(0);
        let mut wache = 0u64;
        while caprock_hal::timer::ticks(0) < t0 + 3 && wache < 200_000_000 {
            core::hint::spin_loop();
            wache += 1;
        }
    };
    // --- Aufbau: die Sonde bringt ihr EIGENES Ziel mit -------------------------------------
    //
    // Bis hierher mass sie am x86-Worker (`WORKER_TID0`) und lief deshalb nur dort. Der
    // Mechanismus ist aber arch-neutral -- Scheduler, Caps, ABI --, also gehoert die Sonde es
    // auch. Dasselbe Argument wie bei `dbgmem` und wie bei `dmatests` vor drei Wochen: **eine
    // Sonde, die nur ein Zweig faehrt, laesst die andere ungeprueft**, und „ungeprueft" ist nicht
    // „vermutlich gruen".
    //
    // Der Traeger schreibt seinen Rundenzaehler in eine Seite, die ihm gemappt wird; der Kernel
    // liest sie ueber die Physadresse. Damit ist der Fortschritt eine **Wirkung des Ziels** und
    // nicht ein Wert, den der gemessene Pfad selbst setzt -- die Regel, an der die vergiftete
    // Mangel-Marke und die `park`-Zeile gescheitert sind.
    let Some(baton) = system::alloc(4096, 4096).map(|c| c.region().base) else {
        println!("dbg     : SKIP (keine Seite fuer den Rundenzaehler)");
        DBG_OK.uebersprungen();
        return;
    };
    poke(baton, 0);
    let Some(zielpd) = system::create_pd() else {
        println!("dbg     : SKIP (keine PD frei)");
        DBG_OK.uebersprungen();
        return;
    };
    let Some((zp, _)) = system::spawn_isolated_parked(traeger as *const () as usize, baton as usize, prio)
    else {
        println!("dbg     : SKIP (kein Ziel-Thread)");
        DBG_OK.uebersprungen();
        return;
    };
    system::bind_pd_parked(&zp, zielpd);
    system::map_into_parked(&zp, baton, 4096, 1); // RW -- der Traeger zaehlt hinein
    let Some(tid) = system::admit(zp) else {
        println!("dbg     : SKIP (Ziel nicht zulassbar)");
        DBG_OK.uebersprungen();
        return;
    };
    let raw = tid.to_raw();

    // --- 1. Die Sonde laeuft ueberhaupt? (Sprechprobe -- ohne sie belegt „steht" nichts.) ------
    let a0 = peek(baton);
    warten();
    let a1 = peek(baton);
    let laeuft = a1 > a0;

    // --- 2. §0: OHNE Praegung ist die PD mit KEINER Cap des Systems debuggbar ------------------
    //
    // Nicht „die Sonde haelt keine Cap" -- das waere `ERR_BADCAP` und eine Aussage ueber die Sonde.
    // Gefragt ist die Aussage ueber das SYSTEM, und die wird beantwortet, indem jeder Cap-Slot der
    // Wurzel-PD als Debug-Cap versucht wird. Alle muessen abgewiesen werden.
    let keine_autoritaet = !system::any_debug_authority_over(zielpd);
    let mut alle_slots_abgewiesen = true;
    let mut versucht = 0usize;
    for slot in 0..64 {
        if system::debug_stop(0, slot, raw).is_ok() {
            alle_slots_abgewiesen = false;
        }
        versucht += 1;
    }
    let vorher_undebuggbar = keine_autoritaet && alle_slots_abgewiesen;

    // --- 3. Praegen und ableiten --------------------------------------------------------------
    let Some(wurzelslot) = system::mint_debuggable(zielpd) else {
        println!("dbg     : SKIP (keine Debuggable praegbar -- kein Cap-Slot frei)");
        DBG_OK.uebersprungen();
        return;
    };
    let attach = system::debug_attach(0, wurzelslot, caprock_abi::debug::RIGHT_BOTH);
    let Ok(slots) = attach else {
        println!("dbg     : FAILURES debug_attach abgewiesen ({attach:?}) -- die Praegung wirkte, die Ableitung nicht");
        return;
    };
    let ctrl = (slots & 0xffff_ffff) as usize;
    let lese = (slots >> 32) as usize;
    let abgeleitet = ctrl != 0 && lese != 0 && ctrl != lese;

    // --- 4. Anhalten -- und zwar nach WIRKUNG gemessen ----------------------------------------
    let gestoppt = system::debug_stop(0, ctrl, raw).is_ok();
    let b0 = peek(baton);
    warten();
    warten();
    let b1 = peek(baton);
    let steht = b0 == b1;

    // --- 5. Der zweite Halter bekommt ERR_DEBUG_BUSY, nicht Erfolg und nicht Schweigen ---------
    //
    // „Eins" ist eine Kapazitaet, und wer eine Kapazitaet einfuehrt, muss den Ueberlauf BENENNEN
    // (D11). Stillschweigende Annahme gaebe beiden Debuggern den Glauben, ihr `continue` gehoere
    // ihnen -- der erste, der ruft, gaebe das Ziel unter dem anderen frei.
    let zweiter = system::debug_attach(0, wurzelslot, caprock_abi::debug::RIGHT_CONTROL);
    let busy = match zweiter {
        Ok(s2) => {
            system::debug_stop(0, (s2 & 0xffff_ffff) as usize, raw)
                == Err(caprock_abi::result::ERR_DEBUG_BUSY)
        }
        Err(_) => false,
    };

    // --- 6. Die Schreibmaske: das Ringwort wird abgewiesen, ein GPR nicht ----------------------
    // **Gemessen wird der GRUND, nicht der Ausgang** -- und das ist eine Berichtigung, die eine
    // Gegenprobe erzwungen hat (2026-08-20, M5b).
    //
    // Die erste Fassung fragte `== Err(ERR_RIGHTS)`. Damit blieb sie gruen, als BEIDE Ringgatter
    // ausgeschaltet waren -- denn es gibt ein **drittes**: `writeback_erlaubt` ordnet Wort 18
    // keiner schreibbaren Klasse zu und weist es per Vorgabe ab (`UeberDerStufe`). Sicherheitlich
    // ist das gut (drei Schichten, Vorgabe = nein); als MESSUNG war die Zeile stumpf: „abgewiesen
    // weil Ringwort" und „abgewiesen weil unklassifiziert" sahen gleich aus, und eine Mutation am
    // Ringgatter konnte sie nicht bewegen.
    //
    // Der Kernel zaehlt die Gruende getrennt (`DEBUG_WR_RING` gegen `DEBUG_WR_LEVEL`), also wird
    // hier die Groesse gelesen, die sich tatsaechlich aendert. Dieselbe Lehre wie die `park`-Zeile,
    // die `is_parked` statt `blocked` las.
    // **Zwei Konjunkte, weil es zwei Aussagen sind** -- und die Trennung ist selbst gemessen:
    //
    // * `Ringwort-abgewiesen` = der AUSGANG. Er ueberlebt das Ausschalten EINER Schicht, denn es
    //   sind drei (Politik-Gatter, HAL-Gatter, Vorgabe-Nein). Das ist die Tiefe.
    // * `Ringwort-Grund-RING` = der GRUND. Er faellt, sobald das Politik-Gatter fehlt -- auch wenn
    //   der Schreibversuch weiterhin abgewiesen wird. Das ist die Schaerfe.
    //
    // Mit nur der ersten Zeile war die Messung stumpf: „abgewiesen weil Ringwort" und „abgewiesen
    // weil unklassifiziert" sahen gleich aus, und keine Mutation am Ringgatter konnte sie bewegen.
    // Mit nur der zweiten waere die Tiefe unbelegt -- man saehe nicht, dass der Schreibversuch
    // auch ohne den Grund scheitert.
    // **Die Indizes kommen aus der TABELLE, nicht aus dem Kopf.**
    //
    // Die erste arch-neutrale Fassung schrieb hartkodiert auf Wort 18 (x86: `cs`) und 17 (x86:
    // `rip`). Auf aarch64 sind das **Allzweckregister** -- die Sonde schrieb sie erfolgreich und
    // meldete `Ringwort-abgewiesen=false`, also einen Fehler, den es nicht gab, waehrend sie den
    // Fall, um den es geht, gar nicht mehr traf. Eine Sonde, die eine Registerlage NACHBILDET,
    // prueft eine zweite Wirklichkeit; `redirect::indizes` gibt es genau dafuer.
    let ix = caprock_sched::redirect::indizes(caprock_hal::exception::FRAME_ARCH);
    let ring_idx = ix.and_then(|i| i.ring.first().copied()).unwrap_or(usize::MAX);
    let pc_idx = ix.map(|i| i.pc).unwrap_or(usize::MAX);
    let gpr_idx = ix.map(|i| i.n_gpr / 2).unwrap_or(0);
    // **Sprechprobe der Tabelle selbst**: stimmen ihre Zahlen mit dem ueberein, was der Kernel
    // wirklich ablegt? Ohne sie waere sie eine zweite Wirklichkeit mit hübschem Namen.
    let indizes_stimmen = caprock_sched::redirect::indizes_stimmen(
        caprock_hal::exception::FRAME_ARCH,
        caprock_hal::exception::FRAME_GPR,
        caprock_hal::exception::FRAME_WOERTER,
    );
    let ring_vorher = system::DEBUG_WR_RING.load(Ordering::Relaxed);
    let ring_ausgang = system::debug_write_reg(0, ctrl, raw, ring_idx as u64, 0x33);
    let ring_abgewiesen = ring_ausgang == Err(caprock_abi::result::ERR_RIGHTS);
    let ring_grund = system::DEBUG_WR_RING.load(Ordering::Relaxed) == ring_vorher + 1;
    let gpr_erlaubt =
        system::debug_write_reg(0, ctrl, raw, gpr_idx as u64, 0x1234_5678).is_ok();
    // Ein nicht-kanonischer PC faultete **im Kernel** beim `iretq` -- deshalb hat er eine eigene
    // Pruefung und ist keine Geschmacksfrage.
    let krummer_pc_abgewiesen =
        system::debug_write_reg(0, ctrl, raw, pc_idx as u64, 0x0001_0000_0000_0000)
            == Err(caprock_abi::result::ERR_RIGHTS);

    // --- 6b. Die STOPP-LATENZ -- p99 ueber >= 100 Stopps ---------------------------------------
    //
    // `steht=true` belegt, dass der Halt HAELT. Es belegt **nicht**, dass er im zugesagten Fenster
    // ankam -- und „<= 10 ms" ohne Messung ist eine Zusage, keine Eigenschaft.
    //
    // **Gemessen wird vom Setzen des Grundes bis zu dem Augenblick, in dem der Thread auf KEINEM
    // Kern mehr `current` ist.** Das ist die Groesse, die die Zusage nennt: `debug_stop` nimmt ihn
    // sofort aus der Ready-Queue, aber laeuft er gerade, haelt er erst beim naechsten
    // Kerneleintritt an -- dafuer ist der IPI da.
    //
    // **Mit EIGENEM Ziel auf einem FREMDEN Kern.** Die erste Fassung mass am Worker -- und der
    // liegt per Entwurf auf dem Bootkern, also auf dem Kern, den der Bericht faehrt; dort kann er
    // waehrend der Messung gar nicht laufen. Ergebnis war `SKIP`, jedes Mal, aus einem Grund, der
    // mit dem Debugger nichts zu tun hat. Eine Messung, deren wahrscheinlichstes Ergebnis „nicht
    // messbar" ist, gehoert eingerichtet und nicht wiederholt.
    //
    // **Eigene Namen, kein Shadowing der aeusseren.** Ein Block, der `tid`/`ctrl` ueberdeckt und
    // danach „zurueckstellt", ist genau die Form, bei der spaeter jemand die falsche Groesse
    // erwischt -- dieselbe Klasse wie ein Parameter mit zwei Bedeutungen.
    const LATENZ_N: usize = 128;
    const NACHLAUF: u64 = 2_000_000; // Obergrenze fuers Anhalten -- weit ueber einem Tick
    let mut proben = [0u64; LATENZ_N];
    let mut lief_beim_stopp = 0usize;
    let mein_kern = caprock_hal::cpu::core_id();

    let aufbau = (0..system::num_cores())
        .find(|&c| c != mein_kern && caprock_sched::core_online(c))
        .and_then(|c| {
            let lpd = system::create_pd()?;
            let lp = system::spawn_on_core_parked(
                c,
                latenz_ziel as *const () as usize,
                0,
                system::IDLE_PRIO,
            )?;
            system::bind_pd_parked(&lp, lpd);
            let lt = system::admit(lp)?;
            let w = system::mint_debuggable(lpd)?;
            let sl = system::debug_attach(0, w, caprock_abi::debug::RIGHT_CONTROL).ok()?;
            Some((lt, (sl & 0xffff_ffff) as usize, w))
        });

    let ziel_kern = aufbau.and_then(|(t, _, _)| system::owner_core_of(t));
    let messbar = matches!((aufbau, ziel_kern), (Some(_), Some(c)) if c != mein_kern);

    if let (true, Some((lt, lc, lw))) = (messbar, aufbau) {
        let lraw = lt.to_raw();
        // **Gezaehlte Schleife, kein `iter_mut`.** Die Proben werden DICHT abgelegt (nur gueltige),
        // der Schreibindex ist also nicht der Schleifenindex -- ein Iterator ueber dasselbe Feld
        // waere ein zweiter, widersprechender Zugriff.
        for _ in 0..LATENZ_N {
            // Erst laufen lassen -- und **in TICKS warten, nicht in Schleifendurchlaeufen**.
            // Ein wiedereingereihter Thread wird auf einem fremden Kern erst beim naechsten Tick
            // eingeplant; jedes Spin-Budget unterhalb eines Ticks verfehlt das strukturell.
            // Wortgleich die Lehre aus `freeze_bericht`.
            let _ = system::debug_continue(0, lc, lraw);
            let t_start = caprock_hal::timer::ticks(0);
            while !system::thread_is_current(lt) && caprock_hal::timer::ticks(0) < t_start + 3 {
                core::hint::spin_loop();
            }
            let lief = system::thread_is_current(lt);
            let t0 = caprock_hal::timer::cycles();
            let _ = system::debug_stop(0, lc, lraw);
            let mut w2 = 0u64;
            while system::thread_is_current(lt) && w2 < NACHLAUF {
                core::hint::spin_loop();
                w2 += 1;
            }
            // **Nur Proben zaehlen, bei denen das Ziel wirklich lief.** Eine Null von einem
            // stehenden Thread ist kein schneller Stopp, sondern gar keiner -- sie zoege das p99
            // nach unten und rechnete die Zusage schoen.
            if lief {
                proben[lief_beim_stopp] = caprock_hal::timer::cycles().wrapping_sub(t0);
                lief_beim_stopp += 1;
            }
        }
        // Aufraeumen: laufen lassen und die Autoritaet abbauen. Eine Sonde, die ihren Gegenstand
        // angehalten zuruecklaesst, veraendert die Laeufe danach.
        let _ = system::debug_continue(0, lc, lraw);
        let _ = system::revoke_slot(0, lw);
        let _ = system::delete_slot(0, lw);
    }

    // Einfache Einfuegesortierung ueber die GUELTIGEN Proben -- 128 Werte, einmal je Lauf.
    for i in 1..lief_beim_stopp {
        let v = proben[i];
        let mut j = i;
        while j > 0 && proben[j - 1] > v {
            proben[j] = proben[j - 1];
            j -= 1;
        }
        proben[j] = v;
    }
    let n = lief_beim_stopp;
    let p50 = if n > 0 { proben[n / 2] } else { 0 };
    let p99 = if n > 0 { proben[(n * 99) / 100] } else { 0 };
    let pmax = if n > 0 { proben[n - 1] } else { 0 };
    // **Die Schwelle kommt aus DERSELBEN Quelle wie bei C9** (`cycles_per_sec()/TICK_HZ`), nicht
    // aus einer zweiten Rechnung. Und `tick == 0` ist ein Befund, kein Messwert: ein einseitiger
    // Vergleich waere gruen, sobald die Messung ausfaellt (F1/`NOSEL_TEXT`, wortgleich).
    let tick = crate::sperrmark::schwelle();
    let latenz_ok = !messbar || (tick > 0 && p99 <= tick && lief_beim_stopp >= 100);
    let promille = if tick > 0 { p99.saturating_mul(1000) / tick } else { 0 };

    // --- 6c. Der SCHLECHTE Fall -- und er ist nicht ungemessen, sondern woanders gemessen -------
    //
    // Die Reihe oben misst den guten Fall: ein spinnender Thread nimmt den IPI jederzeit an, das
    // p99 liegt bei Mikrosekunden. Die Zusage `<= 10 ms` gilt aber dem Kern, der gerade **nicht**
    // annehmen kann -- und die Groesse dahinter ist die laengste IRQ-maskierte Strecke im Kernel.
    //
    // **Genau die misst C9 seit dem 2026-08-12** (`sperre`-Zeile, `hoechststand_bereinigt`). Eine
    // eigene Sonde dafuer zu bauen hiesse, dieselbe Groesse ein zweites Mal zu messen -- und zwei
    // Zahlen fuer eine Tatsache laufen auseinander. Zusammengesetzt wird stattdessen:
    //
    //     Stopp-Latenz(schlecht)  <=  laengste maskierte Strecke  +  Latenz(gut)
    //
    // Beide Summanden sind gemessen, also ist die Schranke es auch. Geprueft wird die Summe gegen
    // EINEN Tick -- dieselbe Schwelle, dieselbe Quelle.
    //
    // **Was die Summe NICHT deckt, und das gehoert in die Zeile:** `sperre` sieht Maskierung unter
    // einer SPERRE. Ein von Hand gerufenes `local_irq_disable` und der Trap-Kontext selbst stehen
    // in keiner der beiden Messungen -- die `sperre`-Zeile sagt das ueber sich selbst, und diese
    // hier erbt den Vorbehalt, statt ihn zu verschweigen.
    // **Nur zusammensetzen, wo der Summand selbst gemessen UND gegattert ist.**
    //
    // Gemessen 2026-08-20: die `sperre`-Zeile (C9) laeuft **nur auf x86** -- auf aarch64 gibt es
    // sie nicht, und `hoechststand_bereinigt()` liefert dort trotzdem eine Zahl (5 446 459). Die
    // erste Fassung hat sie genommen und war prompt rot: 8720 Promille eines Ticks.
    //
    // Das waere ein Befund ueber den Debugger gewesen, und es ist keiner. Die Zahl stammt aus
    // einem Melder, den auf dieser Architektur **niemand prueft** -- weder kalibriert noch
    // gegattert. Eine Schranke aus einem ungeprueften Summanden ist keine Schranke, egal in welche
    // Richtung sie ausfaellt. Also **SKIP mit Grund**, und der Grund benennt die Vorbedingung:
    // solange C9 auf aarch64 nicht laeuft, ist die Zusage dort HERGELEITET und nicht gemessen.
    let c9_laeuft = cfg!(target_arch = "x86_64") && crate::sperrmark::eichstand() > 0 && crate::sperrmark::tickrate() > 0;
    let maskiert_max = crate::sperrmark::hoechststand_bereinigt();
    let schlimmstfall = maskiert_max.saturating_add(p99);
    let schlimmstfall_ok = !messbar || !c9_laeuft || (tick > 0 && schlimmstfall <= tick);
    let sf_promille = if tick > 0 { schlimmstfall.saturating_mul(1000) / tick } else { 0 };

    // --- 7. Fortsetzen ------------------------------------------------------------------------
    let fortgesetzt = system::debug_continue(0, ctrl, raw).is_ok();
    let c0 = peek(baton);
    warten();
    let c1 = peek(baton);
    let laeuft_wieder = c1 > c0;

    // --- 8. Ein Revoke darf das Ziel NICHT unbrauchbar machen ---------------------------------
    //
    // Der Kern von Z6b §6. Nur der Debugger entfernt `DEBUG`; faellt seine Autoritaet weg,
    // traegt das Ziel einen Grund, den niemand mehr entfernen darf. Gemessen wird wieder die
    // **Wirkung**: der Zaehler muss nach dem Revoke wieder laufen.
    let _ = system::debug_stop(0, ctrl, raw);
    let d0 = peek(baton);
    warten();
    let stand_vor_revoke = peek(baton) == d0;
    let revoked = system::revoke_slot(0, wurzelslot);
    warten();
    let e1 = peek(baton);
    let revoke_bricht_nicht = e1 > d0;
    let freigaben = system::DEBUG_RELEASED.load(Ordering::Relaxed);

    // --- 9. Nach dem Revoke lebt KEIN Steuerrecht mehr -- die Wurzel aber schon ---------------
    //
    // **Meine erste Fassung hat hier `any_debug_authority_over` geprueft und ist durchgefallen**,
    // und der Code hatte recht: `revoke` loescht den TEILBAUM, nicht die vorgelegte Cap. Die
    // Wurzel bleibt, und sie soll bleiben -- wer sie haelt, leitet jederzeit neu ab. Das ist genau
    // die Custody-Aussage: ein Freigabefenster endet nicht damit, dass die abgeleiteten Rechte
    // ablaufen, sondern damit, dass die WURZEL geht.
    let kein_steuerrecht_mehr = !system::any_debug_control_over(zielpd);
    let wurzel_lebt_noch = system::any_debug_authority_over(zielpd);

    // --- 9b. Der ABSTURZ einer Debugger-PD -- gemessen, nicht gelesen -------------------------
    //
    // Bis hierher waren `revoke` und ein einzelnes `cap_delete` gemessen. Der Fall, um den es
    // wirklich geht, ist ein dritter: **die Debugger-PD stirbt**, waehrend sie ihr Ziel haelt.
    // `destroy_pd` loescht jeden ihrer Caps einzeln ueber `cap_delete` -- also ueber denselben
    // Sammler, aber ueber den anderen Pfad. Dass das so ist, war gelesen; hier wird es gefahren.
    let (fremd_ok, fremd_gestoppt, fremd_stand, fremd_laeuft_wieder) =
        match system::create_pd() {
            Some(fremd) => match system::debug_attach_to(0, wurzelslot, fremd) {
                Some(fslot) => {
                    let g = system::debug_stop(fremd, fslot, raw).is_ok();
                    let f0 = peek(baton);
                    warten();
                    let stand = peek(baton) == f0;
                    // **Der Absturz.** Kein Revoke, kein einzelnes delete -- die ganze PD geht.
                    system::destroy_pd(fremd);
                    warten();
                    let wieder = peek(baton) > f0;
                    (true, g, stand, wieder)
                }
                None => (false, false, false, false),
            },
            None => (false, false, false, false),
        };

    // --- 10. Der TEARDOWN-Pfad: `cap_delete`, nicht `cap_revoke` ------------------------------
    //
    // Eine sterbende Debugger-PD loescht ihre Caps **einzeln** (`cap_delete`), sie ruft kein
    // `revoke`. Zwei Pfade, eine Regel -- und die `cap_delete`-Kopie ist die, die altert, weil der
    // ausdrueckliche Revoke der Fall ist, an den beim Testen jeder zuerst denkt (K1a). Deshalb
    // steht er hier als **eigene** Zeile und nicht als Anhaengsel an Punkt 8.
    let wurzel_geloescht = system::delete_slot(0, wurzelslot);
    let danach_undebuggbar = !system::any_debug_authority_over(zielpd);

    let ok = laeuft
        && vorher_undebuggbar
        && abgeleitet
        && gestoppt
        && steht
        && busy
        && indizes_stimmen
        && ring_abgewiesen
        && ring_grund
        && latenz_ok
        && schlimmstfall_ok
        && gpr_erlaubt
        && krummer_pc_abgewiesen
        && fortgesetzt
        && laeuft_wieder
        && stand_vor_revoke
        && revoked
        && revoke_bricht_nicht
        && kein_steuerrecht_mehr
        && wurzel_lebt_noch
        && wurzel_geloescht
        && danach_undebuggbar
        && fremd_ok
        && fremd_gestoppt
        && fremd_stand
        && fremd_laeuft_wieder
        && freigaben > 0;

    println!(
        "dbg     : laeuft-vorher={laeuft} ({a0}->{a1}) vorher-undebuggbar={vorher_undebuggbar}          (keine-Autoritaet={keine_autoritaet} alle-{versucht}-Slots-abgewiesen={alle_slots_abgewiesen})          abgeleitet={abgeleitet} gestoppt={gestoppt} steht={steht} ({b0}->{b1})"
    );
    println!(
        "dbg     : Indizes-stimmen={indizes_stimmen} (Ring={ring_idx} PC={pc_idx} GPR={gpr_idx}) zweiter-Halter-BUSY={busy} Ringwort-abgewiesen={ring_abgewiesen} Ringwort-Grund-RING={ring_grund} GPR-erlaubt={gpr_erlaubt} krummer-PC-abgewiesen={krummer_pc_abgewiesen} fortgesetzt={fortgesetzt} laeuft-wieder={laeuft_wieder} ({c0}->{c1})"
    );
    println!(
        "dbg     : stand-vor-revoke={stand_vor_revoke} revoked={revoked} revoke-bricht-nicht={revoke_bricht_nicht} ({d0}->{e1}) freigaben={freigaben} kein-Steuerrecht-mehr={kein_steuerrecht_mehr} Wurzel-lebt-noch={wurzel_lebt_noch} Wurzel-geloescht-cap_delete={wurzel_geloescht} danach-undebuggbar={danach_undebuggbar}"
    );
    println!(
        "dbg     : Stopp-Latenz {} -- gueltige Proben {lief_beim_stopp}/{LATENZ_N} (gefordert 100) \
         p50={p50} p99={p99} max={pmax} Zyklen, p99 = {promille} Promille eines Ticks \
         (Schwelle {tick}, aus DERSELBEN Quelle wie C9). Ziel auf Kern {:?}, Bericht auf Kern \
         {mein_kern}. **Gezaehlt werden nur Proben, bei denen das Ziel wirklich LIEF** -- eine \
         Null von einem stehenden Thread ist kein schneller Stopp, sondern gar keiner, und sie \
         wuerde die Zusage schoenrechnen. Der Halt greift beim naechsten KERNELEINTRITT, nicht \
         mitten in einer Instruktion. **WAS DIESE REIHE NICHT MISST:** die Zusage `<= 10 ms` ist \
         die OBERE Schranke fuer einen Zielkern, der gerade maskiert hat oder in einem langen \
         kritischen Abschnitt steht -- ein spinnender Thread nimmt den IPI jederzeit an, deshalb \
         liegt das p99 hier bei Mikrosekunden. Die Schranke bleibt aus dem Tick HERGELEITET, nicht \
         an ihrer Grenze gemessen; '0 Promille' belegt den guten Fall, nicht den schlechten",
        if messbar { "gemessen" } else { "SKIP (Ziel auf DEM Kern, der den Bericht faehrt -- dort kann es waehrend der Messung nicht laufen; das ist nicht entscheidbar und deshalb weder gruen noch rot)" },
        ziel_kern
    );
    println!(
        "dbg     : Stopp-Latenz SCHLIMMSTFALL {}: laengste \
         IRQ-maskierte Strecke {maskiert_max} Zyklen (C9/`sperre`, hoechststand_bereinigt) + p99 \
         {p99} = {schlimmstfall} Zyklen = {sf_promille} Promille eines Ticks, Schwelle {tick}. \
         **Beide Summanden sind gemessen, also ist die Schranke es auch** -- eine eigene Sonde \
         fuer die maskierte Strecke waere eine zweite Zahl fuer dieselbe Tatsache. NICHT gedeckt: \
         `sperre` sieht Maskierung unter einer SPERRE; ein von Hand gerufenes local_irq_disable \
         und der Trap-Kontext stehen in keiner der beiden Messungen",
        if c9_laeuft { "(zusammengesetzt, nicht zweitgemessen)" } else { "SKIP -- C9/`sperre` laeuft auf dieser Architektur NICHT, der Summand ist also ungeprueft; eine Schranke aus einem ungeprueften Summanden ist keine" }
    );
    println!(
        "dbg     : Debugger-PD-Absturz: aufgebaut={fremd_ok} gestoppt={fremd_gestoppt} \
         stand={fremd_stand} laeuft-wieder-nach-destroy_pd={fremd_laeuft_wieder} \
         (destroy_pd loescht je Cap ueber cap_delete -- der DRITTE Pfad neben revoke und dem \
         einzelnen delete, und der einzige, der im Betrieb wirklich vorkommt)"
    );
    println!(
        "dbg     : stops={} busy={} continues={} attaches={} nicht-debuggbar-abgewiesen={}          wr(ok={} ring={} stufe={} wert={})",
        system::DEBUG_STOPS.load(Ordering::Relaxed),
        system::DEBUG_BUSY.load(Ordering::Relaxed),
        system::DEBUG_CONTINUES.load(Ordering::Relaxed),
        system::DEBUG_ATTACHES.load(Ordering::Relaxed),
        system::DEBUG_NOT_DEBUGGABLE.load(Ordering::Relaxed),
        system::DEBUG_WR_OK.load(Ordering::Relaxed),
        system::DEBUG_WR_RING.load(Ordering::Relaxed),
        system::DEBUG_WR_LEVEL.load(Ordering::Relaxed),
        system::DEBUG_WR_VALUE.load(Ordering::Relaxed),
    );
    println!(
        "dbg     : {} (Z6b: Debug-Autoritaet ist eine Cap ueber GENAU EINE PD. Die tragende Zeile          ist 'vorher-undebuggbar': eine Sonde mit JEDER anderen Cap des Systems faehrt alle          Cap-Slots der Wurzel-PD durch und wird jedes Mal abgewiesen -- DAS ist die Aussage, die          ptrace nicht treffen kann. Gemessen wird durchweg die WIRKUNG am Rundenzaehler, nie ein          Rueckgabewert: 'angehalten' ist eine Behauptung, 'der Zaehler steht ueber drei Ticks'          eine Messung. NICHT gemessen in dieser Fassung: das Lesen von Zielspeicher          (DEBUG_READ_MEM) -- der Pfad ist gebaut, die Sonde dafuer nicht, und eine ungemessene          Zeile wird hier nicht gruen behauptet)",
        if ok { "ALL PASS" } else { "FAILURES" }
    );
    // **Bewusst KEIN Konjunkt in `all_done()`** -- genau wie bei `freeze_bericht`, und ich habe
    // den Beleg dafuer heute selbst produziert: mit dem Konjunkt meldete der Lauf
    // `bringup : offen waren: sweep dbg verif` und lief in den Watchdog, WAEHREND die Zeile
    // darueber `dbg : ALL PASS` sagte. Was der Bericht setzt, kann den Bericht nicht ausloesen --
    // die Falle aus A-6.1, zum dritten Mal.
    //
    // Gegattert wird die Zeile trotzdem, nur eine Ebene hoeher: `test-qemu-x86.sh` prueft sie mit
    // `check`. Eine Zeile, die niemand liest, ist die Form, die dieses Projekt bei `pdbind`
    // bezahlt hat (`pdbind : FAILURES` bei `== ALL PASS ==`).
    DBG_OK.gemessen(ok);
}
