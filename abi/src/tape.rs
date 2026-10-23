//! The tape ABI, version 1: what the kernel hands a tape.
//!
//! A tape is a position-independent ELF64 executable whose entry point is
//! `extern "C" fn(api: &TapeApi)` (System V ABI). Everything the kernel offers is a
//! field of one table, so there is one thing to pass and nothing to count: where v1
//! appended up to 43 cdecl arguments, a tape here just reads the fields it needs.
//!
//! Append-only: fields are only ever added at the end, and `size` is the number of
//! bytes the kernel's table has, so a tape built against an older layout is unaffected
//! and a tape built against a newer one can check `size` before using a late field.
//! `read_key` and `should_abort` drain the same input: use one or the other.
//!
//! The same rule holds for the structs the kernel *fills in* for a tape (`AudioInfo`, `AudioStats`, `DiskInfo`): never add a
//! field to one. A tape built before the field would hand the kernel a smaller struct than it writes, and the kernel would write
//! past its end. New information gets a new call and a new struct (`audio_buffer`/`AudioBuffer` was added for exactly this reason).

/// Version 2 added `puts` (a whole string in one console call). A tape built for 2 checks the table's version before
/// using it, so it still runs on a kernel that only has version 1. Version 3 added `audio_devices` and `audio_use_device`. Version 4 added `nic_active`. Version 5 added `peer_info`. Version 6 added `wall_clock_set`. Version 7 added the `serial_*` calls. Version 8 added `console_size`. Version 9 added the `nic_ts_*` hardware timestamp calls and `nic_send_stamped`. Version 10 added `nic_ts_adjust` and `nic_ts_step` (steering the card's clock). Version 11 added `nic_ts_all`. Version 12 added `nic_multicast_join`. Version 13 added `nic_vlan_filter`. Version 14 added `nic_tx_offload`. Version 15 added `nic_counters`. Version 16 added `fp_level`. Version 17 added the `gfx_*` calls. Version 18 added `audio_use_output`. Version 19 added `fs_list`. Version 20 added `gfx_screens` and the `gfx_*_on` calls (a screen number). Version 21 added `audio_float`. Version 22 added the `gpio_*` calls. Version 23 added `i2c_speed` and `i2c_transfer`.
pub const TAPE_API_VERSION: u32 = 23;

/// `read_key` return values: nothing, abort (the operator asked the tape to stop), or
/// `KEY_BASE + byte` for a typed byte.
pub const KEY_NONE: u32 = 0;
pub const KEY_ABORT: u32 = 1;
pub const KEY_BASE: u32 = 0x100;

/// `fs_read`/`fs_write` error returns (negative).
pub const FS_NOT_FOUND: isize = -1;
pub const FS_TOO_BIG: isize = -2;
pub const FS_ERROR: isize = -3;
pub const FS_BAD_NAME: isize = -4;
pub const FS_NO_VOLUME: isize = -5;

