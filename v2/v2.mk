# fos v2: the UEFI loader and (later) the 64-bit kernel. Included from the top-level
# Makefile; every target here is prefixed v2- so plain `make` still builds v1 only.
# See docs/v2-design.md.

V2_BUILD := $(BUILD)/v2
V2_LOADER_EFI := $(CARGO_TARGET_DIR)/x86_64-unknown-uefi/release/fos-loader.efi
V2_IMAGE := $(V2_BUILD)/v2.img

.PHONY: v2-loader v2-image v2-run test-v2-loader

# The loader: a PE/COFF UEFI application (x86_64-unknown-uefi), built with the same
# pinned nightly and -Z build-std as everything else here.
v2-loader:
	cd v2/loader && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

# A bootable disk image: GPT + a FAT32 EFI System Partition with \EFI\BOOT\BOOTX64.EFI
# (v2/tools/build_image.py; standard library only).
v2-image: v2-loader
	mkdir -p $(V2_BUILD)
	python3 v2/tools/build_image.py --loader $(V2_LOADER_EFI) --output $(V2_IMAGE)

# Boot the image under QEMU + OVMF with the serial console on the terminal.
v2-run: v2-image
	cp /usr/share/OVMF/OVMF_VARS_4M.fd $(V2_BUILD)/vars.fd
	qemu-system-x86_64 -machine q35 -m 512 -serial stdio -no-reboot \
		-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
		-drive if=pflash,format=raw,file=$(V2_BUILD)/vars.fd \
		-drive format=raw,file=$(V2_IMAGE)

test-v2-loader: v2-image
	python3 v2/tools/test_loader.py
