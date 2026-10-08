//! `ipcperm-cli` -- the client half of the tagged-IPC-capability end-to-end test.
//!
//! It holds the server's endpoint in slot 2 (untagged, as every client does), derives tagged
//! capabilities from it with `CCOPY` and calls the server through them. The server answers with
//! the permission mask and the id **the kernel delivered**, so every claim below is checked
//! against what the receiving side actually saw, not against what this program asked for alone.
//!
//! Six facts, each reported through its own badge on the manifest notification (the pattern
//! `init` and `wasmhost` use):
//!
//! | Badge | Fact |
//! |---|---|
//! | `UNTAGGED` | an untagged capability still delivers the plain badge (no permissions) |
//! | `TAGGED` | a tagged capability delivers exactly the requested mask and id |
//! | `ATTENUATED` | deriving again can only remove permissions, and the id is inherited |
//! | `DUP_GATED` | a tagged capability without `DUP` cannot be copied or derived from |
//! | `WIDE_ID` | an id that does not fit 48 bits is refused |
//! | `NOT_ENDPOINT` | only an endpoint capability can be tagged |

#![no_std]
#![no_main]

use libcaprock::{call, ccopy, ccopy_ipc, cdelete, ipc_perm, result, signal};

// Badge bits 46..51: free in the root and client notifications (32..36 root, 40..45 wasm/client).
// Must match `kernel/src/arch/x86_64/bringup.rs` (`IPCPERM_*`).
pub const UNTAGGED: u64 = 1 << 46;
pub const TAGGED: u64 = 1 << 47;
pub const ATTENUATED: u64 = 1 << 48;
pub const DUP_GATED: u64 = 1 << 49;
pub const WIDE_ID: u64 = 1 << 50;
pub const NOT_ENDPOINT: u64 = 1 << 51;

/// Manifest notification (carries the client badge) and the server's endpoint.
const NTFN: u64 = 1;
const EP: u64 = 2;
/// Scratch slots: 3 = first tagged cap, 4 = attenuated cap, 5 = report copy, 6 = refused copies.
const TAG1: u64 = 3;
const TAG2: u64 = 4;
const REPORT: u64 = 5;
const SCRATCH: u64 = 6;
/// `Rights::WRITE`: enough to `CALL`; the client does not need to receive.
const W: u64 = 2;

libcaprock::entry!(run);

/// Report one fact: a copy of the notification carrying `bit`, signalled once, then dropped.
fn report(bit: u64) {
    if ccopy(NTFN, REPORT, W, bit) == result::OK {
        signal(REPORT, 0);
        let _ = cdelete(REPORT);
    }
}

fn run(_arg: usize) -> ! {
    // "I am alive and can signal": the client badge itself, with no capability operation.
    signal(NTFN, 0);

    // 1. Untagged: the badge is the plain one (0 for the service endpoint), no permissions.
    let r = call(EP, [0; 4]);
    if r.result == result::OK && r.msg[0] == 0 && r.msg[1] == 0 {
        report(UNTAGGED);
    }

    // 2. Tagged: ask for mask 0b0101 | DUP and id 0x1234; the server must see exactly that.
    let mask1 = 0b0101 | ipc_perm::DUP;
    if ccopy_ipc(EP, TAG1, W, 0x1234, mask1) == result::OK {
        let r = call(TAG1, [0; 4]);
        if r.result == result::OK && r.msg[0] == mask1 as u64 && r.msg[1] == 0x1234 {
            report(TAGGED);
        }

        // 3. Attenuate: ask for more than the parent holds (0b1111 | GRANT). The result is
        //    parent & request = 0b0101 -- DUP was not requested, GRANT was never held -- and
        //    the id (0 = inherit) stays 0x1234.
        if ccopy_ipc(TAG1, TAG2, W, 0, 0b1111 | ipc_perm::GRANT) == result::OK {
            let r = call(TAG2, [0; 4]);
            if r.result == result::OK && r.msg[0] == 0b0101 && r.msg[1] == 0x1234 {
                report(ATTENUATED);
            }

            // 4. TAG2 has no DUP: neither a tagged derivation nor a plain copy may succeed.
            let a = ccopy_ipc(TAG2, SCRATCH, W, 0, 0b0001);
            let b = ccopy(TAG2, SCRATCH, W, 0);
            if a == result::ERR_RIGHTS && b == result::ERR_RIGHTS {
                report(DUP_GATED);
            }
        }
    }

    // 5. An id that does not fit 48 bits would collide with the permission half: refused.
    if ccopy_ipc(EP, SCRATCH, W, 1 << 48, 0b1) == result::ERR_RIGHTS {
        report(WIDE_ID);
    }

    // 6. A notification has no server to attest to: refused with the "wrong kind" code.
    if ccopy_ipc(NTFN, SCRATCH, W, 0, 0b1) == result::ERR_BADCAP {
        report(NOT_ENDPOINT);
    }

    libcaprock::exit();
}
