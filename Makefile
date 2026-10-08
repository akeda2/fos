# Fosfor OS build. The 64-bit tree: a UEFI loader (boot/uefi), one kernel (kernel/), the tapes (tapes/)
# and the host tools (tools/). See docs/shared-codebase.md for where this is going (a BIOS boot path next
# to the UEFI one) and docs/v2-design.md for the design so far.
#
# Every cargo invocation shares one target directory, so -Z build-std's core/compiler_builtins and the
# shared path crates (abi, tape-rt, tape-net ...) compile once. None of the crates is a workspace member;
# cargo's own locking keeps this safe under `make -j`. $(CURDIR), not a relative path, because recipes cd
# into each crate before calling cargo.
BUILD := build
export CARGO_TARGET_DIR := $(CURDIR)/$(BUILD)/cargo-target

.DEFAULT_GOAL := image
.PHONY: all help clean bios print-image test-usb-replug test-usb-kbd-replug test-xhci-abort

all: image tapes

help:
	@echo "make image        build $(BUILD)/v2/v2.img (UEFI loader + kernel + every tape)"
	@echo "make run          build it and boot it in QEMU + OVMF with the serial console on the terminal"
	@echo "make test         unit tests of abi/fat/audio-rec, then the boot test (loader, kernel, framebuffer)"
	@echo "make test-NAME    one end-to-end test (see the .PHONY list below), e.g. test-nic, test-hda, test-recorder"
	@echo "make test-all     every test-* target in turn, with a summary"
	@echo "make loader|kernel|tapes   build one part"
	@echo "make clean        remove $(BUILD)/"

# A stick that is unplugged and plugged back in (QEMU monitor device_del / device_add) gets its disk number back.
test-usb-replug: image
	python3 tools/test_usb_replug.py

# A keyboard that is unplugged and plugged back in keeps working, five times over (xHCI and EHCI).
test-usb-kbd-replug: image
	python3 tools/test_usb_kbd_replug.py

# A timed-out xHCI command is aborted and the command ring carries on (usb abort-test).
test-xhci-abort: image
	python3 tools/test_xhci_abort.py

# `make -s print-image`: the path `make image` writes (tools/build_image.py's DEFAULT_IMAGE must equal it; test-stick-guids checks).
print-image:
	@echo $(abspath $(IMAGE))

clean:
	rm -rf $(BUILD)

OUT := $(BUILD)/v2
# The BIOS boot chain's binaries; defined here, before the `image` rule, because prerequisites are expanded when read.
BIOS_OUT := $(BUILD)/bios
BIOS_STAGES := $(BIOS_OUT)/stage1.bin $(BIOS_OUT)/stage2.bin
LOADER_EFI := $(CARGO_TARGET_DIR)/x86_64-unknown-uefi/release/fos-loader.efi
KERNEL_ELF := $(CARGO_TARGET_DIR)/x86_64-fos-kernel/release/fos-v2-kernel
IMAGE := $(OUT)/v2.img
# v2 tapes: crates under tapes/, each built to build/cargo-target/x86_64-fos-tape/release/NAME.
TAPES := hello calc memtest nettest bridge router ip-chat telnet net-chat ed record recorder play audio-net
TAPE_BINS := $(foreach tape,$(TAPES),$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape))
TAPE_FILES := $(foreach tape,$(TAPES),--file $(tape)=$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape)) --file debug=tools/default-debug.txt

