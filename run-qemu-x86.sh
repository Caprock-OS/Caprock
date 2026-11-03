#!/usr/bin/env bash
# Boot the Caprock kernel (x86_64) under QEMU.
#
# The x86_64 counterpart of run-qemu.sh. Target configuration: q35 machine,
# 4 cores, 4 GiB RAM. The kernel is booted as a Multiboot image (mb32).
# KVM with the host CPU is enabled automatically when /dev/kvm is usable;
# otherwise QEMU falls back to TCG with Skylake-Client (the suite's
# fallback model — see test-qemu-x86.sh for the host-cache-info rationale).
# Exit QEMU with Ctrl-A then X.
set -euo pipefail
cd "$(dirname "$0")"

KERNEL="build/target/x86_64-unknown-none/release/caprock-kernel.mb32"
[ -f "$KERNEL" ] || { echo "kernel not built — run ./build-x86.sh first" >&2; exit 1; }

# KVM, if available. `+invtsc` must be requested explicitly (QEMU leaves it
# out even with `-cpu host` because it blocks live migration) and
# `host-cache-info=on` so the real cache geometry is reported instead of the
# synthetic legacy descriptors. Under TCG, `Skylake-Client` is used instead of
# the bare `qemu64` (which lacks CPUID leaf 4 and x2APIC).
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL=(-enable-kvm -cpu host,+invtsc,host-cache-info=on)
else
    ACCEL=(-cpu Skylake-Client)
fi

exec qemu-system-x86_64 \
    -machine q35 \
    -smp 4 \
    -m 4G \
    "${ACCEL[@]}" \
    -nographic \
    -serial mon:stdio \
    -kernel "$KERNEL" \
    "$@"
