# Rust tape programming for dummies

A "tape" is the smallest possible fos program: a tiny freestanding ELF32
`ET_EXEC` image whose `PT_LOAD` segments are validated and materialized by
the shell's loader. The current image region is the fixed 4 MiB physical area
(`pmem::TAPE_BASE`, `0x100000`); legacy raw-flat binaries remain accepted
while older tapes are modernized. No
filesystem, no process table, no built-in heap, and no per-tape memory
isolation -- just your
code, a stack it borrows from the
kernel, and a handful of function pointers the kernel hands you to talk
to the outside world (including, if you need it, a way to borrow memory
far bigger than that fixed 4 MiB region -- see section 6 -- and a
way to read input -- see section 7).

The normal image uses a supervisor-only identity map, so those physical
addresses remain directly usable as pointers. `make image-unsafe-memory`
exists to compare that with the older unpaged model; it does not change a
tape's ABI, but it intentionally removes the normal image's page-fault and
stack-guard coverage.

Neither image gives a tape a private address space: the default additionally
protects the kernel's own text/read-only pages and maps PCI memory BARs only
for the owning driver, but a tape still runs in ring 0 and can misuse every
address the kernel maps.

This guide walks through writing, building, loading, and running a Rust
tape from nothing, assuming no prior fos or embedded-Rust experience. For
the deeper "why", see AGENTS.md's "Tapes: fixed load address, and writing
one in Rust", "Tape ABI grows a second callback", "Tape ABI grows
alloc/free", "Tape ABI grows again: `read_key` and startup arguments",
and "Tape ABI grows kernel-managed networking" sections, plus
[`NETWORKING.md`](../NETWORKING.md) for the protocol-layer design -- this
document only covers the "how".

## 1. What a tape actually is

A new Rust tape is an ELF32 file. The loader ignores section headers and
uses only validated `PT_LOAD` program headers, so the file retains its
entry point, segment sizes, and BSS metadata. A raw flat image is still a
deliberate compatibility fallback, but new tapes should keep the linker
output intact rather than stripping it with `objcopy -O binary`.
The call uses the kernel's 32 KiB stack, not a private tape stack; keep
fixed local buffers bounded, especially when a tape is launched from a
nested `term net` command.

Every Rust tape must ultimately export exactly one C-callable entry point
named `tape_main`, matching one of these signatures. A small `_start` stub
clears `.bss` and tail-jumps to it:

```rust
// One-shot tapes: print something, maybe compute something, then return.
extern "C" fn tape_main(putc: extern "C" fn(u32));

// Looping tapes: keep running until told to stop.
extern "C" fn tape_main(putc: extern "C" fn(u32), should_abort: extern "C" fn() -> u32);

// Tapes that need more than their own fixed 4 MiB region of memory.
extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    should_abort: extern "C" fn() -> u32,
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
);

// Tapes that want interactive input, startup arguments, and raw Ethernet.
extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    should_abort: extern "C" fn() -> u32,
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    nic_mac: extern "C" fn(u32) -> u32,
    nic_send: extern "C" fn(u32, u32) -> u32,
    nic_receive: extern "C" fn(u32, u32) -> u32,
    nic_max_frame_len: extern "C" fn() -> u32,
    ticks: extern "C" fn() -> u64,
    nic_count: extern "C" fn() -> u32,
    nic_mac_at: extern "C" fn(u32, u32) -> u32,
    nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    terminal_position: extern "C" fn() -> u32,
    fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    fs_remove: extern "C" fn(u32, u32) -> u32,
);
```

- `putc(byte)` is the normal way to print. It goes through the kernel's own
  `console::print_byte`, so anything you print also mirrors to serial and to
  a connected `term net` peer, exactly like ordinary shell output. A tape
  deliberately drawing a local-only animation can instead write VGA memory
  directly; section 3 explains that exception.
- `should_abort()` returns non-zero once a local operator has pressed one
  of the "give up" keys (F12/F4/F6/F7/F8/F10) or a remote `term net` peer
  has requested an abort. A one-shot tape doesn't get this callback at
  all -- it's expected to just finish. A looping tape must check it once
  per iteration and return when it's non-zero, or it can never be stopped.
- `alloc(bytes)`/`free(address, bytes)` hand out and return memory from a
  much larger pool than your own fixed 4 MiB region -- see section 6 if
  your tape needs a buffer bigger than that.