# ---- Opus (stage C of audio-net): libopus built freestanding for the 64-bit tape target ----
# The vendored tree (tapes/opus-codec/vendor/opus, v1.5.2, fixed-point, trimmed to what the build needs) is compiled
# with the C compiler directly, position-independent and without the red zone or any SSE registers
# (-mgeneral-regs-only), and archived; a tape links the archive through RUSTFLAGS (which replaces the shared
# .cargo/config.toml rustflags, so those are repeated). See tapes/opus-codec.
OPUS_VENDOR := tapes/opus-codec/vendor/opus
OPUS_SOURCES := $(wildcard $(OPUS_VENDOR)/celt/*.c) $(wildcard $(OPUS_VENDOR)/silk/*.c) $(wildcard $(OPUS_VENDOR)/silk/fixed/*.c) $(wildcard $(OPUS_VENDOR)/src/*.c)
OPUS_OBJS := $(patsubst $(OPUS_VENDOR)/%.c,$(OUT)/opus-fixed/%.o,$(OPUS_SOURCES))
OPUS_LIB := $(OUT)/opus-fixed/libopus_fixed.a
OPUS_CFLAGS := -O2 -ffreestanding -fno-builtin -fno-stack-protector -fPIE -fno-asynchronous-unwind-tables -mgeneral-regs-only -mno-red-zone -ffunction-sections -fdata-sections -U_FORTIFY_SOURCE -DHAVE_CONFIG_H -Itapes/opus-codec -I$(OPUS_VENDOR)/include -I$(OPUS_VENDOR)/celt -I$(OPUS_VENDOR)/silk -I$(OPUS_VENDOR)/silk/fixed
TAPE_LINK := -C link-arg=-T../tape.ld -C relocation-model=pic -C link-arg=-pie -C link-arg=--no-dynamic-linker -C link-arg=-znorelro


.PHONY: opus-lib tape-opus-bench loader kernel image run test test-faults test-keyboard test-usb-boot test-config test-tapes test-nettest test-debug-mode test-msi test-router test-nettapes test-hub test-disks test-stick-guids test-format test-ed test-diag test-usb-audio test-hda test-audio-net test-audio-serve test-record test-recorder test-recorder-fat test-fat-shell test-format-exfat test-recorder-format test-grow test-install test-lineedit test-scripts test-nic test-netfs test-rtl8139

# The loader: a PE/COFF UEFI application (x86_64-unknown-uefi), built with the same
# pinned nightly and -Z build-std as everything else here.
loader:
	cd boot/uefi && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

# The kernel: an ELF64 at a fixed physical address (kernel/linker.ld) for the
# custom x86_64-fos-kernel target.
kernel:
	cd kernel && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec

# Every v2 tape (ELF64 PIE executables for the 64-bit tape ABI).
.PHONY: tapes
tapes: $(OPUS_LIB)
	for tape in $(TAPES); do (cd tapes/$$tape && cargo +nightly-2026-09-13 build --release) || exit 1; done

# A bootable disk image: GPT + a FAT32 EFI System Partition with \EFI\BOOT\BOOTX64.EFI
# and a payload partition with the kernel in slot 0 (tools/build_image.py;
# standard library only).
image: loader kernel tapes $(BIOS_STAGES)
	mkdir -p $(OUT)
	python3 tools/build_image.py --loader $(LOADER_EFI) --kernel $(KERNEL_ELF) --output $(IMAGE) \
		--bios-stage1 $(BIOS_OUT)/stage1.bin --bios-stage2 $(BIOS_OUT)/stage2.bin $(TAPE_FILES)

# Boot the image under QEMU + OVMF with the serial console on the terminal.
run: image
	cp /usr/share/OVMF/OVMF_VARS_4M.fd $(OUT)/vars.fd
	qemu-system-x86_64 -machine q35 -m 512 -serial stdio -no-reboot \
		-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
		-drive if=pflash,format=raw,file=$(OUT)/vars.fd \
		-drive format=raw,file=$(IMAGE)

# Loader + kernel under QEMU + OVMF, including a framebuffer screendump check.
test: image
	cargo +nightly-2026-09-13 test --manifest-path abi/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path fat/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path tapes/audio-rec/Cargo.toml
	python3 tools/test_boot.py

# Deliberate-fault kernels (one Cargo feature each): checks the exception reports,
# including a kernel stack overflow reported through the double-fault stack.
test-faults: loader kernel
	python3 tools/test_faults.py

# Local keyboard: PS/2 keys through the QEMU monitor, and a boot with no keyboard.
test-keyboard: image
	python3 tools/test_keyboard.py

# Boot the image from a USB stick (qemu-xhci, then usb-ehci) and use it as the root disk.
test-usb-boot: image
	python3 tools/test_usb_boot.py

# Keyboards and sticks behind USB hubs (one and two tiers) on qemu-xhci.
test-hub: image
	python3 tools/test_usb_hub.py

# The diskN selector of ls/cat/write/rm, against a second and a blank disk.
test-disks: image
	python3 tools/test_disks.py

# Random per-stick GUIDs (write_stick.py) and boot-disk discovery between look-alike sticks.
test-stick-guids: image
	python3 tools/test_stick_guids.py

# `format`: blank, junk, foreign-GPT and FOSFS disks, the boot-disk refusal and the confirmation step.
test-format: image
	python3 tools/test_format.py

# The ed line editor tape from the local keyboard (cursor keys, other volumes).
test-ed: image
	python3 tools/test_ed.py

# cpuinfo, bootinfo, memmap, irqstat, pci read/write, io, mmio.
test-diag: image
	python3 tools/test_diag.py

# USB audio class 1.0 against QEMU's usb-audio: discovery, a played tone recorded to WAV, capture.
test-usb-audio: image
	python3 tools/test_usb_audio.py

# Intel HD Audio against QEMU's intel-hda + hda-duplex: discovery, widget paths, MSI/INTx/polled, a played tone recorded to WAV,
# capture (silence: QEMU's WAV backend has no input), the unchanged play/record tapes, and USB audio next to it.
test-hda: image
	python3 tools/test_hda.py

# audio-net: a tone sent over raw Ethernet between two guests and recorded from the receiver's usb-audio.
test-audio-net: image
	python3 tools/test_audio_net.py

# audio-net serve: Ogg Opus over HTTP from a guest, fetched from the host and decoded with ffmpeg.
test-audio-serve: image
	python3 tools/test_audio_serve.py

# NETFS: foscopy.py and copy HOST/NAME between machines.
test-netfs: image
	python3 tools/test_netfs.py

# nic sniff / discover / peers / map / use.
test-nic: image
	python3 tools/test_nic.py

# The RTL8139 driver in a guest whose only NIC is QEMU's rtl8139.
test-rtl8139: image
	python3 tools/test_rtl8139.py

# Scripts and bare command names.
test-scripts: image
	python3 tools/test_scripts.py

# The shell's line editor: history, cursor keys, Ctrl-keys, over the serial line.
test-lineedit: image
	python3 tools/test_lineedit.py

# record: streaming a take to disk (a test tone through the real disk path), checked by parsing the image.
test-record: image
	python3 tools/test_record.py

# recorder: meters and pre-roll from the start, manual and silence-triggered takes (test tone with a gate).
test-recorder: image
	python3 tools/test_recorder.py

# recorder onto FAT32 and exFAT volumes (made by mkfs, checked by fsck and read back).
test-recorder-fat: image
	python3 tools/test_recorder_fat.py

# ls/copy/rm on FAT32 and exFAT disks, and the clock.
test-fat-shell: image
	python3 tools/test_fat_shell.py

# format diskN exfat: a blank disk becomes a GPT exFAT volume that fsck.exfat accepts.
test-format-exfat: image
	python3 tools/test_format_exfat.py

# formatting a blank disk as exFAT from the recorder's UI.
test-recorder-format: image
	python3 tools/test_recorder_format.py

# grow diskN: the FOSFS partition and volume fill the disk; sgdisk verifies the GPT.
test-grow: image
	python3 tools/test_grow.py

# `install diskN`: clone the boot disk onto a blank disk, then boot that disk alone.
test-install: image
	python3 tools/test_install.py

# hostname / settings / debug / start files on the boot volume.
test-config: image
	python3 tools/test_config.py

# Tapes: ELF64 PIE programs through the TapeApi table (hello, calc, a tape over the network).
test-tapes: image
	python3 tools/test_tapes.py

# nettest between two e1000 NICs on host-played TCP segments.
test-nettest: image
	python3 tools/test_nettest.py

# Crash records, reboot-on-crash, the loop guard, the watchdog, KUP and the A/B fallback.
test-debug-mode: image
	python3 tools/test_debug_mode.py

# NIC interrupts: MSI on QEMU's e1000e (self-test, counters, hlt instead of spinning),
# the classic e1000 staying polled, and the no-msi setting.
test-msi: image
	python3 tools/test_msi.py

# v1 end-to-end tests re-run against the v2 kernel through a qemu-system-i386 shim (the same
# wire-level checks: the router's ARP/forwarding/DHCP/NAT/failover, the bridge, ip-chat).
# (net-chat's test also drives the v1 shell `chat` command, which v2 does not have yet.)
SHIM_DIR := $(OUT)/shim
test-router: image
	mkdir -p $(SHIM_DIR) && cp tools/qemu_v1_shim.py $(SHIM_DIR)/qemu-system-i386
	PATH=$(abspath $(SHIM_DIR)):$$PATH FOS_V2=1 python3 tapes/test_router_e2e.py
test-nettapes: image
	mkdir -p $(SHIM_DIR) && cp tools/qemu_v1_shim.py $(SHIM_DIR)/qemu-system-i386
	for t in bridge ip_chat; do PATH=$(abspath $(SHIM_DIR)):$$PATH FOS_V2=1 python3 tapes/test_$${t}_e2e.py || exit 1; done


$(OUT)/opus-fixed/%.o: $(OPUS_VENDOR)/%.c tapes/opus-codec/config.h tapes/opus-codec/custom_support.h
	mkdir -p $(dir $@)
	gcc $(OPUS_CFLAGS) -c $< -o $@

$(OPUS_LIB): $(OPUS_OBJS)
	ar rcs $@ $(OPUS_OBJS)

opus-lib: $(OPUS_LIB)

# The benchmark tape (not part of TAPES: it links the C archive).
tape-opus-bench: $(OPUS_LIB)
	cd tapes/opus-bench && RUSTFLAGS="$(TAPE_LINK) -L $(CURDIR)/$(OUT)/opus-fixed -l static=opus_fixed" cargo +nightly-2026-09-13 build --release

# ---- The BIOS boot path (boot/bios) ----
# Stage one (the MBR, 512 bytes) and stage two (real mode -> long mode -> the kernel) are plain GNU as/ld, flat binaries
# linked at their load addresses; `as --64` accepts the .code16/.code32 sections they contain. `make image` writes both
# into the disk image (the MBR's code area and the GPT boot gap), so the one image boots under UEFI and under BIOS.
BIOS_ADDRESS_stage1 := 0x7c00
BIOS_ADDRESS_stage2 := 0x8000
$(BIOS_OUT)/%.bin: boot/bios/%.S
	mkdir -p $(BIOS_OUT)
	as --64 -o $(BIOS_OUT)/$*.o $<
	ld -m elf_x86_64 -Ttext=$(BIOS_ADDRESS_$*) --oformat binary -o $@ $(BIOS_OUT)/$*.o
bios: $(BIOS_STAGES)

# make test-bios-NAME: the existing test tools/test_NAME.py, with QEMU's firmware swapped for SeaBIOS so the image is booted
# by the BIOS path (tools/qemu_bios_shim.py strips the OVMF drives from every qemu command line the test builds).
BIOS_SHIM := $(BIOS_OUT)/shim
BIOS_TESTS := boot config debug-mode tapes scripts lineedit ed msi nic netfs nettest diag format disks stick-guids install
$(BIOS_SHIM)/qemu-system-x86_64: tools/qemu_bios_shim.py
	mkdir -p $(BIOS_SHIM)
	cp $< $@
	chmod +x $@
.PHONY: $(addprefix test-bios-,$(BIOS_TESTS)) test-bios test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode
$(addprefix test-bios-,$(BIOS_TESTS)): test-bios-%: image $(BIOS_SHIM)/qemu-system-x86_64
	PATH=$(abspath $(BIOS_SHIM)):$$PATH FOS_FIRMWARE=bios python3 tools/test_$(subst -,_,$*).py
test-bios: $(addprefix test-bios-,$(BIOS_TESTS)) test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode

# The two ways to skip the video mode switch: a held Shift key, and `debug video skip` (a flag in the slot metadata).
test-bios-video: image
	python3 tools/test_bios_video.py

# `bootcode update`: the BIOS boot code of a running disk is replaced (a patched stage two runs at the next boot); bad inputs are refused.
test-bios-bootcode: image
	python3 tools/test_bios_bootcode.py

# The VBE mode follows the monitor's EDID (QEMU's VGA playing 1280x800, 1920x1080, 1366x768, 1024x768 and no EDID at all).
test-bios-edid: image
	python3 tools/test_bios_edid.py

# `sector diskN LBA`, the metadata write-back check in stage two, and why `debug slots` cannot read a bad metadata sector.
test-bios-sector: image
	python3 tools/test_bios_sector.py

# Stage two picks the kernel's address from the E820 map: boots with 512 MiB, 40 MiB and (expected to stop with a message) 8 MiB.
test-bios-placement: image
	python3 tools/test_bios_placement.py

# Every test-* target in turn (slow); prints a summary and fails if any failed. Run one by name for quick iteration.
ALL_TESTS := $(filter-out test-all test-bios test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode,$(filter test-%,$(shell sed -n 's/^\.PHONY: //p' Makefile))) $(addprefix test-bios-,$(BIOS_TESTS)) test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode
.PHONY: test-all
test-all: image
	@failed=""; for t in $(ALL_TESTS); do \
		if $(MAKE) --no-print-directory $$t; then echo "PASS $$t"; else echo "FAIL $$t"; failed="$$failed $$t"; fi; done; \
	if [ -n "$$failed" ]; then echo "FAILED:$$failed"; exit 1; fi; echo "all tests passed"

