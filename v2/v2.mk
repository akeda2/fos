# fos v2: the UEFI loader and (later) the 64-bit kernel. Included from the top-level
# Makefile; every target here is prefixed v2- so plain `make` still builds v1 only.
# See docs/v2-design.md.

V2_BUILD := $(BUILD)/v2
V2_LOADER_EFI := $(CARGO_TARGET_DIR)/x86_64-unknown-uefi/release/fos-loader.efi
V2_KERNEL_ELF := $(CARGO_TARGET_DIR)/x86_64-fos-kernel/release/fos-v2-kernel
V2_IMAGE := $(V2_BUILD)/v2.img
# v2 tapes: crates under v2/tapes/, each built to build/cargo-target/x86_64-fos-tape/release/NAME.
V2_TAPES := hello calc memtest nettest bridge router ip-chat telnet net-chat
V2_TAPE_BINS := $(foreach tape,$(V2_TAPES),$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape))
V2_TAPE_FILES := $(foreach tape,$(V2_TAPES),--file $(tape)=$(CARGO_TARGET_DIR)/x86_64-fos-tape/release/$(tape)) --file debug=tools/default-debug.txt

.PHONY: v2-loader v2-kernel v2-image v2-run test-v2 test-v2-faults test-v2-keyboard test-v2-usb-boot test-v2-config test-v2-tapes test-v2-nettest test-v2-debug-mode test-v2-msi test-v2-router test-v2-nettapes

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
v2-tapes:
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
