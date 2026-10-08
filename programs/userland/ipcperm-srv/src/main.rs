//! `ipcperm-srv` -- the server half of the tagged-IPC-capability end-to-end test.
//!
//! The server trusts nothing a client sends. On every `RECV` it takes the badge word the kernel
//! wrote into `x1`, splits it with `caprock_abi::ipc_perm`, and replies with the two halves and
//! the raw word. The client compares them with what it asked for when it derived its capability;
//! if the kernel did not deliver the permission mask unchanged, or let the client influence it,
//! the comparison fails.

#![no_std]
#![no_main]

use libcaprock::{ipc_perm, recv, reply, result, yield_now};

/// Endpoint slot of a service PD (slot 1 is the manifest notification, slot 2 the endpoint).
const EP_SLOT: u64 = 2;

libcaprock::entry!(run);

fn run(_arg: usize) -> ! {
    loop {
        let r = recv(EP_SLOT);
        if r.result != result::OK {
            // Quiescing or an error: do not spin on the endpoint.
            yield_now();
            continue;
        }
        let wire = r.badge;
        reply(
            EP_SLOT,
            [ipc_perm::perms_of(wire) as u64, ipc_perm::id_of(wire), wire, 0],
        );
    }
}