- `read_key()` is a non-blocking "read one input event" callback -- see
  section 7 for the full encoding and how to build a line reader on top
  of it. Only needed by a tape that wants actual keystrokes, not just
  "give up" detection.
- `args_ptr`/`args_len` are the address and length of whatever text
  followed `tape run` on the command line (`tape run 2 + 2` gives you
  `"2 + 2"`, or a zero-length slice if nothing followed) -- a simple,
  one-shot way to pass your tape a startup parameter, the way a Unix
  program reads `argv`. See section 7.
- `nic_mac(out_ptr)` copies the active kernel-managed NIC's six-byte MAC to
  `out_ptr` and returns non-zero on success. `nic_send(frame_ptr, length)`
  sends one complete raw Ethernet frame, while `nic_receive(out_ptr,
  capacity)` copies one received raw frame and returns its length (or zero
  if none is ready). These callbacks let a tape use Ethernet without
  owning PCI discovery, DMA buffers, or receive-ring recovery. Call
  `nic_mac` once first and report failure before entering a receive loop;
  see `tapes/net-chat/src/main.rs` for a complete shell-compatible example.
- `nic_max_frame_len()` reports the largest frame the kernel can return.
  Use it when sizing a receive buffer instead of duplicating the RTL8139
  constant. `ticks()` returns the kernel's monotonic timer count; protocol
  timers should compare this value rather than programming PIT hardware from
  the tape.
- `nic_count()` reports all initialized interfaces. The `*_at` callbacks use
  zero-based interface indices for concurrent multi-NIC tapes. The final
  `nic_set_promiscuous_at(index, enabled)` callback enables or disables
  receive-all filtering on one interface; a layer-2 bridge needs this so it
  can receive frames addressed to hosts behind another port.
- `wait_for_event()` is the last callback (slot 36, after the audio ones). Call
  it when a polling pass found nothing to do: it sleeps until the next
  interrupt (a received frame, a keypress, or at worst the 10 ms timer tick)
  so an idle tape does not run the CPU at 100%. It may return early; just poll
  again. See `tapes/bridge/src/main.rs` for the pattern. A tape that depends on
  `tape-rt` can instead call `tape_rt::set_wait_hook(wait_for_event)` once and
  then `tape_rt::idle()` from any helper function, with no extra parameter.
- `set_input_mode(mode)` is slot 37, after `wait_for_event`. Every tape starts
  with mode 0: `read_key` erases the previous character on screen itself when
  it returns Backspace. Bit 0 set (`tape_rt::set_raw_keys(true)`) turns that
  off for a tape that moves the cursor and redraws on its own. Cursor keys
  already arrive through `read_key` as ANSI sequences (`ESC [ D` is Left), the
  same from the local keyboard and from `term net`; `tape_rt::KeyDecoder`
  turns the bytes into `Key::Left`, `Key::Home`, `Key::Delete` and so on, and
  output such as `ESC [ 3 D` (cursor left) is interpreted by the kernel's VGA
  console. A lone Escape never reaches a tape: it is the abort event (over
  `term net`, a frame holding only `0x1b`). See `tapes/ed/src/main.rs`'s
  `read_line_cursor` for the whole pattern.
- `micros()` is slot 38, after `set_input_mode`: microseconds since boot as a
  `u64`, read from the PIT's own counter (about 1 us resolution, never goes
  backwards), unlike `ticks()`'s 10 ms steps. Use it for pacing and latency;
  call it once per pass of a polling loop (it costs a few port reads).
  `tape_rt::set_micros_hook(micros)` once, then `tape_rt::micros()` anywhere.
- `terminal_position()` returns the kernel's packed, 1-based ANSI cursor
  position: row in the high 16 bits and column in the low 16 bits. It is
  useful for terminal protocols that must answer a cursor-position request.
  The filesystem callbacks operate on complete FOSFS files rather than raw
  sectors: `fs_file_length(name_ptr, name_len, out_len_ptr)` writes an
  unaligned `u64` length and returns a status; `fs_read(name_ptr, name_len,
  buffer_ptr, capacity, out_len_ptr)` copies one whole file into a
  caller-provided buffer; `fs_write` replaces or creates one whole file; and
  `fs_remove` deletes it. A zero return is success, while nonzero values
  report invalid arguments, missing files, insufficient read buffers,
  unavailable disk firmware, or FOSFS capacity errors. These callbacks are
  useful for optional tape state such as DHCP leases without exposing BIOS,
  GPT, or sector-layout details.