#[repr(C)]
pub struct TapeApi {
    pub version: u32,
    /// `size_of` the table as the kernel built it.
    pub size: u32,
    /// Prints one byte on the console (screen, serial, and a `term net` client).
    pub putc: extern "C" fn(u32),
    /// 1 if the operator asked the tape to stop.
    pub should_abort: extern "C" fn() -> u32,
    /// Non-blocking: `KEY_NONE`, `KEY_ABORT`, or `KEY_BASE + byte`. Reads the local
    /// keyboard or the `term net` session, whichever has input.
    pub read_key: extern "C" fn() -> u32,
    /// Bit 0: raw keys (Backspace is delivered, not erased on screen by the kernel).
    pub set_input_mode: extern "C" fn(u32),
    /// Sleep until the next interrupt (a frame, a key, the timer tick). Use it when a
    /// polling pass found nothing to do, instead of spinning.
    pub wait_for_event: extern "C" fn(),
    /// Timer ticks since boot (100 per second).
    pub ticks: extern "C" fn() -> u64,
    /// Microseconds since boot.
    pub micros: extern "C" fn() -> u64,
    /// Allocates `bytes` (rounded up to whole 4 KiB pages) of zeroed physical RAM,
    /// identity-mapped; 0 if there is none. One pool, any size, anywhere in RAM.
    pub alloc: extern "C" fn(usize) -> usize,
    /// Returns what `alloc` gave (same address, same size). Nothing is freed
    /// automatically when a tape returns.
    pub free: extern "C" fn(usize, usize),
    /// The tape's command-line arguments: the raw bytes after `tape run`.
    pub args: *const u8,
    pub args_len: usize,
    /// `fs_read(name, name_len, buffer, capacity)`: a whole file from the boot FOSFS
    /// volume; its length, or a negative `FS_*` code. Too small a buffer is
    /// `FS_TOO_BIG`, never a truncation.
    pub fs_read: extern "C" fn(*const u8, usize, *mut u8, usize) -> isize,
    /// `fs_write(name, name_len, data, len)`: create or replace a file; 0 or a negative code.
    pub fs_write: extern "C" fn(*const u8, usize, *const u8, usize) -> isize,
    /// Network interfaces (zero-based indices).
    pub nic_count: extern "C" fn() -> usize,
    /// Writes the 6-byte MAC to the pointer; 1 on success.
    pub nic_mac: extern "C" fn(usize, *mut u8) -> u32,
    /// Sends one raw Ethernet frame; 1 if it was queued.
    pub nic_send: extern "C" fn(usize, *const u8, usize) -> u32,
    /// Copies the next received frame (not one of the terminal's own) into the buffer
    /// and returns its length, 0 if none.
    pub nic_receive: extern "C" fn(usize, *mut u8, usize) -> usize,
    pub nic_max_frame_len: extern "C" fn() -> usize,
    /// The firmware memory map: UEFI descriptors, `memory_map_descriptor_size` bytes
    /// apart (kind 7 is free RAM, kinds 1-6, 9, 10 are RAM in use or reclaimable).
    pub memory_map: *const u8,
    pub memory_map_count: usize,
    pub memory_map_descriptor_size: usize,
    /// Pages (4 KiB) of RAM the kernel's allocator still has free: a soak test compares it
    /// before and after to catch leaks.
    pub free_pages: extern "C" fn() -> usize,
    /// `fs_remove(name, name_len)`: delete a file; 0, or `FS_NOT_FOUND`/another negative code.
    pub fs_remove: extern "C" fn(*const u8, usize) -> isize,
    /// Promiscuous mode on/off for an interface (receive every frame); 1 on success.
    /// A tape that turns it on must turn it off before returning.
    pub nic_set_promiscuous: extern "C" fn(usize, u32) -> u32,
    /// USB audio. Fills `AudioInfo` (what the attached audio device offers and what is running);
    /// returns 1 if there is a device, 0 if not.
    pub audio_info: extern "C" fn(*mut AudioInfo) -> u32,
    /// Starts streams at `rate` Hz: capture and/or playback (non-zero = yes). 0 on success, or a
    /// negative `AUDIO_*` code. A started stream keeps running until `audio_stop`.
    pub audio_start: extern "C" fn(u32, u32, u32) -> i32,
    /// Stops every running stream and releases its buffers.
    pub audio_stop: extern "C" fn(),
    /// Copies up to `len` bytes of captured audio (interleaved frames, the format `AudioInfo` says) into
    /// the buffer; returns how many bytes it had. Never blocks; call it from the tape's loop, which is
    /// also what lets the kernel service the device's isochronous transfers.
    pub audio_read: extern "C" fn(*mut u8, usize) -> usize,
    /// Queues up to `len` bytes of playback audio (interleaved frames); returns how many fitted in the
    /// kernel's ring. A short return means "full: come back later".
    pub audio_write: extern "C" fn(*const u8, usize) -> usize,
    /// Fills `AudioStats` with the counters of the running streams.
    pub audio_stats: extern "C" fn(*mut AudioStats),
    /// Receive every multicast frame on an interface (on) or only broadcasts and our own address (off); 1 on
    /// success. A tape that joins an IGMP group needs this: the NIC drops multicast it has no filter for.
    pub nic_set_multicast: extern "C" fn(usize, u32) -> u32,
    /// `fs_stream_open(name, name_len, max_bytes)`: starts a file that grows as it is written, for recording to disk.
    /// A file is one contiguous run of sectors, so the space is reserved now: `max_bytes`, or the largest free run
    /// when 0. Returns the reserved size in bytes, or a negative code. One stream at a time; opening replaces the last.
    pub fs_stream_open: extern "C" fn(*const u8, usize, u64) -> isize,
    /// `fs_stream_write(offset, data, len, file_length)`: writes at a byte offset (a multiple of 512; the last sector
    /// of the data is padded) and, if `file_length` is not 0, makes that the file's length. 0 or a negative code.
    pub fs_stream_write: extern "C" fn(u64, *const u8, usize, u64) -> isize,
    /// `fs_stream_close(file_length)`: ends the stream, making `file_length` the final length and freeing the rest of
    /// the reserved space. Without it the whole reservation stays allocated to the file.
    pub fs_stream_close: extern "C" fn(u64) -> isize,
    /// `puts(text, length)`: prints a string through the console in one call (version 2). Printing a byte at a time with
    /// `putc` repaints the whole text grid for every byte; a full-screen tape needs this one.
    pub puts: extern "C" fn(*const u8, usize),
    /// `audio_select(direction, mode)`: picks which of the device's modes (alternate settings: channels and bit depth) the next
    /// `audio_start` uses; direction 0 capture, 1 playback; mode is the 1-based number `audio modes` shows, 0 = automatic (the
    /// best one that offers the rate). 0 on success, else an `AUDIO_*` code. It applies to the next start.
    pub audio_select: extern "C" fn(u32, u32) -> i32,
    /// `hostname(nic, buffer, capacity)`: the name this machine announces on interface `nic` (zero-based; the `hostname`
    /// file's name plus the one-based interface number, or `fos2`), copied into the buffer; its length, 0 if it did not fit.
    /// A tape puts it in its DHCP requests so the server learns the machine's name.
    pub hostname: extern "C" fn(usize, *mut u8, usize) -> usize,
    /// `disk_info(number, out)`: what disk `number` (one-based, as `disks` lists them) holds, for a tape that lets the user
    /// choose where to write. 0 if there is no such disk. The kernel mounts the volume to find out, so call it when a
    /// menu opens, not every frame. A tape built against a kernel without this call sees `size` smaller than its own
    /// `TapeApi` and must not call it.
    pub disk_info: extern "C" fn(usize, *mut DiskInfo) -> u32,
    /// `disk_format_exfat(number, label, label_len)`: erases disk `number` (one-based) and makes it one exFAT volume that
    /// a PC can read (GPT, cluster size chosen from the disk's size); `label` is up to 11 ASCII characters, may be empty.
    /// 0 on success, else a `FORMAT_*` code. Everything on the disk is lost, so the tape must have asked the user; the boot
    /// disk is refused here whatever the tape says.
    pub disk_format_exfat: extern "C" fn(usize, *const u8, usize) -> i32,
    /// `audio_buffer(out)`: the capture ring's size and how much is waiting in it (64-bit, unlike `AudioInfo`'s 32-bit
    /// `capture_available`): the backlog a slow disk builds, against the room before audio is dropped. Zeros when capture is not
    /// running. A separate call, not more fields in `AudioInfo`, so that tapes built before it was added never have the kernel
    /// write past the end of their copy of that struct.
    pub audio_buffer: extern "C" fn(*mut AudioBuffer),
    /// `audio_set_capture_ring(bytes)`: the size of the capture ring the next `audio_start` makes (the default is 16 MiB; the
    /// kernel halves a request it cannot satisfy down to that). The ring is what absorbs a disk that stalls or is slower
    /// than the audio, so a recorder asks for as much memory as it can spare. The size granted is `capture_ring_bytes` in
    /// `AudioInfo` once capture runs. The request is forgotten when the tape ends.
    pub audio_set_capture_ring: extern "C" fn(u64),
    /// `wall_clock(out)`: the local time of day (the machine's RTC, read once at boot and advanced by the timer; `date set` sets
    /// it). 1 and `out` filled if the clock is set, 0 if the RTC held no valid time (a dead battery) -- a tape then has no
    /// date to show and must not invent one.
    pub wall_clock: extern "C" fn(*mut WallClock) -> u32,
    /// `audio_devices(number, out)` (version 3): how many audio devices the machine has, numbered as `audio` lists them (USB
    /// audio first, then each HD Audio controller that has a playback or capture path). If `number` is 1 to that count, `out` is
    /// filled with that device's `AudioDevice`; `number` 0 only counts. Check `version >= 3` and `size` before calling.
    pub audio_devices: extern "C" fn(u32, *mut AudioDevice) -> u32,
    /// `audio_use_device(number)` (version 3): makes device `number` (1-based, as `audio_devices` numbers them) the one every other
    /// `audio_*` call uses; 0 means automatic (USB audio if present, else the best HD Audio controller). Stops anything running
    /// first. 0 on success, `AUDIO_NO_DEVICE` for a number that does not exist. The choice is undone when the tape ends, so a
    /// tape never changes what the operator picked with `audio device N`.
    pub audio_use_device: extern "C" fn(u32) -> i32,
    /// `nic_active()` (version 4): the zero-based number of the interface the operator chose with `nic use N` (default 0). The
    /// single-interface tape calls of the old API (`tape-rt`'s `compat::nic_mac/send/receive`, used by `net-chat`, `ip-chat` and
    /// `telnet`) go to this interface, as they did in v1; a tape that picks interfaces itself uses `nic_mac(index, ...)` and friends
    /// directly. Check `version >= 4` and `size` before calling; before version 4 the answer is always 0.
    pub nic_active: extern "C" fn() -> u32,
    /// `peer_info(number, out)` (version 5): the other fos machines this one has heard announce their names (every machine
    /// broadcasts its `hostname`, or `fos-` and the last three MAC bytes, every 10 s and when it boots; see `nic discover`).
    /// Returns how many are known; if `number` is 1 to that count, `out` is filled with that peer, `number` 0 only counts.
    /// A tape resolves a machine name to a MAC address with it, without asking on the wire. Check `version >= 5` and `size`.
    pub peer_info: extern "C" fn(usize, *mut PeerInfo) -> usize,
    /// `wall_clock_set(time)` (version 6): sets the clock to `time` (local time, like `wall_clock` reports and `date set` takes) and
    /// writes it to the RTC. 1 if it was set, 0 if the date is not valid (before 2024 or after 2099). The clock steps at once, so a
    /// tape calls it only when nothing is being recorded; `ptp settime` does it once, when it first locks to a master with real
    /// time. Check `version >= 6` and `size` before calling.
    pub wall_clock_set: extern "C" fn(*const WallClock) -> u32,
    /// `serial_count()` (version 7): how many serial ports a tape may use, numbered 1 to that count. COM1 is not among them: it is
    /// the console. Check `version >= 7` and `size` before calling this or any `serial_*` call.
    pub serial_count: extern "C" fn() -> usize,
    /// `serial_info(number, out)` (version 7): fills `SerialInfo` for port `number` (1-based); 1 on success, 0 for no such port.
    pub serial_info: extern "C" fn(usize, *mut SerialInfo) -> u32,
    /// `serial_open(number, baud, format)` (version 7): programs the port (`format` is `SERIAL_8N1` or'ed with `SERIAL_STOP2` and
    /// one parity flag), clears its FIFOs and raises DTR and RTS. Opening an open port reprograms it. 0 or a negative `SERIAL_*`
    /// code. Every port a tape opened is closed when it ends.
    pub serial_open: extern "C" fn(usize, u32, u32) -> i32,
    /// `serial_close(number)` (version 7): stops using the port (DTR and RTS drop).
    pub serial_close: extern "C" fn(usize),
    /// `serial_read(number, buffer, capacity)` (version 7): copies received bytes out; returns how many. Never blocks. The kernel
    /// moves the UART's FIFO into a ring whenever the tape calls `read_key`, `should_abort`, `wait_for_event` or this, so call
    /// one of them at least every millisecond or two at 115200 baud.
    pub serial_read: extern "C" fn(usize, *mut u8, usize) -> usize,
    /// `serial_write(number, data, len)` (version 7): queues bytes for sending; returns how many the transmitter took (at most
    /// its 16-byte FIFO per call, so a short return means "come back later"). Never blocks.
    pub serial_write: extern "C" fn(usize, *const u8, usize) -> usize,
    /// `serial_lines(number)` (version 7): the modem control lines, `SERIAL_LINE_*` bits (CTS, DSR, RI, DCD in; DTR, RTS out).
    pub serial_lines: extern "C" fn(usize) -> u32,
    /// `serial_set_lines(number, lines)` (version 7): sets the output lines (`SERIAL_LINE_DTR`, `SERIAL_LINE_RTS`).
    pub serial_set_lines: extern "C" fn(usize, u32),
    /// `serial_break(number, on)` (version 7): holds the transmit line low (a break) while `on` is non-zero.
    pub serial_break: extern "C" fn(usize, u32),
    /// `console_size()` (version 8): the text grid of the console, `columns << 16 | rows` (0 when there is no console). A tape
    /// that lays out a screen or tells a remote host its window size (Telnet NAWS) uses it instead of assuming 80x25.
    pub console_size: extern "C" fn() -> u32,
    /// `nic_ts_enable(index)` (version 9): arms the interface's IEEE 1588 hardware timestamps (Intel I350 today); 1 if it has
    /// them, 0 if not (the tape then keeps its software stamps). Received PTP event frames (Ethernet 0x88f7 or UDP port 319)
    /// are then stamped by the card, and `nic_send_stamped` returns transmit stamps. Check `version >= 9` and `size`.
    pub nic_ts_enable: extern "C" fn(usize) -> u32,
    /// `nic_ts_clock(index)` (version 9): the card's own clock in nanoseconds (the timeline of every stamp; 40 bits, wraps in
    /// about 18 minutes, so subtract with wrapping and mask). 0 on a card without timestamps.
    pub nic_ts_clock: extern "C" fn(usize) -> u64,
    /// `nic_ts_rx(index)` (version 9): the hardware stamp of the frame the last `nic_receive(index, ...)` returned, in the
    /// card's clock; 0 if that frame has none (not a PTP event, or the card had no stamp free). Valid until the next receive.
    pub nic_ts_rx: extern "C" fn(usize) -> u64,
    /// `nic_send_stamped(index, frame, length, stamp)` (version 9): like `nic_send`, and stores the frame's transmit stamp (card
    /// clock) in `*stamp`, 0 if none. Returns 1 if the frame was sent, 0 if not. Waits for the stamp, a few tens of microseconds.
    pub nic_send_stamped: extern "C" fn(usize, *const u8, usize, *mut u64) -> u32,
    /// `nic_ts_adjust(index, ppb)` (version 10): runs the card's clock `ppb` parts per billion fast (negative: slow); 0 is the
    /// nominal rate. 1 if done, 0 on a card without timestamps. The rate stays until changed; a tape that ends should restore 0.
    pub nic_ts_adjust: extern "C" fn(usize, i32) -> u32,
    /// `nic_ts_step(index, delta_ns)` (version 10): moves the card's clock by `delta_ns` (signed). 1 if done, 0 if not.
    pub nic_ts_step: extern "C" fn(usize, i64) -> u32,
    /// `nic_ts_all(index, on)` (version 11): after `nic_ts_enable`, makes the card stamp *every* received frame, not only PTP
    /// events, so `nic_ts_rx` after each `nic_receive` gives its arrival time in the card's clock (0 = off again). 1 if done,
    /// 0 if the card cannot. Meant for latency measurement (nettest) and RTP arrival times (audio-net).
    pub nic_ts_all: extern "C" fn(usize, u32) -> u32,
    /// `nic_multicast_join(index, mac, on)` (version 12): `mac` points at 6 bytes, a multicast address. Lets frames for that one
    /// group through the card's hash filter (on) or stops that (off; joins are counted), without `nic_set_multicast`'s "every
    /// multicast frame". 1 if done, 0 on a card without the filter (then use `nic_set_multicast`). Groups share filter bits,
    /// so a tape still checks the address. The kernel drops every group when a tape ends.
    pub nic_multicast_join: extern "C" fn(usize, *const u8, u32) -> u32,
    /// `nic_vlan_filter(index, vid, on)` (version 13): registers (on) or drops VLAN id `vid` (0-4095) in the card's VLAN
    /// filter. With the first id the card starts dropping tagged frames of other ids (untagged frames always pass), so a
    /// tape that never calls it sees every tag as before. The filter stays off while the card is promiscuous. 1 if done,
    /// 0 on a card without it (the 82579 and I219 have none) or an invalid id. The kernel clears all ids (filter off) when a tape ends.
    pub nic_vlan_filter: extern "C" fn(usize, u32, u32) -> u32,
    /// `nic_tx_offload(index, on)` (version 14): with it on, `nic_send` lets the card compute the TCP/UDP checksum of an
    /// unfragmented IPv4 frame: a UDP frame whose checksum field is 0 gets the real checksum (the tape need not compute it;
    /// a UDP frame with a checksum, and every other frame, goes unchanged), and a TCP frame gets its checksum field
    /// overwritten. Bytes after the IP total length are dropped. 1 if done, 0 on a card without it (the I350 has it; the
    /// tape then checksums itself). The kernel turns it off when a tape ends.
    pub nic_tx_offload: extern "C" fn(usize, u32) -> u32,
    /// `nic_counters(index, out, count)` (version 15): writes up to `count` u64 values to `out` and returns how many it wrote
    /// (0: no such interface). The order is fixed, new values only at the end: 0 negotiated Mbit/s (0 unknown), 1 full duplex
    /// (0 or 1), 2 packets the card missed for want of receive buffers, 3 no-buffer events, 4 CRC errors, 5 symbol errors,
    /// 6 PAUSE (XOFF) frames received, 7 PAUSE frames sent, 8 frames the kernel's tape queue dropped. Values 2-7 are 0 on a
    /// card without these counters; all but 0 and 1 are running totals since boot, so take two snapshots and subtract.
    pub nic_counters: extern "C" fn(usize, *mut u64, usize) -> usize,
    /// `fp_level()` (version 16): what floating point the kernel switched on for this tape, `FP_*`. The kernel does it before
    /// the tape's entry runs (a hard-float build may use an XMM register in its very first instruction, so a call made from
    /// the tape would come too late) and undoes it when the tape ends. Only a tape built for `x86_64-fos-tape-fp` may use
    /// SSE; a soft-float tape never notices. The kernel itself stays soft-float and never touches XMM/YMM, so the state is
    /// the tape's alone. A tape that wants AVX2 or FMA checks for `FP_AVX2` first (an `#[target_feature]` function, never
    /// the baseline build), and falls back to the SSE2 code otherwise.
    pub fp_level: extern "C" fn() -> u32,
    /// `gfx_info()` (version 17): the pixel framebuffer's size as `width << 32 | height`, 0 when the machine has none (a BIOS
    /// boot on the VGA text screen, or no video at all). Tape pixels are `0x00RRGGBB` whatever the screen's byte order is.
    pub gfx_info: extern "C" fn() -> u64,
    /// `gfx_open()` (version 17): the tape takes the screen. The console stops painting (its text grid, the serial port and
    /// `term net` carry on, so `println!` still reaches those) and the screen is cleared to the console's background.
    /// 1 on success, 0 without a pixel framebuffer. The kernel gives the screen back (`gfx_close`) when the tape ends.
    pub gfx_open: extern "C" fn() -> u32,
    /// `gfx_close()` (version 17): clears the screen and draws the console's text again.
    pub gfx_close: extern "C" fn(),
    /// `gfx_fill(x, y, width, height, rgb)` (version 17): a filled rectangle of one colour, clipped to the screen. 1 if drawn,
    /// 0 if the tape has not opened the screen.
    pub gfx_fill: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    /// `gfx_blit(x, y, width, height, pixels, stride)` (version 17): copies `height` rows of `width` 0x00RRGGBB pixels
    /// (`stride` pixels from one row to the next, at least `width`) to the screen at (x, y), clipped on the right and
    /// bottom. 1 if drawn, 0 if not (the screen is not open, `pixels` is null, `stride < width`, or (x, y) is off screen).
    /// The framebuffer is write-combining memory that is slow to read, so a tape keeps its picture in ordinary memory and
    /// blits the part that changed.
    pub gfx_blit: extern "C" fn(u32, u32, u32, u32, *const u32, u32) -> u32,
    /// `audio_use_output(number)` (version 18): sends playback to device `number` (1-based, as `audio_devices` numbers them)
    /// while capture stays on the device `audio_use_device` (or the operator) chose; 0 = playback follows the input device.
    /// `audio_start` then starts each direction on its own device, `audio_read`/`audio_write`/`audio_info`/`audio_stats` combine
    /// the two. USB audio is a single instance, so the useful pairs are USB in + HD Audio out and the reverse. Stops anything
    /// running first. 0 on success, `AUDIO_NO_DEVICE` for a number that does not exist. Undone when the tape ends.
    pub audio_use_output: extern "C" fn(u32) -> i32,
    /// `fs_list(volume, volume_len, index, out)` (version 19): the `index`th (from 0) file on a volume, as a `FileInfo`:
    /// 1 if `out` was filled, 0 when `index` is past the last file, a negative `FS_*` code (`FS_NO_VOLUME`) otherwise.
    /// `volume` names it like the prefix of a file reference: empty for the boot volume, `diskN` or `@LABEL`. The order is
    /// the directory's and stays the same while no file is added or removed. FAT32/exFAT volumes list their root
    /// directory (x86-64 only for now), subdirectories flagged `FILE_DIRECTORY`. Free space is in `disk_info`.
    pub fs_list: extern "C" fn(*const u8, usize, usize, *mut FileInfo) -> isize,
    /// `gfx_screens()` (version 20): how many pixel screens a tape can draw on. 0 without one, 1 on most machines; a
    /// Raspberry Pi 4 with `max_framebuffers=2` and two monitors has 2. Screen 0 is the console's.
    pub gfx_screens: extern "C" fn() -> u32,
    /// `gfx_info_on(screen)` (version 20): as `gfx_info` for screen number `screen` (0-based); 0 for a number that does not
    /// exist. The calls `gfx_info`..`gfx_blit` of versions 17-19 are these with screen 0.
    pub gfx_info_on: extern "C" fn(u32) -> u64,
    /// `gfx_open_on(screen)` (version 20): as `gfx_open`. Only screen 0 has the console; another screen is cleared to black
    /// and has no text to give back. The kernel closes every screen a tape opened when it ends.
    pub gfx_open_on: extern "C" fn(u32) -> u32,
    /// `gfx_close_on(screen)` (version 20): as `gfx_close`.
    pub gfx_close_on: extern "C" fn(u32),
    /// `gfx_fill_on(screen, x, y, width, height, rgb)` (version 20): as `gfx_fill`.
    pub gfx_fill_on: extern "C" fn(u32, u32, u32, u32, u32, u32) -> u32,
    /// `gfx_blit_on(screen, x, y, width, height, pixels, stride)` (version 20): as `gfx_blit`.
    pub gfx_blit_on: extern "C" fn(u32, u32, u32, u32, u32, *const u32, u32) -> u32,
    /// `audio_float()` (version 21): which running-or-default audio streams carry 32-bit IEEE float samples instead of
    /// signed integers, `AUDIO_FLOAT_*` bits. `audio_info` reports such a stream as 4-byte slots of 32 bits, so without
    /// this a tape would read the bit patterns as integers. A float sample is -1.0..1.0 for full scale (it may exceed it).
    pub audio_float: extern "C" fn() -> u32,
    /// `gpio_pins()` (version 22): the GPIO pins a tape may change, bit N for GPIO N (Raspberry Pi 4: GPIO2..27 except the
    /// console UART's 14 and 15, less the `gpio-deny` setting); 0 on a machine without pins. Reads cover all 58.
    pub gpio_pins: extern "C" fn() -> u64,
    /// `gpio_read()` (version 22): the level of every pin, GPIO0 in bit 0.
    pub gpio_read: extern "C" fn() -> u64,
    /// `gpio_write(mask, values)` (version 22): sets the output pins in `mask` to the matching bits of `values`; returns the
    /// pins refused (not allowed, or not an output).
    pub gpio_write: extern "C" fn(u64, u64) -> u64,
    /// `gpio_mode(pin, function, level)` (version 22): `function` 0 input, 1 output (starting at `level`, written before the
    /// pin turns into an output), 2..=7 alt0..alt5. 0 when done, -1 pin not allowed, -2 bad argument, -3 no GPIO. When the tape
    /// ends, pins it changed to an alternate function go back to input; outputs keep their level.
    pub gpio_mode: extern "C" fn(u32, u32, u32) -> i32,
    /// `gpio_pull(pin, pull)` (version 22): 0 none, 1 up, 2 down; results as `gpio_mode`. Not undone when the tape ends.
    pub gpio_pull: extern "C" fn(u32, u32) -> i32,
    /// `gpio_function(pin)` (version 22): 0 input, 1 output, 2..=7 alt0..alt5; negative without GPIO or for a bad pin.
    pub gpio_function: extern "C" fn(u32) -> i32,
    /// `i2c_speed(hz)` (version 23): the Raspberry Pi 4 header's I2C bus (BSC1, GPIO2 SDA / GPIO3 SCL, which the kernel switches
    /// to their I2C function on the first transfer). 0 asks for the current SCL speed in Hz (100000 by default); 10000..=1000000
    /// sets it and returns 0. Negative: -3 bad argument, -4 no I2C bus (not a Pi). Back to 100 kHz when the tape ends.
    pub i2c_speed: extern "C" fn(u32) -> i32,
    /// `i2c_transfer(address, out, out_len, input, in_len)` (version 23): writes `out_len` bytes to the 7-bit device `address`
    /// (0x08..=0x77) if non-zero, then (after a STOP, no repeated START) reads `in_len` bytes into `input` if non-zero; each
    /// length at most 65535. 0 on success; -1 no acknowledge, -2 timeout or clock stretched too long or a short transfer,
    /// -3 bad argument (or no bytes at all), -4 no I2C bus, -5 GPIO2/3 not allowed (`gpio-deny`).
    pub i2c_transfer: extern "C" fn(u32, *const u8, u32, *mut u8, u32) -> i32,
}

