#!/bin/bash
# Cross-checks crates/md against the real Linux md driver, on loop devices backed by files in a temp folder.
# Run it as root (it needs losetup, mdadm, mount); it never touches a real disk:
#
#     (cd crates/md && cargo build --example probe) && sudo tools/check_md_mdadm.sh [CONFIG...]
#
# CONFIG is one of: raid1 raid0 raid10 raid5-ls raid5-la raid5-rs raid5-ra raid5-3 raid6-ls raid6-la raid6-rs raid6-ra raid6-5 raid1-bm raid5-bm raid6-bm (an internal
# write-intent bitmap, 1 MiB chunks) raid5-ppl (a partial parity log). Default: all of them.
# For each, mdadm creates the array and an ext4 with a known file; then
#   1. probe assembles the members read-only: the dump must fsck clean and hold the file;
#   2. probe --write-test starts, writes and stops the array; mdadm must assemble it and fsck it clean;
#   3. (redundant levels) a member is blanked and rebuilt by probe --rebuild; mdadm must assemble the result,
#      its own "check" must find 0 mismatches, and the file must still be there.
# With a bitmap, between 2 and 3: the write-test leaves 0 dirty bits and the bitmap's events level with the array's;
# probe --crash-test leaves the array open, mdadm must see dirty bits, and probe --resync (and, on another crash,
# mdadm's own resync of just those chunks) must bring them back to 0.
# With a partial parity log: probe --tear loses the parity write of a write and leaves the array open; probe --recover
# must replay the log (entries, 0 mismatched) and stop clean, then mdadm assembles it, "check" finds 0 mismatches and
# the file system is clean.
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

# dirty bits of the bitmap on the first loop device
dirty_bits() { mdadm --examine-bitmap "${loops[0]}" | sed -n 's/.*(chunks), \([0-9]*\) dirty.*/\1/p'; }
events_level() {
    a=$(mdadm --examine "${loops[0]}" | awk '$1=="Events"{print $3}')
    b=$(mdadm --examine-bitmap "${loops[0]}" | awk '$1=="Events"&&$2==":"{print $3}')
    [ "$a" = "$b" ] && ok "$1: bitmap events $b = array events $a" || bad "$1: bitmap events $b, array events $a"
}

bitmap_steps() {
    attach "${files[@]}"
    [ "$(dirty_bits)" = 0 ] && ok "write-test leaves 0 dirty bits" || bad "dirty bits after write-test: $(dirty_bits)"
    events_level "after write-test"
    detach
    "$PROBE" --crash-test "${files[@]}" | tail -1 | sed 's/^/    /'
    attach "${files[@]}"
    [ "$(dirty_bits)" -gt 0 ] 2>/dev/null && ok "mdadm sees $(dirty_bits) dirty bit(s) after the crash test" || bad "no dirty bits after the crash test"
    detach
    "$PROBE" --resync "${files[@]}" | tail -2 | sed 's/^/    /'
    attach "${files[@]}"
    [ "$(dirty_bits)" = 0 ] && ok "probe --resync clears the bits" || bad "dirty bits after probe --resync: $(dirty_bits)"
    events_level "after probe resync"
    mdadm --assemble $MD "${loops[@]}" >/dev/null 2>&1 && ok "mdadm assembles after probe resync" || bad "mdadm --assemble after resync"
    fsck_ok $MD "after probe resync"
    mdadm --stop $MD >/dev/null
    detach
    "$PROBE" --crash-test "${files[@]}" | tail -1 >/dev/null
    attach "${files[@]}"
    mdadm --assemble $MD "${loops[@]}" >/dev/null 2>&1 && ok "mdadm assembles the crashed array" || bad "mdadm --assemble after crash"
    mdadm --wait $MD >/dev/null 2>&1
    mdadm --stop $MD >/dev/null
    [ "$(dirty_bits)" = 0 ] && ok "mdadm's own resync clears the bits" || bad "dirty bits after mdadm's resync: $(dirty_bits)"
    detach
}

