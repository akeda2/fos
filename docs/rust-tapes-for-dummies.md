# Rust tape programming for dummies

A "tape" is the smallest possible fos program: a tiny freestanding binary
that gets loaded into a fixed 4 MiB region of physical memory
(`pmem::TAPE_BASE`, `0x100000`) and run with a single `call` from the
shell's `tape run` command. No filesystem, no process table, no heap, no
paging -- just your code, a stack it borrows from the kernel, and one or
two function pointers the kernel hands you to talk to the outside world.

This guide walks through writing, building, loading, and running a Rust
tape from nothing, assuming no prior fos or embedded-Rust experience. For
the deeper "why", see AGENTS.md's "Tapes: fixed load address, and writing
one in Rust" and "Tape ABI grows a second callback" sections -- this
document only covers the "how".

## 1. What a tape actually is

A tape is a flat binary blob: not an ELF file, not anything with headers
or sections a loader has to parse -- just raw machine code and data bytes,
copied verbatim into memory starting at `0x100000` and then jumped into.
That's why building one means compiling normally, then stripping away
everything but the raw bytes with `objcopy -O binary`.

Every tape must export exactly one C-callable entry point named
`tape_main`, matching one of two signatures:

```rust
// One-shot tapes: print something, maybe compute something, then return.
extern "C" fn tape_main(putc: extern "C" fn(u32));

// Looping tapes: keep running until told to stop.
extern "C" fn tape_main(putc: extern "C" fn(u32), should_abort: extern "C" fn() -> u32);
```

- `putc(byte)` is the *only* way to print. It goes through the kernel's
  own `console::print_byte`, so anything you print also mirrors to serial
  and to a connected `term net` peer, exactly like ordinary shell output.
- `should_abort()` returns non-zero once a local operator has pressed one
  of the "give up" keys (F12/F4/F6/F7/F8/F10) or a remote `term net` peer
  has requested an abort. A one-shot tape doesn't get this callback at
  all -- it's expected to just finish. A looping tape must check it once
  per iteration and return when it's non-zero, or it can never be stopped.

That's the entire ABI. No file I/O, no memory allocation calls, no
threads, no interrupts of your own -- if you need something more, you're
probably describing a kernel feature, not a tape.

## 2. Anatomy of a Rust tape directory

Every existing Rust tape (`tools/tapes/hello-rs/`, `tools/tapes/vga-colors/`)
follows the same five-file layout. Copy whichever one is closer to what
you want to build and start editing -- `hello-rs` for a one-shot tape,
`vga-colors` for a looping one.

```
tools/tapes/your-tape/
├── Cargo.toml           # crate metadata + release profile (panic = "abort", etc.)
├── .cargo/config.toml   # points cargo at the kernel's own i686-fos target
├── linker.ld            # fixes the load address at 0x100000
├── src/entry.S           # 10-line asm stub: zero .bss, then jump to tape_main
└── src/main.rs           # your actual tape, written in ordinary #![no_std] Rust
```

You will only ever need to edit `src/main.rs` and `Cargo.toml`'s crate
`name`. The other three files are boilerplate that's identical (or nearly
identical) across every tape -- copy them from an existing tape rather
than writing them from scratch.

### `Cargo.toml`

```toml
[package]
name = "your-tape"       # <- change this; must be unique among tapes
version = "0.1.0"
edition = "2024"

[profile.dev]
panic = "abort"           # a tape has nowhere to unwind to -- see main.rs's panic_handler

[profile.release]
panic = "abort"
codegen-units = 1
lto = true
opt-level = "z"           # optimize for size: a tape has no filesystem quota, but small is simple
strip = "symbols"
```

### `.cargo/config.toml`

Copy this file byte-for-byte from `hello-rs` or `vga-colors` -- it just
points cargo at the same `targets/i686-fos.json` custom target spec the
kernel itself uses, with the same `-Z build-std` unstable flags, and fixes
the linker script to `linker.ld` in the same directory.

### `linker.ld`

Also copy verbatim. It fixes the entry point's load address at
`0x100000` (`pmem::TAPE_BASE`) -- the one fact that lets ordinary,
non-position-independent Rust work here at all, instead of needing the
hand-written relocation tricks `tools/tapes/primes.S` (the assembly
reference tape) has to use.

### `src/entry.S`

Also copy verbatim. It's a fixed 10-line stub whose only job is to zero
`.bss` (so Rust `static`/`static mut` variables actually start at zero --
a flat binary's `.bss` is not automatically zeroed the way an ELF loader
would do it) and then `jmp` into your Rust `tape_main`. You will not need
to touch this file even for a fairly involved tape.

## 3. Writing `src/main.rs`

Start from this skeleton for a one-shot tape:

```rust
#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("entry.S"));

use core::panic::PanicInfo;

fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(putc: extern "C" fn(u32)) {
    print_bytes(putc, b"Hello from my own tape!\n");
    // ... your logic here ...
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
```