/// `audio_float` bits: the capture stream / the playback stream carries IEEE 754 single-precision samples.
pub const AUDIO_FLOAT_CAPTURE: u32 = 1;
pub const AUDIO_FLOAT_PLAYBACK: u32 = 2;

/// What `fs_list` reports about one file.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileInfo {
    pub length: u64,
    /// `FILE_*` bits.
    pub flags: u32,
    pub name_length: u32,
    /// The name, `name_length` bytes of it (FOSFS names are at most 16, FAT ones up to 64).
    pub name: [u8; 64],
}

pub const FILE_DIRECTORY: u32 = 1;

impl Default for FileInfo {
    fn default() -> Self {
        FileInfo { length: 0, flags: 0, name_length: 0, name: [0; 64] }
    }
}

impl FileInfo {
    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_length as usize).min(64)]
    }
}

/// `fp_level` returns: no SSE state enabled, SSE2 (every x86-64 CPU; `f32`/`f64` in XMM registers), or AVX2 with FMA (the
/// OS has enabled YMM state through XCR0 as well).
pub const FP_NONE: u32 = 0;
pub const FP_SSE2: u32 = 1;
pub const FP_AVX2: u32 = 2;
/// Level 1 is "the architecture's baseline floating point is on", whatever the CPU: on aarch64 that is NEON/FP (always
/// present there), and the kernel reports it as `FP_NEON`. Level 2 (`FP_AVX2`) exists on x86-64 only, so a tape's
/// `>= FP_AVX2` test is false on ARM, and the values are never reused for another meaning.
pub const FP_BASE: u32 = 1;
pub const FP_NEON: u32 = FP_BASE;

