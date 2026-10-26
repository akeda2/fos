# Fosfor OS build. The 64-bit tree: a UEFI loader (boot/uefi), one kernel (kernel/), the tapes (tapes/)
# and the host tools (tools/). See docs/shared-codebase.md for where this is going (a BIOS boot path next
# to the UEFI one) and docs/design.md for the design so far.
#
# Every cargo invocation shares one target directory, so -Z build-std's core/compiler_builtins and the
# shared path crates (abi, tape-rt, tape-net ...) compile once. None of the crates is a workspace member;
# cargo's own locking keeps this safe under `make -j`. $(CURDIR), not a relative path, because recipes cd
# into each crate before calling cargo.
BUILD := build
export CARGO_TARGET_DIR := $(CURDIR)/$(BUILD)/cargo-target

.DEFAULT_GOAL := image
.PHONY: all help clean bios print-image test-usb-replug test-usb-kbd-replug test-usb-pointer test-xhci-abort test-idle-cpu test-net-chat test-telnet test-release-loops test-nettest-lab test-nettest-remote test-audio-bench test-bootcode-uefi test-console-utf8 test-keymap test-console-colors test-hostnames test-config-fat test-fterm test-fterm-net test-fterm-ui test-serial-shell test-midi test-services

# `all` builds both machines' images: the x86 image and the Raspberry Pi 4 SD card image. What is missing is said up front.
ifneq ($(filter all image-pi4 image-aarch64,$(MAKECMDGOALS)),)
ifneq ($(shell python3 tools/check_build_tools.py >&2; echo $$?),0)
$(error build prerequisites missing, see the list above)
endif
endif

all: image tapes image-pi4

help:
	@echo "make all          both images: the x86 one and the Raspberry Pi 4 SD card image (checks the tools first)"
	@echo "make image        build $(BUILD)/fosfor/fosfor.img (UEFI loader + kernel + every tape)"
	@echo "make run          build it and boot it in QEMU + OVMF with the serial console on the terminal"
	@echo "make test         host unit tests (abi, crates, tape libraries), then the boot test (loader, kernel, framebuffer)"
	@echo "make test-NAME    one end-to-end test (see the .PHONY list below), e.g. test-nic, test-hda, test-recorder"
	@echo "make test-all     every test-* target in turn, with a summary (TEST_JOBS=N: N at a time)"
	@echo "make loader|kernel|tapes   build one part"
	@echo "make clean        remove $(BUILD)/"

# A stick that is unplugged and plugged back in (QEMU monitor device_del / device_add) gets its disk number back.
test-usb-replug: image
	python3 tools/test_usb_replug.py

# A USB pointer device (tablet, mouse) nothing claims has its HID report descriptor dumped and parsed.
test-usb-pointer: image
	python3 tools/test_usb_pointer.py

# A keyboard that is unplugged and plugged back in keeps working, five times over (xHCI and EHCI).
test-usb-kbd-replug: image
	python3 tools/test_usb_kbd_replug.py

# A timed-out xHCI command is aborted and the command ring carries on (usb abort-test).
test-xhci-abort: image
	python3 tools/test_xhci_abort.py

# An idle machine sleeps in hlt instead of spinning (QEMU's host CPU use).
test-idle-cpu: image
	python3 tools/test_idle_cpu.py

# The net-chat tape: chat text and discovery over raw Ethernet, driven from a term net session.
test-net-chat: image
	python3 tools/test_net_chat.py

# The fterm serial terminal tape against a host-side peer on the second QEMU UART (COM2).
test-fterm: image
	python3 tools/test_fterm.py

# The fterm settings panel, driven by keystrokes.
test-fterm-ui: image
	python3 tools/test_fterm_ui.py

# fterm over Telnet (user-mode network) and term net (a host-side peer on the multicast segment).
test-fterm-net: image
	python3 tools/test_fterm_net.py

# The shell on a serial port (`serial-shell` in settings), driven from a host-side peer on COM2.
test-serial-shell: image
	python3 tools/test_serial_shell.py