Or this skeleton for a looping tape that a user can abort:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(putc: extern "C" fn(u32), should_abort: extern "C" fn() -> u32) {
    print_bytes(putc, b"Running... press F12 to stop.\n");
    loop {
        // ... do one unit of work / one animation frame ...
        if should_abort() != 0 {
            break;
        }
    }
    print_bytes(putc, b"Stopped.\n");
}
```

Rules to keep in mind while writing the body:

- **No heap.** No `Vec`, `String`, `Box`, or anything from `alloc` -- there
  is no allocator wired up. Use fixed-size stack arrays or, for anything
  larger than a few hundred bytes, a `static mut` array instead (a tape
  runs on the kernel's own stack with no guard page, so keep stack usage
  modest -- see `hello-rs`'s sieve array for the pattern).
- **No `core::fmt`/`println!`.** Printing numbers means writing your own
  decimal-digit loop (see `print_u32` in `tools/tapes/hello-rs/src/main.rs`)
  -- `core::fmt` pulls in more machinery than is worth it for a teaching
  example, and there's no `Display` target to write to anyway besides
  `putc`.
- **Printing floods `term net`.** `putc` mirrors to serial and any
  connected `term net` peer. A one-shot tape printing a few lines is
  fine. A *looping* tape should print only a start/stop message through
  `putc` and do any per-frame output by writing directly to VGA memory
  instead (see the next section) -- otherwise every animation frame spams
  a remote terminal.
- **Only `unsafe` for genuinely low-level things** -- raw pointer writes
  into VGA memory, or reinterpreting a `static mut` as `&mut`. Everything
  else should be ordinary safe Rust.
- **The panic handler must loop forever.** A tape has no OS underneath it
  to return control to on panic (that's what `panic = "abort"` in
  `Cargo.toml` already commits to); `loop {}` is the only sane thing a
  panic handler can do here.

### Writing directly to VGA (optional, for looping/animated tapes)

If your tape needs to redraw the screen every frame, don't do it through
`putc` -- write straight to the VGA text-mode framebuffer at physical
`0xb8000` instead, the same way `tools/tapes/vga-colors/src/main.rs` does:

```rust
const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const VGA_WIDTH: usize = 80;

fn put_cell(row: usize, col: usize, color: u8, byte: u8) {
    unsafe {
        VGA_BUFFER
            .add(row * VGA_WIDTH + col)
            .write_volatile(u16::from(color) << 8 | u16::from(byte));
    }
}
```

Each cell is 2 bytes: the character in the low byte, a color attribute in
the high byte (low nibble = foreground color, high nibble = background;
see `vga-colors`'s `COLORS` array for the 15 usable foreground colors --
color 0 is black text, which would be invisible on the default black
background). This never touches serial or `term net` at all, which is
exactly why it's the right tool for a per-frame animation loop.

## 4. Building your tape

Add a target to the `Makefile`, modeled on the existing `tape-hello-rs`/
`tape-vga-colors` targets:

```make
tape-your-tape:
	cd tools/tapes/your-tape && cargo +nightly-2026-09-13 build --release \
		-Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem \
		-Z json-target-spec
	mkdir -p $(BUILD)/tapes
	objcopy -O binary tools/tapes/your-tape/target/i686-fos/release/your-tape-bin-name \
		$(BUILD)/tapes/your-tape.bin
```

Then add `tape-your-tape` to both the `tapes:` aggregate target and the
`.PHONY` list, and a one-line description to the `help` target, so
`make all`/`make tapes`/`make help` all pick it up automatically. Build
it on its own with:

```sh
make tape-your-tape
```

This runs `cargo build` against the kernel's own custom `i686-fos` target
(the same `-Z build-std` unstable-cargo dance the kernel itself needs,
since there's no prebuilt `std`/`core` for this target), then strips the
resulting ELF down to a raw flat binary with `objcopy -O binary` -- the
same two-step process for every tape, Rust or assembly.

## 5. Loading and running it

Boot fos (`make run`, or a physical machine set up per AGENTS.md's `term
net` section), then from another terminal on the same network:

```sh
python3 tools/send_tape.py build/tapes/your-tape.bin
```

`send_tape.py` prints a numbered list of detected fos hosts if more than
one is on the network (see AGENTS.md's "numbered peer address book"
section) -- pick yours, and it transfers the tape over to the running
machine. Back at the fos shell (or over a `term net` session):

```
tape load net
tape run
```

`tape load net` confirms the transfer arrived; `tape run` executes it. A
one-shot tape prints its output and returns you straight to the shell
prompt. A looping tape keeps running until F12 (or F4/F6/F7/F8/F10)
is pressed locally, or the remote `term net` peer sends its own abort
request -- either way, control returns to the shell without ending the
`term net` session itself.

## 6. Where to look next

- `tools/tapes/hello-rs/src/main.rs` -- the simplest complete one-shot
  Rust tape (a sieve of Eratosthenes over a small range).
- `tools/tapes/vga-colors/src/main.rs` -- the simplest complete looping
  Rust tape (a colored-text VGA animation, abortable both locally and
  remotely).
- `tools/tapes/primes.S` -- the original hand-written, position-
  independent assembly tape, useful for understanding what Rust's fixed
  load address saves you from having to do by hand.
- AGENTS.md's "Tape print callback ABI", "Tapes: fixed load address, and
  writing one in Rust", and "Tape ABI grows a second callback" sections --
  the full historical and technical reasoning behind every constraint
  described above.
- `kernel/src/shell.rs`'s `tape_run` doc comment -- the authoritative,
  in-code description of the ABI both callbacks must satisfy.