/// `serial_open` line formats: the low bits of the UART's line control register, so they combine with `|`.
pub const SERIAL_8N1: u32 = 0x03;
pub const SERIAL_7N1: u32 = 0x02;
pub const SERIAL_STOP2: u32 = 0x04;
pub const SERIAL_PARITY_ODD: u32 = 0x08;
pub const SERIAL_PARITY_EVEN: u32 = 0x18;

/// `serial_lines` bits.
pub const SERIAL_LINE_CTS: u32 = 0x01;
pub const SERIAL_LINE_DSR: u32 = 0x02;
pub const SERIAL_LINE_RI: u32 = 0x04;
pub const SERIAL_LINE_DCD: u32 = 0x08;
pub const SERIAL_LINE_DTR: u32 = 0x10;
pub const SERIAL_LINE_RTS: u32 = 0x20;

/// `serial_open` errors (negative).
pub const SERIAL_NO_PORT: i32 = -1;
pub const SERIAL_BAD_SETTINGS: i32 = -2;
/// The port belongs to the kernel (`serial-shell` in `settings`): the shell is on it.
pub const SERIAL_BUSY: i32 = -3;

/// `SerialInfo::kind`: what drives the port.
pub const SERIAL_KIND_UART: u32 = 1;
/// A USB serial adapter (`kernel/src/usb_serial.rs`); its ports are numbered after the UARTs.
pub const SERIAL_KIND_USB: u32 = 2;

