#!/usr/bin/env bash
# Runs both x86_64 QEMU suites and gives one verdict.
#
# Why two suites: `test-qemu-x86.sh` deliberately boots WITHOUT a boot archive and asserts that
# the kernel then names the reason (`root : FAILURES (NoArchive)`, B-1.5), so its summary lists
# `archive` and `root` as "known red". Those lines are an expected absence, not a defect. The
# archive, the manifest and the root task are exercised by `test-qemu-x86-load.sh`. Neither suite
# covers the other's half, so a verdict about "the x86 kernel" needs both.
#
# Usage: ./test-qemu-x86-all.sh [seconds-per-boot]   (default 180)
set -uo pipefail
cd "$(dirname "$0")"
SECS="${1:-180}"

./test-qemu-x86.sh "$SECS"
main_rc=$?
./test-qemu-x86-load.sh "$SECS"
load_rc=$?

echo
echo "== test-qemu-x86.sh      (no archive; archive/root are expected red): rc=$main_rc =="
echo "== test-qemu-x86-load.sh (archive, manifest, root task, drivers):      rc=$load_rc =="
if [ "$main_rc" -eq 0 ] && [ "$load_rc" -eq 0 ]; then
    echo "== BOTH SUITES PASS =="
    exit 0
fi
echo "== AT LEAST ONE SUITE FAILED =="
exit 1
