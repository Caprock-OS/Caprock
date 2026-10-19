//! Host tests for the general IPC permission capability (tagged endpoint capabilities).
//!
//! An integration test and not a unit test, because it has to build a `CapSpace` from raw
//! slabs (`Slab::attach` is `unsafe`) and the library itself is `forbid(unsafe_code)`.

use caprock_abi::ipc_perm::{self, DUP, GRANT};
use caprock_cap::{CapError, CapSlot, CapSpace, Object};
use caprock_mem::{PhysAllocator, Rights};
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
fn untagged_capability_is_unchanged() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let minted = s.mint(ep, Rights::WRITE, 0xdead_beef_cafe).unwrap();
    // The plain badge is delivered, even above bit 48 -- the old behaviour.
    assert_eq!(s.wire_badge(minted), Some(0xdead_beef_cafe));
    assert_eq!(s.slot_perms(minted), Ok(None));
    assert!(s.may_dup(minted));
    assert!(s.may_grant(minted));
    assert_eq!(audit(&s), 0);
}

#[test]
fn derive_delivers_permissions_and_id_in_one_word() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let c = s.derive_ipc(ep, Rights::WRITE, 0x1234, 0b0101 | DUP).unwrap();
    let w = s.wire_badge(c).unwrap();
    assert_eq!(ipc_perm::id_of(w), 0x1234);
    // An untagged source counts as ALL, so the result is exactly the requested mask.
    assert_eq!(ipc_perm::perms_of(w), 0b0101 | DUP);
    assert_eq!(audit(&s), 0);
}

#[test]
fn permissions_only_shrink_along_a_derivation() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let a = s.derive_ipc(ep, Rights::WRITE, 1, 0b0111 | DUP).unwrap();
    // Ask for more than the parent has: the extra bits are silently dropped, never granted.
    let b = s.derive_ipc(a, Rights::WRITE, 0, 0b1111 | DUP | GRANT).unwrap();
    assert_eq!(s.slot_perms(b), Ok(Some(0b0111 | DUP)));
    // A narrower request narrows.
    let c = s.derive_ipc(b, Rights::WRITE, 0, 0b0001 | DUP).unwrap();
    assert_eq!(s.slot_perms(c), Ok(Some(0b0001 | DUP)));
    // Id inheritance with 0.
    assert_eq!(ipc_perm::id_of(s.wire_badge(c).unwrap()), 1);
    assert_eq!(audit(&s), 0);
}

#[test]
fn dup_and_grant_gate_further_derivation_and_transfer() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let no_dup = s.derive_ipc(ep, Rights::WRITE, 1, 0b11).unwrap();
    assert!(!s.may_dup(no_dup));
    assert!(!s.may_grant(no_dup));
    assert_eq!(
        s.derive_ipc(no_dup, Rights::WRITE, 0, 0b01),
        Err(CapError::Invalid),
        "a tagged capability without DUP cannot be derived from"
    );
    let with_grant = s.derive_ipc(ep, Rights::WRITE, 2, 0b11 | GRANT).unwrap();
    assert!(with_grant != no_dup);
    assert!(s.may_grant(with_grant));
    assert!(!s.may_dup(with_grant));
    assert_eq!(audit(&s), 0);
}

#[test]
fn copy_keeps_the_mask_and_mint_keeps_the_id_narrow() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let t = s.derive_ipc(ep, Rights::WRITE, 5, 0b11 | DUP).unwrap();
    let copy = s.copy(t, Rights::WRITE).unwrap();
    assert_eq!(s.slot_perms(copy), Ok(Some(0b11 | DUP)));
    // A tagged capability cannot be re-minted with an id that would collide with the mask bits.
    assert_eq!(s.mint(t, Rights::WRITE, 1 << 48), Err(CapError::Invalid));
    let ok = s.mint(t, Rights::WRITE, (1 << 48) - 1).unwrap();
    assert_eq!(ipc_perm::id_of(s.wire_badge(ok).unwrap()), (1 << 48) - 1);
    assert_eq!(audit(&s), 0);
}

#[test]
fn only_endpoints_with_a_nonempty_mask_and_a_narrow_id_qualify() {
    let mut s = space();
    let ntfn = s.install_notification(3, Rights::RW).unwrap();
    assert_eq!(s.derive_ipc(ntfn, Rights::WRITE, 1, 0b1), Err(CapError::Invalid));
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    assert_eq!(s.derive_ipc(ep, Rights::WRITE, 1, 0), Err(CapError::Invalid), "empty mask");
    assert_eq!(s.derive_ipc(ep, Rights::WRITE, 1 << 48, 0b1), Err(CapError::Invalid), "wide id");
    assert_eq!(audit(&s), 0);
}

#[test]
fn revoking_the_root_takes_every_tagged_child_with_it() {
    let mut s = space();
    let ep = s.install_endpoint(7, Rights::RW).unwrap();
    let a = s.derive_ipc(ep, Rights::WRITE, 1, 0b11 | DUP).unwrap();
    let _b = s.derive_ipc(a, Rights::WRITE, 2, 0b01).unwrap();
    assert_eq!(s.used_slots(), 3);
    let mut items = [(0u32, 0u64); 8];
    let mut dma = [(0u64, 0u64); 8];
    let mut fin = caprock_cap::Finalized::new(&mut items, &mut dma);
    let mut alloc = PhysAllocator::new();
    s.revoke(&mut alloc, ep, &mut fin).unwrap();
    assert_eq!(s.used_slots(), 1, "only the root remains");
    assert_eq!(audit(&s), 0);
}