/// What `serial_info` fills in. Kernel-filled, so never extend it: a new fact gets a new call.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SerialInfo {
    pub kind: u32,
    /// 1 while a tape has the port open.
    pub open: u32,
    /// The UART's I/O base for `SERIAL_KIND_UART`.
    pub io_base: u32,
    pub baud: u32,
    pub format: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Receive faults since the port was opened: bytes lost to an overrun (the kernel's ring or the UART's FIFO), framing
    /// errors, parity errors and breaks seen.
    pub overruns: u32,
    pub framing_errors: u32,
    pub parity_errors: u32,
    pub breaks: u32,
    /// Port name, ASCII, zero padded (`COM2`).
    pub name: [u8; 8],
}

/// What `peer_info` fills in. Kernel-filled, so never extend it: a new fact gets a new call.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PeerInfo {
    pub mac: [u8; 6],
    /// The zero-based interface the peer was heard on.
    pub nic: u8,
    /// Bytes of `name` in use.
    pub name_length: u8,
    /// The peer's announced name (ASCII, not terminated).
    pub name: [u8; 20],
}

impl PeerInfo {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..usize::from(self.name_length).min(20)]).unwrap_or("")
    }
}

/// `AudioDevice::kind` values.
pub const AUDIO_KIND_USB: u32 = 1;
pub const AUDIO_KIND_HDA: u32 = 2;
pub const AUDIO_KIND_I2S: u32 = 3;