The `tapes/ed/` tape is a complete example of this pattern: it validates a
FOSFS name, queries the length, reads the whole file into an `alloc`-backed
buffer, edits newline-delimited bytes, and replaces the file with `fs_write`.
It deliberately uses a bounded buffer and line-oriented commands rather than
trying to expose raw disk operations or build a full-screen terminal editor.

**Important:** these twenty-two parameters are always in this order, and a
tape may only leave off a *trailing* run of them it doesn't use -- never
one in the middle. Only wanting `alloc`/`free` but not `should_abort`?
You must still declare `should_abort` in your signature (even if you
never call it, e.g. name it `_should_abort`) so `alloc`/`free` land in
the right stack positions. This works at all because of how `cdecl`
(the calling convention every tape is called with) works: the caller
always pushes every argument and always cleans them all up afterward, so
a shorter signature just means your code never reads some of the bytes
already sitting on the stack -- but it can only safely ignore them from
the end, not skip over one in the middle.

That's the entire ABI. Tapes still have no threads or interrupts of their
own, but they may use the high-level FOSFS callbacks when persistent state is
appropriate. Raw sectors, BIOS calls, GPT layout, and partition geometry
remain kernel-only.

## 2. Anatomy of a Rust tape directory

The simplest self-contained Rust tapes (`tapes/hello-rs/`,
`tapes/vga-colors/`, `tapes/bigsieve/`) follow the same five-file layout.
Copy whichever one
is closer to what you want to build and start editing -- `hello-rs` for
a one-shot tape, `vga-colors` for a looping one, `bigsieve` for a
one-shot tape that needs a buffer bigger than 4 MiB.

```
tapes/your-tape/
├── Cargo.toml           # crate metadata + release profile (panic = "abort", etc.)
├── .cargo/config.toml   # points cargo at the kernel's own i686-fos target
├── linker.ld            # fixes the load address at 0x100000
├── src/entry.S           # 10-line asm stub: zero .bss, then jump to tape_main
└── src/main.rs           # your actual tape, written in ordinary #![no_std] Rust
```

Within this directory, you normally edit only `src/main.rs` and
`Cargo.toml`'s crate `name`. The other three files are boilerplate that's
identical (or nearly identical) across every tape -- copy them from an
existing tape rather than writing them from scratch. You still add a
corresponding build target to the repository `Makefile` in section 4.

For this walkthrough, copy from `tapes/hello-rs/` specifically, not
`tapes/vga-colors/`: `vga-colors` no longer keeps its own `src/entry.S`,
panic handler, or `print_bytes` -- it depends on `crates/tape-rt/`, a
small shared crate providing all three, instead (see
`crates/tape-rt/src/lib.rs`'s header comment, and AGENTS.md's
"`crates/tape-rt/`: a shared runtime, so far only used by `vga-colors`"
section, for the full story). `hello-rs` and `bigsieve` were left fully
self-contained on purpose, specifically so a walkthrough like this one
can show every line that actually runs, with nothing imported from an
unfamiliar sibling crate -- so keep following that pattern for your own
first tape too. Once you're comfortable with the whole layout, depending
on `tape-rt` (`tape-rt = { path = "../tape-rt" }` in your `Cargo.toml`,
then `use tape_rt::print_bytes;` instead of defining your own, and
deleting your own `src/entry.S`/panic handler) is a perfectly good way to
avoid a fourth copy of the same boilerplate -- see `vga-colors/Cargo.toml`
and `vga-colors/src/main.rs` for exactly what changes.

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
opt-level = "z"           # optimize for size: the tape image has a fixed 4 MiB limit
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
hand-written relocation tricks `tapes/primes.S` (the assembly
reference tape) has to use.

### `src/entry.S`

