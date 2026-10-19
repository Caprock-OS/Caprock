// Caprock — Verus Tier 2: IPC rights model (permission mask + channel rule).
//
// Pins down the two decision functions behind `audit_cdt` codes 9 and 10 and states
// the table invariant they serve:
//
//   code 9:  the permission mask only ever shrinks along a derivation
//            (an untagged parent counts as ALL) — `tag_shrink_mono`.
//   code 10: a channel rule `(value, mask)` is well-formed wherever it stands
//            (`mask != 0`, value within mask) and the send-time check is decisive
//            (accepts its own value, rejects a witness) — `allows_is_decisive`.
//
// `ipc_rights_inv` states the table-level invariant (masks shrink against the parent,
// rules travel unchanged from rule-less parents); the transitions below prove it is
// preserved by every derivation. What it does NOT model (named, like everywhere in this
// tree): badges/ids, rights, budgets, revocation, and the `CALL`-time plumbing itself.
//
// Run: `verus --crate-type=lib verus/cap_ipc_rights.rs` (also picked up by
// `tools/verus-verify.sh`, which discovers every file with a `verus!` block).
use vstd::prelude::*;

verus! {

/// A capability slot: only the fields the two invariants observe.
pub struct Slot {
    pub used: bool,
    /// Whether this slot names an endpoint (only endpoints take rules/masks).
    pub endpoint: bool,
    /// Permission mask; `None` = untagged (counts as ALL).
    pub perms: Option<u16>,
    /// Channel rule; `None` = unrestricted.
    pub chan_value: u32,
    pub chan_mask: u32,
    pub chan_set: bool,
    /// Parent slot index (`spec` only); `u32::MAX` = root.
    pub parent: u32,
}

pub struct Table {
    pub slots: Seq<Slot>,
}

pub const NO_PARENT: u32 = 0xFFFF_FFFF;
pub const PERM_ALL: u16 = 0xFFFF;

/// Send-time check, exactly `ipc_chan::Rule::allows`.
pub open spec fn allows(value: u32, mask: u32, channel: u32) -> bool {
    (channel & mask) == value
}

/// Well-formedness, exactly `ipc_chan::Rule::is_wellformed`.
pub open spec fn wellformed(value: u32, mask: u32) -> bool {
    mask != 0 && (value & !mask) == 0
}

pub open spec fn live(t: Table, i: int) -> bool {
    0 <= i < t.slots.len() && t.slots[i].used
}

/// audit_cdt codes 9 + 10 as one invariant, plus the two structural clauses the
/// framing proof needs (both mirror audit code 4 and the install-time domain rule):
/// live slots link to live parents, and masks/rules only ever stand on endpoints.
pub open spec fn ipc_rights_inv(t: Table) -> bool {
    forall|s: int|
        #![trigger t.slots[s]]
        inv_clause(t, s)
}

/// One slot's clause (shared by the invariant and the preservation proof, so the
/// closing fold matches by unfolding).
pub open spec fn inv_clause(t: Table, s: int) -> bool {
    live(t, s) ==> {
        let nd = t.slots[s];
        // (10) every standing rule is well-formed
        &&& (nd.chan_set ==> wellformed(nd.chan_value, nd.chan_mask))
        // masks/rules only on endpoints (install rule + endpoint-only derivation)
        &&& (nd.endpoint || (!nd.chan_set && nd.perms is None))
        // live slots link to live parents (audit code 4 direction parent->live)
        &&& (nd.parent != NO_PARENT && nd.parent < t.slots.len() as u32
            ==> t.slots[nd.parent as int].used)
        // (9+10) against the parent: masks only shrink, rules never widen or appear
        // twice — a restricted child comes from an unrestricted parent, once.
        &&& (nd.parent != NO_PARENT && nd.parent < t.slots.len() as u32
            && t.slots[nd.parent as int].used ==> {
            let p = t.slots[nd.parent as int];
            // (9): untagged parent counts as ALL
            &&& (nd.perms is Some ==> {
                let have = if p.perms is Some { p.perms->Some_0 } else { PERM_ALL };
                (nd.perms->Some_0 & !have) == 0
            })
            // (10): the rule travels unchanged, and only from a rule-less parent
            &&& (nd.chan_set ==> !p.chan_set
                || (p.chan_value == nd.chan_value && p.chan_mask == nd.chan_mask))
        })
    }
}

/// Tagged derivation (`derive_ipc`): mask becomes `effective_parent & requested`.
/// The mask resulting from a tagged derivation never exceeds the parent.
pub proof fn tag_shrink_mono(parent: Option<u16>, requested: u16)
    ensures
        ({
            let have = if parent is Some { parent->Some_0 } else { PERM_ALL };
            let out = have & requested;
            (out & !have) == 0
        }),
{
    let have = if parent is Some { parent->Some_0 } else { PERM_ALL };
    assert(((have & requested) & !have) == 0) by (bit_vector);
}

/// Unfolding bridge: `wellformed` is exactly its two bitvector atoms. Proved by
/// unfolding alone (no bitvector reasoning needed — the atoms travel as atoms).
pub proof fn wellformed_unfolds(value: u32, mask: u32)
    requires
        wellformed(value, mask),
    ensures
        mask != 0,
        (value & !mask) == 0,
{
}

/// A well-formed rule accepts its own value and rejects a witness outside it —
/// the check is neither vacuous nor dead. (Existence, not universality: full
/// coverage of the 2^32 channel space is not a proof obligation.)
pub proof fn allows_is_decisive(value: u32, mask: u32)
    requires
        mask != 0,
        (value & !mask) == 0,
    ensures
        allows(value, mask, value),
        exists|w: u32| !allows(value, mask, w),
{
    // `by (bit_vector)` proves standalone tautologies (context is not bit-blasted),
    // so the hypotheses ride along as an implication — one closed formula. Note: the
    // witness must be inlined (`!value`, not a `let`): a bound name travels as a free
    // variable and the tactic forgets its equation.
    assert((mask == 0 || (value & !mask) != 0 || (value & mask) == value)) by (bit_vector);
    assert((mask == 0 || (value & !mask) != 0 || ((!value & mask) != value))) by (bit_vector);
    assert(!allows(value, mask, !value));
}

// ---- Transitions: derivations preserve `ipc_rights_inv` ----
//
// One shared frame for all three derivations (`copy`/`mint`, `derive_ipc`,
// `derive_channel`): the child is installed into a free slot `dst` (tables are
// boot-dimensioned and fixed-size — derivation reuses a free slot exactly like
// `CapSpace::alloc_slot`, which fails closed instead of growing). The
// preconditions are exactly the checks the code performs before allocating;
// the proof shows the audited invariant survives each of them.

/// Install a derived child into free slot `dst` (specification of allocation + link).
/// Total: out-of-range indices produce a well-typed but vacuous table — every use
/// is guarded by `derive_pre`, which the preservation proof carries in context.
pub open spec fn derive(
    t: Table,
    src: int,
    dst: int,
    perms: Option<u16>,
    rvalue: u32,
    rmask: u32,
    rset: bool,
) -> Table {
    Table {
        slots: t.slots.update(dst, Slot {
            used: true,
            endpoint: true,
            perms: perms,
            chan_value: rvalue,
            chan_mask: rmask,
            chan_set: rset,
            parent: src as u32,
        }),
    }
}

/// The preconditions every derivation checks before allocating: source and
/// destination in range, the source a live endpoint, the destination free, the
/// mask only shrinking against an untagged-or-wider parent, and a channel rule
/// arriving well-formed exactly once.
pub open spec fn derive_pre(
    t: Table,
    src: int,
    dst: int,
    perms: Option<u16>,
    rvalue: u32,
    rmask: u32,
    rset: bool,
) -> bool {
    &&& 0 <= src < t.slots.len()
    &&& 0 <= dst < t.slots.len()
    &&& t.slots.len() < 0xFFFF_FFFF
    &&& live(t, src)
    &&& t.slots[src].endpoint
    &&& !live(t, dst)
    &&& (perms is Some ==> {
        let have = if t.slots[src].perms is Some {
            t.slots[src].perms->Some_0
        } else {
            PERM_ALL
        };
        (perms->Some_0 & !have) == 0
    })
    &&& (rset ==> (wellformed(rvalue, rmask) && !t.slots[src].chan_set))
}

/// Non-vacuity witness (D18 discipline): the preconditions are satisfiable — a
/// proof whose requires no state meets proves nothing. Two slots suffice.
pub proof fn derive_pre_satisfiable()
    ensures
        exists|t: Table, src: int, dst: int|
            derive_pre(t, src, dst, Some(0b11), 7, 0xFFFF_FFFF, true),
{
    let root = Slot {
        used: true,
        endpoint: true,
        perms: None,
        chan_value: 0,
        chan_mask: 0,
        chan_set: false,
        parent: NO_PARENT,
    };
    let free = Slot {
        used: false,
        endpoint: false,
        perms: None,
        chan_value: 0,
        chan_mask: 0,
        chan_set: false,
        parent: NO_PARENT,
    };
    let t = Table { slots: seq![root, free] };
    assert(t.slots.len() == 2);
    assert(live(t, 0));
    assert(!live(t, 1));
    assert(t.slots[0].endpoint);
    assert(t.slots[0].perms is None);
    assert(!t.slots[0].chan_set);
    assert(0 <= 0 < t.slots.len() && 0 <= 1 < t.slots.len() && t.slots.len() < 0xFFFF_FFFF);
    assert(live(t, 0) && !live(t, 1));
    // mask branch, unfolded: untagged parent counts as ALL (bitvector fact)
    assert(((0b11 as u16) & !PERM_ALL) == 0) by (bit_vector);
    // rule branch, unfolded (bitvector facts, then fold into the spec fn)
    assert((0xFFFF_FFFFu32 != 0) && ((7u32 & !0xFFFF_FFFFu32) == 0)) by (bit_vector);
    assert(wellformed(7u32, 0xFFFF_FFFFu32));
    assert(derive_pre(t, 0, 1, Some(0b11), 7, 0xFFFF_FFFF, true));
}

/// Every checked derivation preserves the audited invariant: the fresh child
/// satisfies its clause by the preconditions; every other slot keeps its node
/// (`update` touches exactly one index), so its clause travels by framing.
pub proof fn derive_preserves(
    t: Table,
    src: int,
    dst: int,
    perms: Option<u16>,
    rvalue: u32,
    rmask: u32,
    rset: bool,
)
    requires
        ipc_rights_inv(t),
        derive_pre(t, src, dst, perms, rvalue, rmask, rset),
    ensures
        ipc_rights_inv(derive(t, src, dst, perms, rvalue, rmask, rset)),
{
    broadcast use vstd::seq::group_seq_axioms;
    let new = derive(t, src, dst, perms, rvalue, rmask, rset);
    // The two indices differ: the source is live, the destination is free.
    assert(src != dst);
    assert forall|s: int| #![trigger new.slots[s]] inv_clause(new, s) by {
        if s == dst {
            // The fresh child. Its node is exactly what the preconditions checked.
            assert(new.slots[dst] == Slot {
                used: true,
                endpoint: true,
                perms: perms,
                chan_value: rvalue,
                chan_mask: rmask,
                chan_set: rset,
                parent: src as u32,
            });
            assert(new.slots[src] == t.slots[src]);
            assert(t.slots[src].used);
            assert(inv_clause(t, src));
        } else {
            // Framing: untouched nodes travel identically, so the old clause —
            // instantiated at the same index — is the new clause.
            assert(new.slots[s] == t.slots[s]);
            assert(inv_clause(t, s));
        }
    }
    assert(ipc_rights_inv(new));
}

} // verus!