/// What `audio_devices` reports for one device. A new struct (not more fields in `AudioInfo`), per the rule at the top of this file.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// `AUDIO_KIND_USB`, `AUDIO_KIND_HDA` or `AUDIO_KIND_I2S` (the Pi's I2S audio HAT; the other fields are 0).
    pub kind: u32,
    /// 1 if this is the device the `audio_*` calls currently use.
    pub active: u32,
    /// PCI location as `bus << 16 | device << 8 | function` for an HD Audio controller; 0 for USB audio.
    pub pci_location: u32,
    /// PCI vendor and device ID of an HD Audio controller; 0 for USB audio.
    pub vendor: u16,
    pub id: u16,
}

/// What `wall_clock` reports: local time, to the second.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct WallClock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub reserved: u8,
}

/// What `audio_buffer` reports.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioBuffer {
    /// The capture ring's size in bytes.
    pub ring_bytes: u64,
    /// Bytes in it that the tape has not read yet.
    pub available_bytes: u64,
}

pub const FORMAT_NO_DISK: i32 = -1;
pub const FORMAT_BOOT_DISK: i32 = -2;
pub const FORMAT_BAD_LABEL: i32 = -3;
pub const FORMAT_FAILED: i32 = -4;

/// `DiskInfo::kind`: what a disk's volume is. FAT32 and exFAT disks are reached as `diskN/NAME`, like a FOSFS one.
pub const DISK_NO_VOLUME: u32 = 0;
pub const DISK_FOSFS: u32 = 1;
pub const DISK_FAT32: u32 = 2;
pub const DISK_EXFAT: u32 = 3;

