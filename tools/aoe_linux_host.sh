#!/usr/bin/env bash
# The host side of docs/block-sharing.md section 9, "Testing against Linux": steps 1-7 against /dev/etherd/e5.2,
# with the guest started by `make run-mf-aoe-tap` in another terminal. Run from the repository root, as yourself (it
# calls sudo for each root step). It stops once, for the guest commands that make the export writable.
#
# It only ever writes to /dev/etherd/e5.2, and only after checking that the device holds exactly the scratch disk
# tools/aoe_linux.py made (same size, same SHA-256), and that the aoe module is bound to fosaoe0 alone.
# Everything it prints also goes to build/aoe-linux/host.log: paste that back.
set -u

DEV=/dev/etherd/e5.2
WORK=build/aoe-linux
IMG=$WORK/data.img
LOG=$WORK/host.log
TAP=fosaoe0

[ -f "$IMG" ] && [ -f "$WORK/data.sha256" ] || { echo "No $IMG: run make run-mf-aoe-tap first (terminal A)."; exit 1; }
exec > >(tee "$LOG") 2>&1

say() { printf '\n== %s\n' "$*"; }
fail() { echo "STOP: $*"; exit 1; }
wait_dev() {   # discover, then up to 10 s for the device node
    echo | sudo tee /dev/etherd/discover > /dev/null
    for _ in $(seq 20); do [ -b "$DEV" ] && return 0; sleep 0.5; done
    return 1
}
dd_rate() { sudo dd "$@" 2>&1 | tail -1; }

expected=$(cut -d' ' -f1 "$WORK/data.sha256")
size=$(stat -c %s "$IMG")
echo "aoe host test, $(date -Is), $(uname -r)"
echo "scratch disk: $IMG, $size bytes, SHA-256 $expected"

say "0. module and interface"
lsmod | grep '^aoe ' || fail "the aoe module is not loaded"
iflist=$(cat /sys/module/aoe/parameters/aoe_iflist 2>/dev/null)
echo "aoe_iflist: '$iflist'"
[ "$(echo $iflist)" = "$TAP" ] || fail "aoe_iflist must be exactly $TAP (it would broadcast AoE elsewhere)"
ip -br link show "$TAP" || fail "no $TAP"
sudo dmesg | grep -i aoe | tail -5

say "1. discovery"
wait_dev || { sudo dmesg | grep -i aoe | tail -10; fail "$DEV did not appear within 10 s"; }
ls -l /dev/etherd/
sudo dmesg | grep -i aoe | tail -5

say "2. size and content through AoE"
got_size=$(sudo blockdev --getsize64 "$DEV")
echo "size: $got_size (expect $size)"
[ "$got_size" = "$size" ] || fail "size differs: not the scratch disk?"
got=$(sudo sha256sum "$DEV" | cut -d' ' -f1)
echo "SHA-256: $got"
[ "$got" = "$expected" ] && echo "digest: same" || fail "digest differs from data.sha256"

say "3. read throughput (64 MiB, O_DIRECT)"
dd_rate if="$DEV" of=/dev/null bs=1M iflag=direct

say "4. the export is read-only"
if sudo dd if=/dev/zero of="$DEV" bs=4k count=1 oflag=direct 2>&1; then
    echo "UNEXPECTED: the write was accepted"
else
    echo "write refused (expected)"
fi
sudo blockdev --flushbufs "$DEV"
got=$(sudo sha256sum "$DEV" | cut -d' ' -f1)
[ "$got" = "$expected" ] && echo "digest after the refused write: same" || echo "UNEXPECTED: digest now $got"

cat > /dev/tty <<EOF

---------------------------------------------------------------------------
In terminal A (the guest shell), type:
    aoe                       (copy its output: the counters after step 3)
    aoe unexport 2
    aoe export 2 rw confirm
then press Enter here.
---------------------------------------------------------------------------
EOF
read -r _ < /dev/tty
echo "(guest switched to read-write by hand)"

say "re-discovery after the re-export"
wait_dev || fail "$DEV is gone after the re-export"
sudo dmesg | grep -i aoe | tail -5
got=$(sudo sha256sum "$DEV" | cut -d' ' -f1)
[ "$got" = "$expected" ] || fail "digest differs before writing: not touching it"
echo "still the scratch disk; writing from here on"

say "5. raw writes: 8 MiB at 16 MiB"
pattern=$(mktemp)
head -c 8M /dev/urandom > "$pattern"
dd_rate if="$pattern" of="$DEV" bs=1M seek=16 oflag=direct conv=fsync
sudo blockdev --flushbufs "$DEV"
sudo cmp -n 8388608 -i 0:16777216 "$pattern" "$DEV" && echo "read back through AoE: same"
sleep 1
cmp -n 8388608 -i 0:16777216 "$pattern" "$IMG" && echo "in the disk image: same"
rm -f "$pattern"

say "6. ext4 through AoE"
mnt=$(mktemp -d)
sudo mkfs.ext4 -q -F "$DEV" && echo "mkfs: done"
if sudo mount "$DEV" "$mnt"; then
    sudo cp -r docs crates/aoe/src "$mnt/"
    sync
    sudo diff -r docs "$mnt/docs" && sudo diff -r crates/aoe/src "$mnt/src" && echo "copied files: same"
    sudo umount "$mnt"
    sudo blockdev --flushbufs "$DEV"
    sudo mount -o ro "$DEV" "$mnt" && {
        sudo diff -r docs "$mnt/docs" && echo "after a remount: same"
        sudo umount "$mnt"
    }
    sudo blockdev --flushbufs "$DEV"
    sudo e2fsck -fn "$DEV"; echo "e2fsck exit: $?"
else
    echo "mount failed"
fi
rmdir "$mnt"

say "7. write throughput (32 MiB, O_DIRECT; overwrites the file system)"
dd_rate if=/dev/zero of="$DEV" bs=1M count=32 oflag=direct conv=fsync

say "module messages during the run"
sudo dmesg | grep -i aoe | tail -15

cat > /dev/tty <<EOF

---------------------------------------------------------------------------
Done. Type "aoe" in terminal A once more and paste its output together with
$LOG
---------------------------------------------------------------------------
EOF
