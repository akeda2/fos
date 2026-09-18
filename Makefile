BUILD := build
KERNEL_ELF := $(BUILD)/kernel.elf
KERNEL_BIN := $(BUILD)/kernel.bin
STAGE1_BIN := $(BUILD)/stage1.bin
STAGE2_BIN := $(BUILD)/stage2.bin
IMAGE := $(BUILD)/fos.img
NETROM_RAW := $(BUILD)/netrom_raw.bin
NETROM := $(BUILD)/netboot.rom
TAPE_PRIMES_BIN := $(BUILD)/tapes/primes.bin
TAPE_HELLO_RS_BIN := $(BUILD)/tapes/hello-rs.bin
TAPE_VGA_COLORS_BIN := $(BUILD)/tapes/vga-colors.bin

.PHONY: all help kernel image run test netrom tapes tape-primes tape-hello-rs tape-vga-colors clean

# Plain `make` (no explicit target) keeps meaning `make image` -- the one
# artifact `run`/`test` need for ordinary kernel/boot work -- even though
# `all` (which builds everything else too) is defined first below, purely
# for readability (it reads naturally as an overview of every target this
# file can build). Without this, plain `make` would silently start
# building netrom/every tape as well, surprising anyone used to the
# previous "make == make image" behavior.
.DEFAULT_GOAL := image

# Prints a short description of every target below, since a Makefile with
# this many targets is no longer obviously self-explanatory just from its
# name list (`make -p`'s own listing is far too noisy to point someone
# at instead). Kept in the same order the targets themselves appear in
# this file; update both together.
help:
	@echo "Targets:"
	@echo "  make            - same as 'make image' (the default goal)"
	@echo "  make all        - build everything below: image, netrom, and every tape"
	@echo "  make kernel     - build only the Rust kernel binary (build/kernel.bin)"
	@echo "  make image      - build build/fos.img (stage1 + stage2 + kernel), via 'make kernel'"
	@echo "  make run        - build the image, then boot it interactively in QEMU"
	@echo "  make test       - build the image, then run the serial-driven QEMU smoke test"
	@echo "  make netrom     - build build/netboot.rom, the optional NIC-ROM boot path"
	@echo "  make tapes      - build every test tape (same as tape-primes + tape-hello-rs + tape-vga-colors)"
	@echo "  make tape-primes     - build build/tapes/primes.bin (hand-written PIC assembly tape)"
	@echo "  make tape-hello-rs   - build build/tapes/hello-rs.bin (Rust tape, one-shot)"
	@echo "  make tape-vga-colors - build build/tapes/vga-colors.bin (Rust tape, loops until 'give up')"
	@echo "  make clean      - remove build/ and every cargo target/ directory"
	@echo "See README.md/AGENTS.md for what each artifact actually does."

# Builds every artifact this repository knows how to produce: the disk
# image (also exercised by `run`/`test`), the optional NIC-ROM boot path,
# and every test tape. Not needed for ordinary kernel/boot work -- `image`
# alone (what `run`/`test`/plain `make` already depend on, see
# `.DEFAULT_GOAL` above) is enough for that -- but this is the one target
# that leaves nothing unbuilt.
all: image netrom tapes

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
# untouched, and this never runs as part of plain `make` (which still just
# means `make image`); it is only built as part of `make all`. See
# README.md's "Booting from a NIC ROM" section.
netrom: kernel
	python3 tools/write_netrom_layout.py --kernel $(KERNEL_BIN) --output $(BUILD)/netrom_layout.inc
	as --32 -I$(BUILD) -Iboot -o $(BUILD)/netrom.o boot/netrom.S
	objcopy -O binary $(BUILD)/netrom.o $(NETROM_RAW)
	python3 tools/build_netrom.py --rom $(NETROM_RAW) --output $(NETROM)

# Builds every test tape at once (see the individual tape-* targets below).
tapes: tape-primes tape-hello-rs tape-vga-colors

# Builds the "primes" test tape: a standalone, position-independent 32-bit
# binary that is not part of the disk image at all. It is meant to be sent
# to a running fos instance with tools/send_tape.py and executed with
# `tape load net` / `tape run`. See tools/tapes/primes.S's header comment
# and README.md's "Tape" section for details. Entirely additive -- untouched
# by plain `make`/`image`/`run`/`test`; built as part of `make all`/`make
# tapes`.
tape-primes:
	mkdir -p $(BUILD)/tapes
	as --32 -o $(BUILD)/tapes/primes.o tools/tapes/primes.S
	objcopy -O binary $(BUILD)/tapes/primes.o $(TAPE_PRIMES_BIN)

# Builds the "hello-rs" test tape: the same idea as tape-primes above, but
# written in ordinary (non-position-independent) Rust instead of hand-
# written PIC assembly -- possible only because every tape now always
# loads at the same fixed physical address (tools/tapes/hello-rs/linker.ld,
# see kernel/src/pmem.rs's TAPE_BASE/TAPE_LEN doc comment). See
# tools/tapes/hello-rs/src/main.rs and README.md's "Tape" section for
# details. Entirely additive -- untouched by plain `make`/`image`/`run`/
# `test`; built as part of `make all`/`make tapes`.
tape-hello-rs:
	cd tools/tapes/hello-rs && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tools/tapes/hello-rs/target/i686-fos/release/hello-tape $(TAPE_HELLO_RS_BIN)

# Builds the "vga-colors" test tape: same fixed-address Rust setup as
# tape-hello-rs above, but demonstrating the tape ABI's second callback
# (`should_abort`, see kernel/src/keyboard.rs's `tape_should_abort` and
# `shell::tape_run`'s doc comment) -- it loops, drawing cycling colored
# text directly to VGA memory (bypassing console::print_byte/serial/
# term net relay entirely for that animation), until a local operator
# presses one of the "give up" keys. See tools/tapes/vga-colors/src/
# main.rs and AGENTS.md's "vga-colors tape" section for details.
# Entirely additive -- untouched by plain `make`/`image`/`run`/`test`;
# built as part of `make all`/`make tapes`.
tape-vga-colors:
	cd tools/tapes/vga-colors && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tools/tapes/vga-colors/target/i686-fos/release/vga-colors-tape $(TAPE_VGA_COLORS_BIN)

clean:
	rm -rf $(BUILD) kernel/target tools/tapes/hello-rs/target tools/tapes/vga-colors/target