/// What `disk_info` reports about one disk.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiskInfo {
    pub sectors: u64,
    /// Free bytes in all and in the longest contiguous run (a recording needs one run); `u64::MAX` when not known
    /// (FAT32, or a disk with no volume).
    pub free_bytes: u64,
    pub largest_bytes: u64,
    /// `DISK_*`: the volume a tape can write files to (a disk with none is listed but unusable).
    pub kind: u32,
    /// 1 for the disk the system booted from.
    pub boot: u32,
    /// The drive's model text, space padded, and the FOSFS volume label (zero padded; empty for FAT).
    pub model: [u8; 40],
    pub label: [u8; 16],
}

impl Default for DiskInfo {
    fn default() -> Self {
        DiskInfo { sectors: 0, free_bytes: u64::MAX, largest_bytes: u64::MAX, kind: 0, boot: 0, model: [0; 40], label: [0; 16] }
    }
}

/// What `audio_info` reports. Slot bytes: how many bytes each sample takes in the stream (a 24-bit
/// sample usually travels in 3 bytes, little-endian, signed).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioInfo {
    pub class_version: u32,
    pub playback_channels: u32,
    pub playback_slot_bytes: u32,
    pub playback_bits: u32,
    pub capture_channels: u32,
    pub capture_slot_bytes: u32,
    pub capture_bits: u32,
    pub rate_count: u32,
    pub rates: [u32; 8],
    /// Non-zero while the stream runs: the sample rate it runs at.
    pub capture_rate: u32,
    pub playback_rate: u32,
    /// Bytes of room in the playback ring and bytes waiting in the capture ring.
    pub playback_space: u32,
    pub capture_available: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioStats {
    pub capture_packets: u64,
    pub capture_bytes: u64,
    pub capture_errors: u64,
    /// Captured bytes dropped because the tape did not read them in time.
    pub capture_overruns: u64,
    pub playback_packets: u64,
    pub playback_errors: u64,
    /// Packets sent as silence because the tape had not written audio in time.
    pub playback_underruns: u64,
}

/// `audio_start` errors (negative).
pub const AUDIO_NO_DEVICE: i32 = -1;
pub const AUDIO_BAD_RATE: i32 = -2;
pub const AUDIO_BUSY: i32 = -3;
pub const AUDIO_NO_DIRECTION: i32 = -4;
pub const AUDIO_FAILED: i32 = -5;
