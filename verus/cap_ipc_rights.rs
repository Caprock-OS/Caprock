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
// rules travel unchanged from rule-less parents); transition-preservation over `Seq`
// updates is the named follow-up — this file proves the decision lemmas, not the
// frame. What it does NOT model (named, like everywhere in this tree): badges/ids,
// rights, budgets, revocation, and the `CALL`-time plumbing itself.
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

/// audit_cdt codes 9 + 10 as one invariant.
pub open spec fn ipc_rights_inv(t: Table) -> bool {
    forall|s: int|
        #![trigger t.slots[s]]
        live(t, s) ==> {
            let nd = t.slots[s];
            // (10) every standing rule is well-formed
            &&& (nd.chan_set ==> wellformed(nd.chan_value, nd.chan_mask))
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

} // verus!