# The `midi` shell command without a USB MIDI device (QEMU has none; the real-hardware loopback is manual).
test-midi: image
	python3 tools/test_midi.py

# The `midi` shell command without a USB MIDI device (QEMU has none; real-hardware loopback is manual).
test-midi: image
	python3 tools/test_midi.py

# The telnet tape against a host-side Telnet server (QEMU user-mode network).
test-telnet: image
	python3 tools/test_telnet.py

# Blocking commands (nic sniff, tape load net) end on the term net client's Escape or disconnect, on any NIC.
test-release-loops: image
	python3 tools/test_release_loops.py

# audio-net bench: master and benchserve guests sweep packet times both ways and recommend ptime/target.
test-audio-bench: image
	python3 tools/test_audio_bench.py

# Two guests: nettest serve=1 and remote=1 find each other, sync clocks and run a test both ways.
test-nettest-remote: image
	python3 tools/test_nettest_remote.py

# nettest's generator and sink through a four-NIC loop: clean, rate limit, loss and delay, selftest, retarget.
test-nettest-lab: image
	python3 tools/test_nettest_lab.py
test-console-colors: image
	python3 tools/test_console_colors.py
test-console-utf8: image
	python3 tools/test_console_utf8.py
# `keymap sv`: å ä ö Å and AltGr symbols from a PS/2 keyboard.
test-keymap: image
	python3 tools/test_keymap.py

# Machines announce their names unprompted; audio-net uses the kernel's name and peer table.
test-hostnames: image
	python3 tools/test_hostnames.py

# hostname/start/settings/debug found on a FAT32/exFAT disk's root.
test-config-fat: image
	python3 tools/test_config_fat.py

# `bootcode update` replaces the UEFI loader on the ESP (verified with fsck.vfat and 7-Zip), and the new loader boots.
test-bootcode-uefi: image
	python3 tools/test_bootcode_uefi.py

# `make -s print-image`: the path `make image` writes (tools/build_image.py's DEFAULT_IMAGE must equal it; test-stick-guids checks).
print-image:
	@echo $(abspath $(IMAGE))

clean:
	rm -rf $(BUILD)

OUT := $(BUILD)/fosfor
# The BIOS boot chain's binaries; defined here, before the `image` rule, because prerequisites are expanded when read.
BIOS_OUT := $(BUILD)/bios
BIOS_STAGES := $(BIOS_OUT)/stage1.bin $(BIOS_OUT)/stage2.bin
LOADER_EFI := $(CARGO_TARGET_DIR)/x86_64-unknown-uefi/release/fosfor-loader.efi
KERNEL_ELF := $(CARGO_TARGET_DIR)/x86_64-fos-kernel/release/fosfor-kernel
IMAGE := $(OUT)/fosfor.img
# tapes: one crate each under tapes/ (shared libraries live in crates/), each built to build/cargo-target/x86_64-fos-tape/release/NAME.
TAPES := hello calc memtest nettest bridge router ip-chat telnet net-chat ed record recorder play audio-net ptp fterm fpcheck gfxcheck ltas gonio clock kaffe organ strings tape-echo epiano phono
# Hard-float tapes (target x86_64-fos-tape-fp, SSE2/AVX2; the kernel enables SSE before they start) build into their own directory.
FP_TAPES := fpcheck ltas gonio clock kaffe audio-net organ strings tape-echo epiano phono
tape_dir = x86_64-fos-tape$(if $(filter $(1),$(FP_TAPES)),-fp)
TAPE_BINS := $(foreach tape,$(TAPES),$(CARGO_TARGET_DIR)/$(call tape_dir,$(tape))/release/$(tape))
# The image and deployments use the stamped copies (tools/stamp_tape.py: the `.fos_build` section `tapesync` compares).
TAPE_STAMPED := $(OUT)/tapes
TAPE_FILES := $(foreach tape,$(TAPES),--file $(tape)=$(TAPE_STAMPED)/$(tape)) --file debug=tools/default-debug.txt

