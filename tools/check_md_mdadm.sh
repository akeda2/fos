#!/bin/bash
# Cross-checks crates/md against the real Linux md driver, on loop devices backed by files in a temp folder.
# Run it as root (it needs losetup, mdadm, mount); it never touches a real disk:
#
#     (cd crates/md && cargo build --example probe) && sudo tools/check_md_mdadm.sh [CONFIG...]
#
# CONFIG is one of: raid1 raid0 raid10 raid5-ls raid5-la raid5-rs raid5-ra raid5-3. Default: all of them.
# For each, mdadm creates the array and an ext4 with a known file; then
#   1. probe assembles the members read-only: the dump must fsck clean and hold the file;
#   2. probe --write-test starts, writes and stops the array; mdadm must assemble it and fsck it clean;
#   3. (redundant levels) a member is blanked and rebuilt by probe --rebuild; mdadm must assemble the result,
#      its own "check" must find 0 mismatches, and the file must still be there.
set -u
cd "$(dirname "$0")/.."
PROBE=${PROBE:-crates/md/target/debug/examples/probe}
MD=/dev/md199
SIZE=96M
[ -x "$PROBE" ] || { echo "build the probe first: (cd crates/md && cargo build --example probe)"; exit 2; }
[ "$(id -u)" = 0 ] || { echo "run as root"; exit 2; }

work=$(mktemp -d /tmp/mdcheck.XXXXXX)
loops=()
cleanup() {
    mountpoint -q "$work/mnt" 2>/dev/null && umount "$work/mnt"
    mdadm --stop "$MD" >/dev/null 2>&1
    for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null; done
    rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/mnt"
fail=0
bad() { echo "  FAIL: $*"; fail=1; }
ok() { echo "  ok:   $*"; }

attach() {
    loops=()
    for f in "$@"; do loops+=("$(losetup -f --show "$f")"); done
}
detach() {
    for l in "${loops[@]}"; do losetup -d "$l"; done
    loops=()
}
fsck_ok() { e2fsck -fn "$1" >"$work/fsck.log" 2>&1 && ok "$2: e2fsck clean" || { bad "$2: e2fsck"; tail -5 "$work/fsck.log"; }; }

run() {
    name=$1
    case $name in
        raid1) level=1; n=2; extra="" ;;
        raid0) level=0; n=2; extra="--chunk=64" ;;
        raid10) level=10; n=4; extra="--chunk=64 --layout=n2" ;;
        raid5-ls) level=5; n=4; extra="--chunk=64 --layout=left-symmetric" ;;
        raid5-la) level=5; n=4; extra="--chunk=64 --layout=left-asymmetric" ;;
        raid5-rs) level=5; n=4; extra="--chunk=64 --layout=right-symmetric" ;;
        raid5-ra) level=5; n=4; extra="--chunk=64 --layout=right-asymmetric" ;;
        raid5-3) level=5; n=3; extra="--chunk=128" ;;
        *) echo "unknown config $name"; return ;;
    esac
    echo "== $name"
    files=()
    for i in $(seq 0 $((n - 1))); do
        f="$work/m$i.img"; truncate -s $SIZE "$f"; files+=("$f")
    done
    attach "${files[@]}"
    mdadm --create $MD --run --metadata=1.2 --bitmap=none --level=$level --raid-devices=$n $extra "${loops[@]}" >/dev/null 2>&1 || { bad "mdadm --create"; detach; return; }
    mdadm --wait $MD >/dev/null 2>&1
    mkfs.ext4 -q -F $MD
    mount $MD "$work/mnt"
    head -c 6M /dev/urandom >"$work/mnt/data"
    mkdir "$work/mnt/d" && cp "$work/mnt/data" "$work/mnt/d/copy"
    sum=$(sha256sum <"$work/mnt/data")
    umount "$work/mnt"
    mdadm --stop $MD >/dev/null
    detach

    out=$("$PROBE" --dump "$work/dump.img" "${files[@]}")
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q "not assembled" && { bad "probe did not assemble"; return; }
    echo "$out" | grep -q "DEGRADED\|dirty" && bad "unexpected state"
    echo "$out" | grep -q "[1-9][0-9]* sectors differ" && bad "mirrors differ"
    fsck_ok "$work/dump.img" "dump"
    mount -o ro,loop "$work/dump.img" "$work/mnt" && [ "$(sha256sum <"$work/mnt/data")" = "$sum" ] && ok "dump holds the file" || bad "dump file"
    umount "$work/mnt"

    "$PROBE" --write-test "${files[@]}" | tail -1 | sed 's/^/    /'
    attach "${files[@]}"
    mdadm --assemble $MD "${loops[@]}" >/dev/null 2>&1 && ok "mdadm assembles after probe write-test" || bad "mdadm --assemble"
    mdadm --detail $MD | grep -E "State :" | sed 's/^/    /'
    fsck_ok $MD "after write-test"
    mdadm --stop $MD >/dev/null
    detach

    [ "$level" = 0 ] && return
    : >"${files[1]}"; truncate -s $SIZE "${files[1]}"
    out=$("$PROBE" "${files[@]}" | head -2)
    echo "$out" | sed 's/^/    /'
    "$PROBE" --rebuild 1 "${files[@]}" | sed 's/^/    /'
    attach "${files[@]}"
    mdadm --assemble $MD "${loops[@]}" >/dev/null 2>&1 && ok "mdadm assembles the rebuilt array" || bad "mdadm --assemble after rebuild"
    mdadm --detail $MD | grep -E "State :|Active Devices|Working Devices" | sed 's/^/    /'
    echo check >/sys/block/md199/md/sync_action
    mdadm --wait $MD >/dev/null 2>&1
    mm=$(cat /sys/block/md199/md/mismatch_cnt)
    [ "$mm" = 0 ] && ok "mdadm check: 0 mismatches" || bad "mdadm check: $mm mismatches"
    fsck_ok $MD "after rebuild"
    mount $MD "$work/mnt" && [ "$(sha256sum <"$work/mnt/data")" = "$sum" ] && ok "file intact after rebuild" || bad "file after rebuild"
    umount "$work/mnt"
    mdadm --stop $MD >/dev/null
    detach
}

configs=("$@")
[ ${#configs[@]} -gt 0 ] || configs=(raid1 raid0 raid10 raid5-ls raid5-la raid5-rs raid5-ra raid5-3)
for c in "${configs[@]}"; do run "$c"; done
[ $fail = 0 ] && echo "ALL OK" || echo "FAILURES"
exit $fail