ppl_steps() {
    "$PROBE" --tear "${files[@]}" | tail -1 | sed 's/^/    /'
    out=$("$PROBE" --recover "${files[@]}")
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q "^ppl: [1-9][0-9]* entries replayed, 0 mismatched, array clean" && ok "probe replays the log and the array is clean" || bad "probe --recover"
    attach "${files[@]}"
    mdadm --assemble $MD "${loops[@]}" >/dev/null 2>&1 && ok "mdadm assembles after the recovery" || bad "mdadm --assemble after recovery"
    echo check >/sys/block/md199/md/sync_action
    mdadm --wait $MD >/dev/null 2>&1
    mm=$(cat /sys/block/md199/md/mismatch_cnt)
    [ "$mm" = 0 ] && ok "mdadm check after the torn write: 0 mismatches" || bad "mdadm check after the torn write: $mm mismatches"
    fsck_ok $MD "after the torn write"
    mdadm --stop $MD >/dev/null
    detach
}

run() {
    name=$1
    case $name in
        raid1|raid1-bm) level=1; n=2; extra="" ;;
        raid0) level=0; n=2; extra="--chunk=64" ;;
        raid10) level=10; n=4; extra="--chunk=64 --layout=n2" ;;
        raid5-ls) level=5; n=4; extra="--chunk=64 --layout=left-symmetric" ;;
        raid5-la) level=5; n=4; extra="--chunk=64 --layout=left-asymmetric" ;;
        raid5-rs) level=5; n=4; extra="--chunk=64 --layout=right-symmetric" ;;
        raid5-ra) level=5; n=4; extra="--chunk=64 --layout=right-asymmetric" ;;
        raid5-3) level=5; n=3; extra="--chunk=128" ;;
        raid5-ppl) level=5; n=3; extra="--chunk=64 --layout=left-symmetric --consistency-policy=ppl" ;;
        raid5-bm) level=5; n=3; extra="--chunk=64 --layout=left-symmetric" ;;
        raid6-ls) level=6; n=4; extra="--chunk=64 --layout=left-symmetric" ;;
        raid6-la) level=6; n=4; extra="--chunk=64 --layout=left-asymmetric" ;;
        raid6-rs) level=6; n=4; extra="--chunk=64 --layout=right-symmetric" ;;
        raid6-ra) level=6; n=4; extra="--chunk=64 --layout=right-asymmetric" ;;
        raid6-5) level=6; n=5; extra="--chunk=128" ;;
        raid6-bm) level=6; n=4; extra="--chunk=64 --layout=left-symmetric" ;;
        *) echo "unknown config $name"; return ;;
    esac
    bm=none; bmopt=""
    case $name in *-bm) bm=internal; bmopt="--bitmap-chunk=1024" ;; esac
    echo "== $name"
    files=()
    for i in $(seq 0 $((n - 1))); do
        f="$work/m$i.img"; truncate -s $SIZE "$f"; files+=("$f")
    done
    attach "${files[@]}"
    mdadm --create $MD --run --metadata=1.2 --bitmap=$bm $bmopt --level=$level --raid-devices=$n $extra "${loops[@]}" >/dev/null 2>"$work/create.err" || { bad "mdadm --create"; sed "s/^/      /" "$work/create.err"; detach; return; }
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

    [ "$bm" = none ] || bitmap_steps
    case $name in *-ppl) ppl_steps ;; esac
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
[ ${#configs[@]} -gt 0 ] || configs=(raid1 raid0 raid10 raid5-ls raid5-la raid5-rs raid5-ra raid5-3 raid6-ls raid6-la raid6-rs raid6-ra raid6-5 raid1-bm raid5-bm raid6-bm)
for c in "${configs[@]}"; do run "$c"; done
[ $fail = 0 ] && echo "ALL OK" || echo "FAILURES"
exit $fail