# ---- Opus (stage C of audio-net): libopus built freestanding for the 64-bit tape target ----
# The vendored tree (crates/opus-codec/vendor/opus, v1.6.1, floating-point, trimmed to what the build needs) is compiled
# with the C compiler directly, position-independent and without the red zone, with SSE2 maths
# (a hard-float tape: x86_64-fos-tape-fp), and archived; a tape links the archive through RUSTFLAGS (which replaces the shared
# .cargo/config.toml rustflags, so those are repeated). See crates/opus-codec.
OPUS_VENDOR := crates/opus-codec/vendor/opus
OPUS_SOURCES := $(filter-out %/mini_kfft.c,$(wildcard $(OPUS_VENDOR)/celt/*.c)) $(wildcard $(OPUS_VENDOR)/silk/*.c) $(wildcard $(OPUS_VENDOR)/silk/float/*.c) $(wildcard $(OPUS_VENDOR)/src/*.c)
OPUS_OBJS := $(patsubst $(OPUS_VENDOR)/%.c,$(OUT)/opus-float/%.o,$(OPUS_SOURCES))
OPUS_LIB := $(OUT)/opus-float/libopus_float.a
OPUS_CFLAGS := -O2 -ffreestanding -fno-stack-protector -fPIE -fno-asynchronous-unwind-tables -msse2 -mfpmath=sse -fno-math-errno -mno-red-zone -ffunction-sections -fdata-sections -U_FORTIFY_SOURCE -DHAVE_CONFIG_H -Icrates/opus-codec -I$(OPUS_VENDOR)/include -I$(OPUS_VENDOR)/celt -I$(OPUS_VENDOR)/silk -I$(OPUS_VENDOR)/silk/float
# The same tree for aarch64 (hard float, clang since no aarch64 gcc is needed): build/fosfor-aarch64/opus-float.
OPUS_A64_OBJS := $(patsubst $(OPUS_VENDOR)/%.c,build/fosfor-aarch64/opus-float/%.o,$(OPUS_SOURCES))
OPUS_A64_LIB := build/fosfor-aarch64/opus-float/libopus_float.a
OPUS_A64_CFLAGS := --target=aarch64-unknown-none-elf -isystem crates/opus-codec/libc-a64 -O2 -ffreestanding -fno-stack-protector -fPIE -fno-asynchronous-unwind-tables -fno-math-errno -ffunction-sections -fdata-sections -U_FORTIFY_SOURCE -DHAVE_CONFIG_H -Icrates/opus-codec -I$(OPUS_VENDOR)/include -I$(OPUS_VENDOR)/celt -I$(OPUS_VENDOR)/silk -I$(OPUS_VENDOR)/silk/float
TAPE_LINK := -C link-arg=-T../tape.ld -C relocation-model=pic -C link-arg=-pie -C link-arg=--no-dynamic-linker -C link-arg=-znorelro


.PHONY: opus-lib-aarch64 image-pi4 tapes-aarch64 loader-aarch64 kernel-aarch64 image-aarch64 test-aarch64-boot test-aarch64-faults test-aarch64-kup test-kup-serial opus-lib tape-opus-bench loader kernel image run test test-faults test-keyboard test-usb-boot test-config test-tapes test-nettest test-debug-mode test-msi test-router test-nettapes test-hub test-disks test-stick-guids test-format test-ed test-diag test-fp test-gfx test-ltas test-gonio test-clock test-kaffe test-organ test-strings test-tape-echo test-epiano test-phono test-split test-usb-audio test-hda test-audio-net test-services test-audio-services test-loopback test-ptp test-ptp-l2 test-ptp-e1000e test-peers release test-fosadm test-ptp-both test-ptp-ntp test-audio-ptp test-audio-ptp-raw test-audio-ptp-avtp test-audio-serve test-record test-recorder test-recorder-fat test-fat-shell test-format-exfat test-recorder-format test-grow test-install test-lineedit test-scripts test-nic test-netfs test-tapesync test-gpio test-rtl8139

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

# Copies of the tapes with the build stamp added: build/fosfor/tapes/NAME is what goes into the image and onto machines.
.PHONY: stamp-tapes
stamp-tapes: tapes
	mkdir -p $(TAPE_STAMPED)
	for tape in $(TAPES); do \
		dir=x86_64-fos-tape; case " $(FP_TAPES) " in *" $$tape "*) dir=x86_64-fos-tape-fp;; esac; \
		python3 tools/stamp_tape.py $(CARGO_TARGET_DIR)/$$dir/release/$$tape $(TAPE_STAMPED)/$$tape || exit 1; done

# A bootable disk image: GPT + a FAT32 EFI System Partition with \EFI\BOOT\BOOTX64.EFI
# and a payload partition with the kernel in slot 0 (tools/build_image.py;
# standard library only).
image: loader kernel tapes stamp-tapes $(BIOS_STAGES)
	mkdir -p $(OUT)
	python3 tools/build_image.py --loader $(LOADER_EFI) --kernel $(KERNEL_ELF) --output $(IMAGE) \
		--bios-stage1 $(BIOS_OUT)/stage1.bin --bios-stage2 $(BIOS_OUT)/stage2.bin $(TAPE_FILES)

# aarch64 (Raspberry Pi 4, QEMU virt; docs/aarch64-plan.md): the loader and kernel for the other CPU, in their own image.
A64_LOADER := $(CARGO_TARGET_DIR)/aarch64-unknown-uefi/release/fosfor-loader.efi
A64_KERNEL := $(CARGO_TARGET_DIR)/aarch64-unknown-none-softfloat/release/fosfor-kernel
A64_IMAGE := $(BUILD)/fosfor-aarch64/fosfor.img

loader-aarch64:
	cd boot/uefi && cargo +nightly-2026-09-13 build --release --target aarch64-unknown-uefi -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

kernel-aarch64:
	cd kernel && cargo +nightly-2026-09-13 build --release --target aarch64-unknown-none-softfloat -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

# Tapes for aarch64 (M2 of the plan): the portable ones, built for the built-in hard-float aarch64-unknown-none target.
A64_TAPES := hello gfxcheck calc ed netcheck clock gonio ptp play record recorder ltas kaffe organ strings tape-echo epiano phono fterm telnet net-chat ip-chat bridge nettest router audio-net opus-bench i2cscan
A64_TAPE_DIR := $(CARGO_TARGET_DIR)/aarch64-unknown-none/release
A64_TAPE_STAMPED := build/fosfor-aarch64/tapes

tapes-aarch64: $(OPUS_A64_LIB)
	for tape in $(A64_TAPES); do (cd tapes/$$tape && cargo +nightly-2026-09-13 build --release --target aarch64-unknown-none -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem) || exit 1; done
	mkdir -p $(A64_TAPE_STAMPED)
	for tape in $(A64_TAPES); do python3 tools/stamp_tape.py $(A64_TAPE_DIR)/$$tape $(A64_TAPE_STAMPED)/$$tape || exit 1; done

image-aarch64: loader-aarch64 kernel-aarch64 tapes-aarch64
	mkdir -p $(dir $(A64_IMAGE))
	python3 tools/build_image.py --arch aarch64 --loader $(A64_LOADER) --kernel $(A64_KERNEL) $(foreach tape,$(A64_TAPES),--file $(tape)=$(A64_TAPE_STAMPED)/$(tape)) --output $(A64_IMAGE)

# The SD card image for a Raspberry Pi 4: the same image plus the pftf edk2 firmware files in the ESP (fetched once, pinned by hash).
# Flash it with `sudo python3 tools/write_stick.py /dev/sdX --image build/fosfor-aarch64/fosfor-pi4.img` (a person runs that).
image-pi4: image-aarch64
	python3 tools/fetch_rpi4_firmware.py build/rpi4-firmware
	python3 tools/build_image.py --arch aarch64 --loader $(A64_LOADER) --kernel $(A64_KERNEL) $(foreach tape,$(A64_TAPES),--file $(tape)=$(A64_TAPE_STAMPED)/$(tape)) --esp-dir build/rpi4-firmware --output build/fosfor-aarch64/fosfor-pi4.img

test-aarch64-boot: image-aarch64
	python3 tools/test_aarch64_boot.py

# Serial KUP: a kernel pushed over the console UART is tried from the other slot and confirmed.
test-aarch64-kup: image-aarch64
	python3 tools/test_kup_serial.py aarch64

test-kup-serial: image
	python3 tools/test_kup_serial.py x86

# Deliberate-fault kernels (one Cargo feature each) must be reported by the aarch64 exception vectors.
test-aarch64-faults: loader-aarch64
	python3 tools/test_aarch64_faults.py

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
	cargo +nightly-2026-09-13 test --manifest-path crates/fat/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/hid/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/audio-rec/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/tape-net/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/audio-wire/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/audio-sample/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/audio-bench/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/tape-sync/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/shell/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/gfx/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/dsp/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/ltas/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/noise/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/fx/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/phono/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/organ/Cargo.toml
	RUST_MIN_STACK=33554432 cargo +nightly-2026-09-13 test --manifest-path crates/strings/Cargo.toml
	RUST_MIN_STACK=33554432 cargo +nightly-2026-09-13 test --manifest-path crates/epiano/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/tape-echo/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/instrument-rt/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/audio-screen/Cargo.toml
	cargo +nightly-2026-09-13 test --manifest-path crates/tape-rt/Cargo.toml
	for tape in router nettest ed ip-chat telnet ptp fterm organ strings tape-echo epiano phono; do RUST_MIN_STACK=33554432 cargo +nightly-2026-09-13 test --lib --manifest-path tapes/$$tape/Cargo.toml || exit 1; done
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

# Hard-float tapes: SSE and AVX2/FMA switched on before a tape starts (fp_level), QEMU default CPU and -cpu max.
test-fp: image
	python3 tools/test_fp.py

# Tape graphics (gfx_*): colours, stride, clipping and the console coming back, read from a QEMU screendump.
test-gfx: image
	python3 tools/test_gfx.py

# The ltas tape (graphic spectrum analyser) against QEMU's HD Audio codec: screen, analysis loop, keys, exit.
test-ltas: image
	python3 tools/test_ltas.py

# The gonio tape (goniometer, correlation and level meters) with its built-in stereo test tone: mono and opposite polarity.
test-gonio: image
	python3 tools/test_gonio.py

# The clock tape against a fake NTP server: the decoded digits and dots, the machine's clock set, 12 h, theme, ntp=off.
test-clock: image
	python3 tools/test_clock.py

# The kaffe tape: buttons and clock drawn, Enter stamps the time and saves it, c clears it.
test-kaffe: image
	python3 tools/test_kaffe.py

test-organ: image
	python3 tools/test_organ.py

test-strings: image
	python3 tools/test_strings.py

test-epiano: image
	python3 tools/test_epiano.py

test-tape-echo: image
	python3 tools/test_tape_echo.py

test-phono: image
	python3 tools/test_phono.py

# run A | B: two graphic tapes share one screen, side by side or stacked (split=v).
test-split: image
	python3 tools/test_split.py


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

# ptp: a master and a slave guest; the slave must lock on, measure a small offset and lose no Sync.
test-ptp: image
	python3 tools/test_ptp.py

# Background tapes: a ptp master as a service (`run ... &1`) while the shell and a foreground tape are used.
test-services: image
	python3 tools/test_services.py

test-audio-services: image
	python3 tools/test_audio_services.py

test-loopback: image
	python3 tools/test_loopback.py

# The same over Ethernet (ethertype 0x88F7), no IP.
test-ptp-l2: image
	python3 tools/test_ptp.py l2

# The same two runs on QEMU's e1000e (82574L): its 4096-bit multicast hash and the e1000e receive path. QEMU emulates
# no IEEE 1588 stamping, so this checks the software-stamp fallback; the stamps themselves need a real card.
test-ptp-e1000e: image
	FOS_NIC_MODEL=e1000e python3 tools/test_ptp.py
	FOS_NIC_MODEL=e1000e python3 tools/test_ptp.py l2

# peers.py --forget also clears the heard host names.
test-peers:
	python3 tools/test_peers.py

# A folder for `fosadm --from`: both CPUs' kernels and tapes plus a manifest (tools/make_release.py).
release: image tapes-aarch64 kernel-aarch64
	python3 tools/make_release.py

test-fosadm:
	python3 tools/test_fosadm.py

# One master serving a UDP slave and an Ethernet slave at once (`ptp master both`).
test-ptp-both: image
	python3 tools/test_ptp_both.py

# A master with NTP time (a fake server on the segment) and a slave that sets its clock from it.
test-ptp-ntp: image
	python3 tools/test_ptp_ntp.py

# audio-net on a PTP clock: master, RTP sender and receiver; stamps, SDP and the receiver's delay.
test-audio-ptp: image
	python3 tools/test_audio_ptp.py

# The same over layer 2: PTP on Ethernet, raw and AVTP streams.
test-audio-ptp-raw: image
	python3 tools/test_audio_ptp.py raw

test-audio-ptp-avtp: image
	python3 tools/test_audio_ptp.py avtp

# audio-net serve: Ogg Opus over HTTP from a guest, fetched from the host and decoded with ffmpeg.
test-audio-serve: image
	python3 tools/test_audio_serve.py

# NETFS: foscopy.py and copy HOST/NAME between machines.
test-netfs: image
	python3 tools/test_netfs.py

# tapesync: pulling newer tapes between machines, authenticated by the synckey file.
test-tapesync: image
	python3 tools/test_tapesync.py

# gpio: the shell command and GPIO over the network, on a RAM-backed pin bank (gpio-sim).
test-gpio: image
	python3 tools/test_gpio.py

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


$(OUT)/opus-float/%.o: $(OPUS_VENDOR)/%.c crates/opus-codec/config.h crates/opus-codec/custom_support.h
	mkdir -p $(dir $@)
	gcc $(OPUS_CFLAGS) -c $< -o $@

$(OPUS_LIB): $(OPUS_OBJS)
	ar rcs $@ $(OPUS_OBJS)

opus-lib: $(OPUS_LIB)

build/fosfor-aarch64/opus-float/%.o: $(OPUS_VENDOR)/%.c crates/opus-codec/config.h crates/opus-codec/custom_support.h
	mkdir -p $(dir $@)
	clang $(OPUS_A64_CFLAGS) -c $< -o $@

$(OPUS_A64_LIB): $(OPUS_A64_OBJS)
	ar rcs $@ $(OPUS_A64_OBJS)

opus-lib-aarch64: $(OPUS_A64_LIB)

# The benchmark tape (not part of TAPES: it links the C archive).
tape-opus-bench: $(OPUS_LIB)
	cd tapes/opus-bench && RUSTFLAGS="$(TAPE_LINK) -L $(CURDIR)/$(OUT)/opus-float -l static=opus_float" cargo +nightly-2026-09-13 build --release

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
ALL_TESTS := $(filter-out test-all test-bios test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode,$(filter test-%,$(shell sed -n '/[$$]/d;s/^\.PHONY: //p' Makefile))) $(addprefix test-bios-,$(BIOS_TESTS)) test-bios-placement test-bios-video test-bios-sector test-bios-edid test-bios-bootcode
.PHONY: test-all
test-all: image
ifdef TEST_JOBS
	@python3 tools/run_tests.py $(TEST_JOBS) $(ALL_TESTS)
else
	@failed=""; for t in $(ALL_TESTS); do \
		if $(MAKE) --no-print-directory $$t; then echo "PASS $$t"; else echo "FAIL $$t"; failed="$$failed $$t"; fi; done; \
	if [ -n "$$failed" ]; then echo "FAILED:$$failed"; exit 1; fi; echo "all tests passed"
endif

