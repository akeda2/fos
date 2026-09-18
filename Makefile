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
TAPE_BIGSIEVE_BIN := $(BUILD)/tapes/bigsieve.bin
TAPE_LONGMODE_BIN := $(BUILD)/tapes/longmode.bin
TAPE_LONGMODE_RS_BIN := $(BUILD)/tapes/longmode-rs.bin
TAPE_CALC_BIN := $(BUILD)/tapes/calc.bin
TAPE_BEEP_BIN := $(BUILD)/tapes/beep.bin

.PHONY: all help kernel image run test netrom tapes tape-primes tape-hello-rs tape-vga-colors tape-bigsieve tape-longmode tape-longmode-rs tape-calc tape-beep clean

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
	@echo "  make tapes      - build every test tape (same as tape-primes + tape-hello-rs + tape-vga-colors + tape-bigsieve + tape-longmode + tape-longmode-rs + tape-calc + tape-beep)"
	@echo "  make tape-primes     - build build/tapes/primes.bin (hand-written PIC assembly tape)"
	@echo "  make tape-hello-rs   - build build/tapes/hello-rs.bin (Rust tape, one-shot)"
	@echo "  make tape-vga-colors - build build/tapes/vga-colors.bin (Rust tape, loops until 'give up')"
	@echo "  make tape-bigsieve   - build build/tapes/bigsieve.bin (Rust tape, uses alloc/free for a 64 MiB buffer)"
	@echo "  make tape-longmode   - build build/tapes/longmode.bin (asm shim + asm body, visits 64-bit long mode and returns)"
	@echo "  make tape-longmode-rs - build build/tapes/longmode-rs.bin (same asm shim, but the 64-bit body is Rust)"
	@echo "  make tape-calc       - build build/tapes/calc.bin (Rust RPN calculator tape; uses read_key/args_ptr/args_len)"
	@echo "  make tape-beep       - build build/tapes/beep.bin (Rust PC speaker tape; beeps on b via read_key)"
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
tapes: tape-primes tape-hello-rs tape-vga-colors tape-bigsieve tape-longmode tape-longmode-rs tape-calc tape-beep

# Builds the "primes" test tape: a standalone, position-independent 32-bit
# binary that is not part of the disk image at all. It is meant to be sent
# to a running fos instance with tools/send_tape.py and executed with
# `tape load net` / `tape run`. See tapes/primes.S's header comment
# and README.md's "Tape" section for details. Entirely additive -- untouched
# by plain `make`/`image`/`run`/`test`; built as part of `make all`/`make
# tapes`.
tape-primes:
	mkdir -p $(BUILD)/tapes
	as --32 -o $(BUILD)/tapes/primes.o tapes/primes.S
	objcopy -O binary $(BUILD)/tapes/primes.o $(TAPE_PRIMES_BIN)

# Builds the "hello-rs" test tape: the same idea as tape-primes above, but
# written in ordinary (non-position-independent) Rust instead of hand-
# written PIC assembly -- possible only because every tape now always
# loads at the same fixed physical address (tapes/hello-rs/linker.ld,
# see kernel/src/pmem.rs's TAPE_BASE/TAPE_LEN doc comment). See
# tapes/hello-rs/src/main.rs and README.md's "Tape" section for
# details. Entirely additive -- untouched by plain `make`/`image`/`run`/
# `test`; built as part of `make all`/`make tapes`.
tape-hello-rs:
	cd tapes/hello-rs && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tapes/hello-rs/target/i686-fos/release/hello-tape $(TAPE_HELLO_RS_BIN)

# Builds the "vga-colors" test tape: same fixed-address Rust setup as
# tape-hello-rs above, but demonstrating the tape ABI's second callback
# (`should_abort`, see kernel/src/keyboard.rs's `tape_should_abort` and
# `shell::tape_run`'s doc comment) -- it loops, drawing cycling colored
# text directly to VGA memory (bypassing console::print_byte/serial/
# term net relay entirely for that animation), until a local operator
# presses one of the "give up" keys. See tapes/vga-colors/src/
# main.rs and AGENTS.md's "vga-colors tape" section for details.
# Entirely additive -- untouched by plain `make`/`image`/`run`/`test`;
# built as part of `make all`/`make tapes`.
tape-vga-colors:
	cd tapes/vga-colors && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tapes/vga-colors/target/i686-fos/release/vga-colors-tape $(TAPE_VGA_COLORS_BIN)

# Builds the "bigsieve" test tape: same fixed-address Rust setup as
# tape-hello-rs above, but demonstrating the tape ABI's third and fourth
# callbacks (`alloc`/`free`, see kernel/src/shell.rs's `tape_alloc`/
# `tape_free` and `shell::tape_run`'s doc comment) -- it requests a fresh
# 64 MiB buffer from the general frame pool at run time (far beyond its
# own fixed 4 MiB tape region) to sieve primes up to a much larger bound
# than tape-hello-rs can, then frees that buffer again before returning.
# See tapes/bigsieve/src/main.rs and AGENTS.md's "Tape ABI grows
# alloc/free" section for details. Entirely additive -- untouched by
# plain `make`/`image`/`run`/`test`; built as part of `make all`/`make
# tapes`.
tape-bigsieve:
	cd tapes/bigsieve && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tapes/bigsieve/target/i686-fos/release/bigsieve-tape $(TAPE_BIGSIEVE_BIN)