Copy this one verbatim from `hello-rs` or `bigsieve` (not `vga-colors`,
which gets it from `tape-rt` instead -- see above). It's a fixed 10-line
stub whose only job is to zero `.bss` (so Rust `static`/`static mut`
variables start at zero in both the ELF and legacy-flat workflows; the ELF
loader also clears each loadable segment's BSS tail) and then `jmp`
into your Rust `tape_main`. You will not need to touch this file even for
a fairly involved tape.

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
  decimal-digit loop (see `print_u32` in `tapes/hello-rs/src/main.rs`)
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
`0xb8000` instead, the same way `tapes/vga-colors/src/main.rs` does:

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
	cd tapes/your-tape && cargo +nightly-2026-09-13 build --release \
		-Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem \
		-Z json-target-spec
	mkdir -p $(BUILD)/tapes
	cp tapes/your-tape/target/i686-fos/release/your-tape-bin-name \
		$(BUILD)/tapes/your-tape.bin
```

Then add `tape-your-tape` to both the `tapes:` aggregate target and the
`.PHONY` list, and a one-line description to the `help` target, so
`make all`/`make tapes`/`make help` all pick it up automatically. To load
the tape from FOSFS on a fresh image, also add a lowercase, at-most-16-byte
file name to `TAPE_IMAGE_ARGS`, for example:

```make
	--file your.bin $(BUILD)/tapes/your-tape.bin \
```

The image target then preloads that ELF file and you can use `tape load disk
your.bin`. Build the tape on its own with:

```sh
make tape-your-tape
```

This runs `cargo build` against the kernel's own custom `i686-fos` target
(the same `-Z build-std` unstable-cargo dance the kernel itself needs,
since there's no prebuilt `std`/`core` for this target), then copies the
ELF32 output unchanged. The fixed-address linker script keeps the first
migration images compatible with the loader; future position-independent
`ET_DYN` images can use the same workflow once relocation support is added.

## 5. Loading and running it

The simplest local loop is to run `make image` after adding the
`TAPE_IMAGE_ARGS` line above, boot it with `make run`, then type:

```text
tape load disk your.bin
tape run
```

For an already-running fos machine, or to avoid rebuilding its disk image,
send the built ELF over Ethernet instead. Boot fos (`make run`, or a physical
machine set up per AGENTS.md's `term net` section), then from another terminal
on the same network:

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
one-shot tape prints its output and returns you straight to the shell prompt.
A looping tape keeps running until F12 (or F4/F6/F7/F8/F10) is pressed
locally, or the remote `term net` peer sends its own abort request -- either
way, control returns to the shell without ending the `term net` session
itself.

## 6. When 4 MiB isn't enough: `alloc`/`free`

Every tape's own compiled code, `static` data, and stack live inside its
fixed 4 MiB region (`pmem::TAPE_BASE`/`TAPE_LEN`) -- but that region isn't
a hard ceiling on how much memory your *running* tape can use. Declare
the four-parameter `tape_main` signature from section 1 and you get two
more callbacks:

```rust
let bytes: u32 = 64 * 1024 * 1024; // 64 MiB
let address = alloc(bytes);
if address == 0 {
    // Not enough contiguous memory was free -- always check this.
} else {
    let buffer: &mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(address as *mut u8, bytes as usize) };
    // `alloc` does NOT zero the memory for you -- do it yourself if you
    // need a clean buffer, the same way `bigsieve` does.
    for byte in buffer.iter_mut() {
        *byte = 0;
    }
    // ... use `buffer` ...
    free(address, bytes); // give it back before returning
}
```

A few things worth knowing before you reach for this:

- **Always check for a zero return from `alloc`.** It means there wasn't
  a big enough contiguous free block left -- this machine's RAM, the
  amount already handed out to other tape allocations, and the degree of
  fragmentation in the remaining 1 MiB chunks are all things a tape can't
  control.
- **`alloc`/`free` round up to whole 1 MiB chunks** (`pmem::
  BIG_CHUNK_SIZE`), a coarser granularity than the general kernel frame
  pool's 4 KiB. Fine for the large, single buffers this is meant for;
  wasteful if you call it many times for small allocations.
- **There is no auto-free.** If your tape returns (or panics) without
  calling `free`, that memory is gone for the rest of this boot -- the
  same unforgiving, trust-the-caller contract the kernel shell's own
  `palloc`/`pfree` commands already have.
- **The big pool spans 128 MiB through 2 GiB, but only RAM actually
  reported by E820 is usable.** Under QEMU without an explicit `-m`
  flag, that's often not true -- `tapes/test_bigsieve_e2e.py` passes
  `-m 256` for exactly this reason. On real hardware with a gigabyte or
  more, this is rarely a concern.

See AGENTS.md's "Tape ABI grows alloc/free: using the rest of the
machine's RAM" for the full story of why this callback pair exists (and
the kernel-side memory pool it's backed by), and `tapes/bigsieve/src/
main.rs` for a complete worked example: a 64 MiB dynamically-allocated
sieve of Eratosthenes, including why its counting loop can't just reuse
`hello-rs`'s algorithm unchanged at this larger scale (an integer
overflow trap worth understanding even outside this project).

## 7. Reading input and startup arguments

Every example so far only ever *prints*. Declare the full seven-parameter
`tape_main` signature from section 1 and a tape can also receive a
startup parameter and read live keystrokes -- whether typed locally or
sent over an active `term net` session; a tape has no way to tell which,
by design, and doesn't need to.

**Startup arguments** are whatever text followed `tape run` on the
command line:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    should_abort: extern "C" fn() -> u32,
    _alloc: extern "C" fn(u32) -> u32,
    _free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
) {
    let args: &[u8] =
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    if args.is_empty() {
        print_bytes(putc, b"No arguments given -- try `tape run` with some text.\n");
    } else {
        print_bytes(putc, b"You passed: ");
        print_bytes(putc, args);
        print_bytes(putc, b"\n");
    }
    // ...
}
```

