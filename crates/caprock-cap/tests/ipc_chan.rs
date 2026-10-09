//! Host tests for the send-time channel rule (`CHAN = 38`, `caprock_abi::ipc_chan`).
//!
//! An integration test and not a unit test, because it has to build a `CapSpace` from raw
//! slabs (`Slab::attach` is `unsafe`) and the library itself is `forbid(unsafe_code)`.
//! The audit-code-10 negative case (a malformed rule in the live table) cannot be built
//! through the public API — it is covered from inside (`space.rs` unit test), because a
//! checker without a failing case would be decoration.

use caprock_abi::ipc_chan::{channel_of_tag, Rule};
use caprock_abi::ipc_perm::{self, DUP};
use caprock_cap::{CapError, CapSlot, CapSpace, Object};
use caprock_mem::Rights;
use caprock_slab::Slab;

const N: usize = 64;

fn space() -> CapSpace {
    let slots_mem: &'static mut [CapSlot] = Box::leak(vec![CapSlot::EMPTY; N].into_boxed_slice());
    let objs_mem: &'static mut [Object] = Box::leak(vec![Object::EMPTY; N].into_boxed_slice());
    let mut slots = Slab::empty();
    let mut objs = Slab::empty();
    // SAFETY: the leaked boxes live for the whole test process, are exclusive, and are attached
    // exactly once.
    unsafe {
        slots.attach(slots_mem.as_mut_ptr(), N, |_| CapSlot::EMPTY);
        objs.attach(objs_mem.as_mut_ptr(), N, |_| Object::EMPTY);
    }
    let mut s = CapSpace::new();
    s.attach(slots, objs);
    s
}

fn audit(s: &CapSpace) -> u32 {
    let mut refs = vec![0u32; s.finalize_capacity()];
    s.audit_cdt(&mut refs)
}

#[test]
fn unrestricted_slot_has_no_rule() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    assert_eq!(s.slot_chan(ep), Ok(None));
    assert_eq!(audit(&s), 0);
}

#[test]
fn derive_channel_stores_a_wellformed_rule() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let c = s.derive_channel(ep, Rights::WRITE, 7, 0xFFFF_FFFF).unwrap();
    assert_eq!(
        s.slot_chan(c),
        Ok(Some(Rule { value: 7, mask: 0xFFFF_FFFF }))
    );
    // The rule is enforced by `Rule::allows`, the same function the dispatch uses.
    let rule = s.slot_chan(c).unwrap().unwrap();
    assert!(rule.allows(channel_of_tag(7)));
    assert!(!rule.allows(channel_of_tag(8)));
    assert_eq!(audit(&s), 0);
}

#[test]
fn malformed_rules_are_refused() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    // mask == 0: matches everything or nothing by accident, never a rule.
    assert_eq!(
        s.derive_channel(ep, Rights::WRITE, 0, 0),
        Err(CapError::Invalid)
    );
    // value bits outside the mask can never match: a caller bug, not a rule.
    assert_eq!(
        s.derive_channel(ep, Rights::WRITE, 0x1_0000, 0xFFFF),
        Err(CapError::Invalid)
    );
    // no rule was installed by the refusals.
    assert_eq!(s.slot_chan(ep), Ok(None));
    assert_eq!(audit(&s), 0);
}

#[test]
fn only_endpoint_capabilities_take_a_rule() {
    let mut s = space();
    let ntfn = s.install_notification(3, Rights::RW).unwrap();
    assert_eq!(
        s.derive_channel(ntfn, Rights::WRITE, 1, 0xFFFF_FFFF),
        Err(CapError::Invalid)
    );
    assert_eq!(audit(&s), 0);
}

#[test]
fn second_rule_is_refused_never_widened() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let a = s.derive_channel(ep, Rights::WRITE, 0x1200, 0xFF00).unwrap();
    // A second rule would have to intersect two (value, mask) pairs in general —
    // narrowing that means something else silently. Refuse; derive from the parent.
    assert_eq!(
        s.derive_channel(a, Rights::WRITE, 0x12AB, 0xFFFF),
        Err(CapError::Invalid)
    );
    assert_eq!(
        s.slot_chan(a),
        Ok(Some(Rule { value: 0x1200, mask: 0xFF00 }))
    );
    assert_eq!(audit(&s), 0);
}

#[test]
fn copy_and_derive_ipc_preserve_the_rule() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let a = s.derive_channel(ep, Rights::WRITE, 9, 0xFFFF_FFFF).unwrap();
    let b = s.copy(a, Rights::READ).unwrap();
    assert_eq!(s.slot_chan(b), s.slot_chan(a));
    // A tagged derivation keeps the rule: restrictions only ever travel with the cap.
    let c = s.derive_ipc(a, Rights::WRITE, 0x1234, 0b0101 | DUP).unwrap();
    assert_eq!(s.slot_chan(c), s.slot_chan(a));
    assert_eq!(ipc_perm::id_of(s.wire_badge(c).unwrap()), 0x1234);
    assert_eq!(audit(&s), 0);
}

#[test]
fn tagged_source_without_dup_cannot_take_a_rule() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let no_dup = s.derive_ipc(ep, Rights::WRITE, 1, 0b11).unwrap();
    assert!(!s.may_dup(no_dup));
    assert_eq!(
        s.derive_channel(no_dup, Rights::WRITE, 1, 0xFFFF_FFFF),
        Err(CapError::Invalid)
    );
    let with_dup = s.derive_ipc(ep, Rights::WRITE, 2, 0b11 | DUP).unwrap();
    assert!(s.derive_channel(with_dup, Rights::WRITE, 1, 0xFFFF_FFFF).is_ok());
    assert_eq!(audit(&s), 0);
}