# Builds the "longmode" test tape: fos's own hand-written assembly
# `tape64_body` (tapes/longmode.S) linked against the reusable mode-
# switch plumbing shared by every "visits 64-bit long mode" tape
# (tapes/longmode64_shim.S -- CPUID check, page tables, private GDT,
# state save/restore, the actual mode transition in both directions).
# See tapes/longmode64_shim.S's header comment for the reusable
# `tape64_body` contract, tapes/longmode.S's header comment for what
# this particular body does, and AGENTS.md's "A tape that visits 64-bit
# long mode and comes back" section for the full design rationale.
# Unlike tape-primes, this needs a real `ld` link step (not just
# `as`+`objcopy`): x86-64 ELF relocations are RELA-style, so a
# linker-free objcopy would leave zeroed addresses everywhere either
# file references a label in the other (see the header comment for the
# full explanation). `-Ttext=0x100000` matches kernel/src/pmem.rs's
# TAPE_BASE, the fixed address every tape is loaded at; `-e tape_main`
# picks the shim's entry point, not the body's. Entirely additive --
# untouched by plain `make`/`image`/`run`/`test`; built as part of
# `make all`/`make tapes`. Note that actually *running* this tape's real
# 64-bit excursion needs `qemu-system-x86_64`, not the `qemu-system-i386`
# every other target in this repo uses -- see tapes/test_longmode_e2e.py.
tape-longmode:
	mkdir -p $(BUILD)/tapes
	as --64 -o $(BUILD)/tapes/longmode64_shim.o tapes/longmode64_shim.S
	as --64 -o $(BUILD)/tapes/longmode.o tapes/longmode.S
	ld -Ttext=0x100000 --oformat=elf64-x86-64 -e tape_main -o $(BUILD)/tapes/longmode.elf $(BUILD)/tapes/longmode64_shim.o $(BUILD)/tapes/longmode.o
	objcopy -O binary $(BUILD)/tapes/longmode.elf $(TAPE_LONGMODE_BIN)

# Builds the "longmode-rs" test tape: the exact same reusable
# tapes/longmode64_shim.S plumbing as tape-longmode above, but its
# `tape64_body` is instead compiled from ordinary `#![no_std]` Rust
# (tapes/longmode-rs/src/lib.rs), targeting a new custom target spec,
# targets/x86_64-fos-tape.json -- a genuinely 64-bit target, unlike
# targets/i686-fos.json every other Rust tape in this repo uses, with
# SSE/MMX disabled and soft-float enabled (fos's CR4 setup never sets
# OSFXSR, so any SSE-encoded instruction would fault). Unlike every
# other Rust tape here, this crate is a `staticlib` (see its Cargo.toml),
# not a `bin`: it has no `_start`/linker script/entry point of its own,
# just the one `tape64_body` function, linked directly against the
# shim's object file with the same `ld -Ttext=0x100000 -e tape_main`
# recipe as tape-longmode above -- the linker pulls the one needed
# object out of the archive automatically, resolving tape-longmode64_
# shim.S's `.extern tape64_body` reference. See AGENTS.md's "A tape that
# visits 64-bit long mode and comes back" section for the full design
# rationale and a line-by-line comparison against tape-longmode's
# hand-written body. Entirely additive -- untouched by plain
# `make`/`image`/`run`/`test`; built as part of `make all`/`make tapes`.
tape-longmode-rs:
	mkdir -p $(BUILD)/tapes
	as --64 -o $(BUILD)/tapes/longmode64_shim.o tapes/longmode64_shim.S
	cd tapes/longmode-rs && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	ld -Ttext=0x100000 --oformat=elf64-x86-64 -e tape_main -o $(BUILD)/tapes/longmode-rs.elf $(BUILD)/tapes/longmode64_shim.o tapes/longmode-rs/target/x86_64-fos-tape/release/liblongmode_tape_body.a
	objcopy -O binary $(BUILD)/tapes/longmode-rs.elf $(TAPE_LONGMODE_RS_BIN)

# Builds the "calc" test tape: same fixed-address Rust setup as
# tape-vga-colors above (depends on ../tape-rt for _start/panic handler/
# print helpers), but demonstrating the tape ABI's newest additions --
# `read_key` and `args_ptr`/`args_len` (see kernel/src/shell.rs's
# `combined_tape_read_key` and `shell::tape_run`'s doc comment, and
# AGENTS.md's "Tape ABI grows again: `read_key` and startup arguments"
# section). A reverse-Polish-notation integer calculator: `tape run 3 4 +`
# evaluates once and returns; plain `tape run` starts an interactive REPL
# reading one line at a time from local keyboard or an active `term net`
# session. See tapes/calc/src/main.rs for the full design. Entirely
# additive -- untouched by plain `make`/`image`/`run`/`test`; built as
# part of `make all`/`make tapes`.
tape-calc:
	cd tapes/calc && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tapes/calc/target/i686-fos/release/calc-tape $(TAPE_CALC_BIN)

# Builds the "beep" test tape: a 32-bit Rust tape using the newest
# seven-argument ABI and `read_key` to drive the PC speaker directly.
# Each lowercase or shifted `b` press starts a 440 Hz, approximately
# 100 ms beep; F12/F4/F6/F7/F8/F10 returns to the shell. It supports both
# local keyboard input and bytes arriving through `term net`.
tape-beep:
	cd tapes/beep && cargo +nightly-2026-09-13 build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tapes/beep/target/i686-fos/release/beep-tape $(TAPE_BEEP_BIN)

clean:
	rm -rf $(BUILD) kernel/target tapes/hello-rs/target tapes/vga-colors/target tapes/bigsieve/target tapes/longmode-rs/target tapes/calc/target