`tape run 2 + 2` gives `args` as `b"2 + 2"`; plain `tape run` gives an
empty slice. There's no parsing convention imposed here at all -- your
tape decides what the bytes mean (a whole expression, one flag character,
whatever fits your program).

**Interactive input** works by polling `read_key()` once per loop
iteration, exactly like `should_abort()` (in fact, don't call both --
see the warning below):

```rust
const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

loop {
    match read_key() {
        TAPE_EVENT_NONE => {
            // nothing waiting yet this iteration -- keep polling
        }
        TAPE_EVENT_ABORT => break,
        event => {
            let byte = (event - TAPE_EVENT_CHAR_BASE) as u8;
            // ... handle `byte`, e.g. echo it, append it to a line buffer ...
        }
    }
}
```

A backspace byte (`8`) is special-cased for you: the kernel erases it
from the screen (locally and over `term net`, cursor and all) before
`read_key` ever returns it, so your own line-editing code only needs to
shrink its own buffer length by one -- it should not also try to print
anything for a backspace itself.

**Important: use `should_abort` *or* `read_key`, never both in the same
tape.** They both end up draining the exact same underlying input
sources (the local keyboard's scancode queue, and an active `term net`
session's incoming network frames) -- polling both from the same loop
races them for the same bytes and will intermittently lose keystrokes to
whichever one happens to poll first. `read_key`'s own `TAPE_EVENT_ABORT`
already reports everything `should_abort` would have told you, so a
tape reading input never needs `should_abort` at all.

See AGENTS.md's "Tape ABI grows again: `read_key` and startup arguments"
for the full story of why this shape was chosen -- including why
line-based input (reading a whole line, not one keystroke) is
deliberately left as something you build yourself out of `read_key`
in ordinary Rust, rather than a second kernel callback.

### Touch screens and mice

`tape_rt::touch_event()` returns the next finger event; `touch_extent(event.panel)` is the panel's coordinate range, so the screen
position is `x * width / extent.0`. A USB touchscreen is a panel with several fingers (ids); a PS/2 or USB mouse or touchpad is panel 6
with finger id 100 (`event.is_mouse()`), its extent is the screen size, `down` means the left button is held, and a move with the
button up arrives as `down == false` (hover). Treat a mouse click as a one-finger tap and your tape works with both. To show a
cursor, remember the last mouse position and paint `gfx::Canvas::cursor(x, y, 2, fill, outline)` last in each frame (see `tapes/miccal`).
The shell's `touch` lists the panels and `touch monitor` prints the events (Escape ends it). Details: `docs/design.md` "Mouse and touchpad".

## 8. Building a networking tape

For protocol applications, keep the same boundary as `tapes/ip-chat/`:
the kernel owns the RTL8139 and exposes only raw-frame callbacks, while
`crates/tape-net/` owns Ethernet, ARP, IPv4, UDP, TCP, and DHCP packet/state
logic. The tape owns configuration policy, fixed buffers, input, output, and
the polling loop. Do not duplicate PCI discovery, DMA setup, receive-ring
management, or NIC reset logic in the tape.

The recommended startup forms are:

```text
tape run static LOCAL_IP NETMASK GATEWAY [PEER_IP]
tape run dhcp [PEER_IP]
```

Static mode is the deterministic choice for QEMU tests. DHCP mode requires a
real DHCP server on the attached network; the multicast network used by the
repository's two-guest integration tests does not provide one. A UDP tape
must resolve the next-hop MAC with ARP, use `ticks()` for bounded polling
deadlines, and treat datagrams as best-effort: UDP provides no delivery
acknowledgement or retransmission. `tapes/ip-chat/src/main.rs` is the complete
reference, and `tapes/test_ip_chat_e2e.py` demonstrates two static peers.

For a stream-oriented application, `tapes/telnet/` is the next example. It
uses the same static configuration shape with an explicit server endpoint:

```text
tape run static LOCAL_IP NETMASK GATEWAY SERVER_IP SERVER_PORT
```

The tape drives one `tape-net::TcpConnection`, then parses Telnet control
bytes incrementally as they arrive in TCP segments. Its parser handles
`IAC`, basic option negotiation, and subnegotiation boundaries while passing
ordinary application bytes to `putc`. It deliberately treats Unicode as
UTF-8 bytes rather than decoding code points: this keeps the tape fixed-buffer
and `no_std` while preserving exactly what a UTF-8 terminal needs to render.
The host-side `tapes/test_telnet_e2e.py` fixture is a useful pattern for
testing a TCP tape without root privileges: it speaks ARP, IPv4, TCP, and the
application protocol over the same multicast Ethernet setup as the other
tape tests.

## 9. Where to look next

- `tapes/hello-rs/src/main.rs` -- the simplest complete one-shot
  Rust tape (a sieve of Eratosthenes over a small range).
- `tapes/vga-colors/src/main.rs` -- the simplest complete looping
  Rust tape (a colored-text VGA animation, abortable both locally and
  remotely).
- `tapes/bigsieve/src/main.rs` -- a one-shot tape using `alloc`/`free`
  to work with a 64 MiB buffer far bigger than its own 4 MiB region.
- `tapes/calc/src/main.rs` -- a reverse-Polish-notation calculator
  demonstrating both `read_key` (an interactive REPL) and startup
  arguments (`tape run 3 4 +`), the section 7 additions above, in a
  complete real tape.
- `crates/tape-net/src/lib.rs` -- the shared fixed-buffer networking library,
  including Ethernet, ARP, IPv4, ICMP, UDP, DHCPv4, and bounded TCP. See
  `NETWORKING.md` for how an application tape should select `dhcp` or
  `static IP NETMASK GATEWAY` mode and drive the library with the kernel
  callbacks.
- `tapes/ip-chat/src/main.rs` -- a complete UDP-over-IPv4 application tape
  using static or DHCP configuration, ARP next-hop resolution, line input,
  and the `/to`/`/broadcast`/`/peers`/`/quit` command policy described above.
- `tapes/telnet/src/main.rs` -- a complete fixed-buffer TCP/Telnet terminal
  client, including static endpoint parsing, incremental option negotiation,
  UTF-8 byte preservation, and terminal input forwarding.
- `tapes/test_telnet_e2e.py` -- an unprivileged ARP/IPv4/TCP/Telnet fixture
  that validates the tape's wire behavior and shell recovery.
- `tapes/primes.S` -- the original hand-written, position-
  independent assembly tape, useful for understanding what Rust's fixed
  load address saves you from having to do by hand.
- `tapes/longmode.S` -- not Rust (this guide is Rust-specific, see its
  own header comment and AGENTS.md's "A tape that visits 64-bit long
  mode and comes back" instead), but worth knowing about: a hand-written
  assembly tape that jumps into 64-bit long mode and back, entirely
  inside one `tape run` call.
- AGENTS.md's "Tape print callback ABI", "Tapes: fixed load address, and
  writing one in Rust", "Tape ABI grows a second callback", "Tape ABI
  grows alloc/free", "Tape ABI grows again: `read_key` and startup
  arguments", and "Tape ABI grows kernel-managed networking" sections --
  the full historical and technical reasoning behind every constraint
  described above.
- `kernel/src/shell.rs`'s `tape_run` doc comment -- the authoritative,
  in-code description of the ABI all twenty-two parameters must satisfy.
