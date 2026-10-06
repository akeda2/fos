# fos v2: the UEFI loader and (later) the 64-bit kernel. Included from the top-level
# Makefile; every target here is prefixed v2- so plain `make` still builds v1 only.
# See docs/v2-design.md.

V2_BUILD := $(BUILD)/v2
V2_LOADER_EFI := $(CARGO_TARGET_DIR)/x86_64-unknown-uefi/release/fos-loader.efi
V2_KERNEL_ELF := $(CARGO_TARGET_DIR)/x86_64-fos-kernel/release/fos-v2-kernel
V2_IMAGE := $(V2_BUILD)/v2.img
# v2 tapes: crates under v2/tapes/, each built to build/cargo-target/x86_64-fos-tape/release/NAME.
V2_TAPES := hello calc memtest nettest bridge router ip-chat telnet net-chat ed record recorder play audio-net
V2_TAPE_BINS := $(foreach tape,$(V2_TAPES),$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape))
V2_TAPE_FILES := $(foreach tape,$(V2_TAPES),--file $(tape)=$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape)) --file debug=tools/default-debug.txt

# ---- Opus (stage C of audio-net): libopus built freestanding for the 64-bit tape target ----
# The vendored tree (tapes/opus-codec/vendor/opus, v1.5.2, fixed-point, trimmed to what the build needs) is compiled
# with the C compiler directly, position-independent and without the red zone or any SSE registers
# (-mgeneral-regs-only), and archived; a tape links the archive through RUSTFLAGS (which replaces the shared
# .cargo/config.toml rustflags, so those are repeated). See v2/tapes/opus-codec.
V2_OPUS_VENDOR := tapes/opus-codec/vendor/opus
V2_OPUS_SOURCES := $(wildcard $(V2_OPUS_VENDOR)/celt/*.c) $(wildcard $(V2_OPUS_VENDOR)/silk/*.c) $(wildcard $(V2_OPUS_VENDOR)/silk/fixed/*.c) $(wildcard $(V2_OPUS_VENDOR)/src/*.c)
V2_OPUS_OBJS := $(patsubst $(V2_OPUS_VENDOR)/%.c,$(V2_BUILD)/opus-fixed/%.o,$(V2_OPUS_SOURCES))
V2_OPUS_LIB := $(V2_BUILD)/opus-fixed/libopus_fixed.a
V2_OPUS_CFLAGS := -O2 -ffreestanding -fno-builtin -fno-stack-protector -fPIE -fno-asynchronous-unwind-tables -mgeneral-regs-only -mno-red-zone -ffunction-sections -fdata-sections -U_FORTIFY_SOURCE -DHAVE_CONFIG_H -Itapes/opus-codec -I$(V2_OPUS_VENDOR)/include -I$(V2_OPUS_VENDOR)/celt -I$(V2_OPUS_VENDOR)/silk -I$(V2_OPUS_VENDOR)/silk/fixed
V2_TAPE_LINK := -C link-arg=-T../tape.ld -C relocation-model=pic -C link-arg=-pie -C link-arg=--no-dynamic-linker -C link-arg=-znorelro


.PHONY: v2-opus-lib v2-tape-opus-bench v2-loader v2-kernel v2-image v2-run test-v2 test-v2-faults test-v2-keyboard test-v2-usb-boot test-v2-config test-v2-tapes test-v2-nettest test-v2-debug-mode test-v2-msi test-v2-router test-v2-nettapes test-v2-hub test-v2-disks test-v2-stick-guids test-v2-format test-v2-ed test-v2-diag test-v2-usb-audio test-v2-audio-net test-v2-audio-serve test-v2-record test-v2-recorder test-v2-recorder-fat test-v2-fat-shell test-v2-format-exfat test-v2-recorder-format test-v2-lineedit test-v2-scripts test-v2-nic test-v2-netfs

# The loader: a PE/COFF UEFI application (x86_64-unknown-uefi), built with the same
# pinned nightly and -Z build-std as everything else here.
v2-loader:
	cd v2/loader && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

# The kernel: an ELF64 at a fixed physical address (v2/kernel/linker.ld) for the
# custom x86_64-fos-kernel target.
v2-kernel:
	cd v2/kernel && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec

# Every v2 tape (ELF64 PIE executables for the 64-bit tape ABI).
.PHONY: v2-tapes
v2-tapes: $(V2_OPUS_LIB)
	for tape in $(V2_TAPES); do (cd v2/tapes/$$tape && cargo +nightly-2026-09-13 build --release) || exit 1; done

# A bootable disk image: GPT + a FAT32 EFI System Partition with \EFI\BOOT\BOOTX64.EFI
# and a payload partition with the kernel in slot 0 (v2/tools/build_image.py;
# standard library only).
v2-image: v2-loader v2-kernel v2-tapes
	mkdir -p $(V2_BUILD)
	python3 v2/tools/build_image.py --loader $(V2_LOADER_EFI) --kernel $(V2_KERNEL_ELF) --output $(V2_IMAGE) $(V2_TAPE_FILES)

# Boot the image under QEMU + OVMF with the serial console on the terminal.
v2-run: v2-image
	cp /usr/share/OVMF/OVMF_VARS_4M.fd $(V2_BUILD)/vars.fd
	qemu-system-x86_64 -machine q35 -m 512 -serial stdio -no-reboot \
		-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
		-drive if=pflash,format=raw,file=$(V2_BUILD)/vars.fd \
		-drive format=raw,file=$(V2_IMAGE)

# Loader + kernel under QEMU + OVMF, including a framebuffer screendump check.
test-v2: v2-image
	cargo +nightly-2026-09-13 test --manifest-path v2/abi/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path v2/fat/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path v2/tapes/audio-rec/Cargo.toml
	python3 v2/tools/test_boot.py

# Deliberate-fault kernels (one Cargo feature each): checks the exception reports,
# including a kernel stack overflow reported through the double-fault stack.
test-v2-faults: v2-loader v2-kernel
	python3 v2/tools/test_faults.py

# Local keyboard: PS/2 keys through the QEMU monitor, and a boot with no keyboard.
test-v2-keyboard: v2-image
	python3 v2/tools/test_keyboard.py

# Boot the image from a USB stick (qemu-xhci, then usb-ehci) and use it as the root disk.
test-v2-usb-boot: v2-image
	python3 v2/tools/test_usb_boot.py

# Keyboards and sticks behind USB hubs (one and two tiers) on qemu-xhci.
test-v2-hub: v2-image
	python3 v2/tools/test_usb_hub.py

# The diskN selector of ls/cat/write/rm, against a second and a blank disk.
test-v2-disks: v2-image
	python3 v2/tools/test_disks.py

# Random per-stick GUIDs (write_stick.py) and boot-disk discovery between look-alike sticks.
test-v2-stick-guids: v2-image
	python3 v2/tools/test_stick_guids.py

# `format`: blank, junk, foreign-GPT and FOSFS disks, the boot-disk refusal and the confirmation step.
test-v2-format: v2-image
	python3 v2/tools/test_format.py

# The ed line editor tape from the local keyboard (cursor keys, other volumes).
test-v2-ed: v2-image
	python3 v2/tools/test_ed.py

# cpuinfo, bootinfo, memmap, irqstat, pci read/write, io, mmio.
test-v2-diag: v2-image
	python3 v2/tools/test_diag.py

# USB audio class 1.0 against QEMU's usb-audio: discovery, a played tone recorded to WAV, capture.
test-v2-usb-audio: v2-image
	python3 v2/tools/test_usb_audio.py

# audio-net: a tone sent over raw Ethernet between two guests and recorded from the receiver's usb-audio.
test-v2-audio-net: v2-image
	python3 v2/tools/test_audio_net.py

# audio-net serve: Ogg Opus over HTTP from a guest, fetched from the host and decoded with ffmpeg.
test-v2-audio-serve: v2-image
	python3 v2/tools/test_audio_serve.py

# NETFS: foscopy.py and copy HOST/NAME between machines.
test-v2-netfs: v2-image
	python3 v2/tools/test_netfs.py

# nic sniff / discover / peers / map / use.
test-v2-nic: v2-image
	python3 v2/tools/test_nic.py

# Scripts and bare command names.
test-v2-scripts: v2-image
	python3 v2/tools/test_scripts.py

# The shell's line editor: history, cursor keys, Ctrl-keys, over the serial line.
test-v2-lineedit: v2-image
	python3 v2/tools/test_lineedit.py

# record: streaming a take to disk (a test tone through the real disk path), checked by parsing the image.
test-v2-record: v2-image
	python3 v2/tools/test_record.py

# recorder: meters and pre-roll from the start, manual and silence-triggered takes (test tone with a gate).
test-v2-recorder: v2-image
	python3 v2/tools/test_recorder.py

# recorder onto FAT32 and exFAT volumes (made by mkfs, checked by fsck and read back).
test-v2-recorder-fat: v2-image
	python3 v2/tools/test_recorder_fat.py

# ls/copy/rm on FAT32 and exFAT disks, and the clock.
test-v2-fat-shell: v2-image
	python3 v2/tools/test_fat_shell.py

# format diskN exfat: a blank disk becomes a GPT exFAT volume that fsck.exfat accepts.
test-v2-format-exfat: v2-image
	python3 v2/tools/test_format_exfat.py

# formatting a blank disk as exFAT from the recorder's UI.
test-v2-recorder-format: v2-image
	python3 v2/tools/test_recorder_format.py

# hostname / settings / debug / start files on the boot volume.
test-v2-config: v2-image
	python3 v2/tools/test_config.py

# Tapes: ELF64 PIE programs through the TapeApi table (hello, calc, a tape over the network).
test-v2-tapes: v2-image
	python3 v2/tools/test_tapes.py

# nettest between two e1000 NICs on host-played TCP segments.
test-v2-nettest: v2-image
	python3 v2/tools/test_nettest.py

# Crash records, reboot-on-crash, the loop guard, the watchdog, KUP and the A/B fallback.
test-v2-debug-mode: v2-image
	python3 v2/tools/test_debug_mode.py

# NIC interrupts: MSI on QEMU's e1000e (self-test, counters, hlt instead of spinning),
# the classic e1000 staying polled, and the no-msi setting.
test-v2-msi: v2-image
	python3 v2/tools/test_msi.py

# v1 end-to-end tests re-run against the v2 kernel through a qemu-system-i386 shim (the same
# wire-level checks: the router's ARP/forwarding/DHCP/NAT/failover, the bridge, ip-chat).
# (net-chat's test also drives the v1 shell `chat` command, which v2 does not have yet.)
V2_SHIM_DIR := $(V2_BUILD)/shim
test-v2-router: v2-image
	mkdir -p $(V2_SHIM_DIR) && cp v2/tools/qemu_v1_shim.py $(V2_SHIM_DIR)/qemu-system-i386
	PATH=$(abspath $(V2_SHIM_DIR)):$$PATH FOS_V2=1 python3 tapes/test_router_e2e.py
test-v2-nettapes: v2-image
	mkdir -p $(V2_SHIM_DIR) && cp v2/tools/qemu_v1_shim.py $(V2_SHIM_DIR)/qemu-system-i386
	for t in bridge ip_chat; do PATH=$(abspath $(V2_SHIM_DIR)):$$PATH FOS_V2=1 python3 tapes/test_$${t}_e2e.py || exit 1; done


$(V2_BUILD)/opus-fixed/%.o: $(V2_OPUS_VENDOR)/%.c tapes/opus-codec/config.h tapes/opus-codec/custom_support.h
	mkdir -p $(dir $@)
	gcc $(V2_OPUS_CFLAGS) -c $< -o $@

$(V2_OPUS_LIB): $(V2_OPUS_OBJS)
	ar rcs $@ $(V2_OPUS_OBJS)

v2-opus-lib: $(V2_OPUS_LIB)

# The benchmark tape (not part of V2_TAPES: it links the C archive).
v2-tape-opus-bench: $(V2_OPUS_LIB)
	cd v2/tapes/opus-bench && RUSTFLAGS="$(V2_TAPE_LINK) -L $(CURDIR)/$(V2_BUILD)/opus-fixed -l static=opus_fixed" cargo +nightly-2026-09-13 build --release
