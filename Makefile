BUILD := build
KERNEL_ELF := $(BUILD)/kernel.elf
KERNEL_BIN := $(BUILD)/kernel.bin
STAGE1_BIN := $(BUILD)/stage1.bin
STAGE2_BIN := $(BUILD)/stage2.bin
IMAGE := $(BUILD)/fos.img
NETROM_RAW := $(BUILD)/netrom_raw.bin
NETROM := $(BUILD)/netboot.rom
TAPE_PRIMES_BIN := $(BUILD)/tapes/primes.bin

.PHONY: all kernel image run test netrom tape-primes clean

all: image

kernel:
	cd kernel && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)
	cp kernel/target/i686-fos/release/fos-kernel $(KERNEL_ELF)
	objcopy -O binary $(KERNEL_ELF) $(KERNEL_BIN)

image: kernel
	python3 tools/write_layout.py --kernel $(KERNEL_BIN) --output $(BUILD)/layout.inc
	as --32 -I$(BUILD) -o $(BUILD)/stage1.o boot/stage1.S
	objcopy -O binary $(BUILD)/stage1.o $(STAGE1_BIN)
	as --32 -I$(BUILD) -o $(BUILD)/stage2.o boot/stage2.S
	objcopy -O binary $(BUILD)/stage2.o $(STAGE2_BIN)
	python3 tools/build_image.py --stage1 $(STAGE1_BIN) --stage2 $(STAGE2_BIN) --kernel $(KERNEL_BIN) --output $(IMAGE)

run: image
	qemu-system-i386 -drive format=raw,file=$(IMAGE) -serial stdio -monitor none -no-reboot -no-shutdown

test: image
	python3 tools/qemu_smoke.py --image $(IMAGE)

# Builds an alternative, optional boot artifact: a PCI expansion ("option")
# ROM that boots fos directly from a network card's own boot ROM instead of
# from disk/USB. Entirely additive -- `image`/`run`/`test` above are
# untouched, and this never runs as part of `make` or `make all`. See
# README.md's "Booting from a NIC ROM" section.
netrom: kernel
	python3 tools/write_netrom_layout.py --kernel $(KERNEL_BIN) --output $(BUILD)/netrom_layout.inc
	as --32 -I$(BUILD) -Iboot -o $(BUILD)/netrom.o boot/netrom.S
	objcopy -O binary $(BUILD)/netrom.o $(NETROM_RAW)
	python3 tools/build_netrom.py --rom $(NETROM_RAW) --output $(NETROM)

# Builds the "primes" test tape: a standalone, position-independent 32-bit
# binary that is not part of the disk image at all. It is meant to be sent
# to a running fos instance with tools/send_tape.py and executed with
# `tape load net` / `tape run`. See tools/tapes/primes.S's header comment
# and README.md's "Tape" section for details. Entirely additive -- untouched
# by `make`/`make all`/`image`/`run`/`test`.
tape-primes:
	mkdir -p $(BUILD)/tapes
	as --32 -o $(BUILD)/tapes/primes.o tools/tapes/primes.S
	objcopy -O binary $(BUILD)/tapes/primes.o $(TAPE_PRIMES_BIN)

clean:
	rm -rf $(BUILD) kernel/target
