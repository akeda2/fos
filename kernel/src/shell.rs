/// Serves one incoming NETFS request frame (`GET_REQUEST`, `PUT_REQUEST`,
/// `PUT_DATA`, `PUT_STOP`), returning whether `tag` was one of them.
/// Shared by `ambient_discovery_poll` (the idle prompt) and both of
/// `term_net`'s receive loops, so a host tool (`tools/foscopy.py`) can reach
/// a machine that sits in `term net` -- which is where every machine spends
/// its time after boot -- without a local escape to the prompt first, the
/// same reason `dispatch_kup_frame` exists. A NIC on the `settings` file's
/// `deny-term-net` list serves no file access either: a denied request is
/// consumed and dropped silently. The shared-staging-buffer caveat on
/// `NetfsPutTransfer` applies unchanged. A GET is answered synchronously, so
/// it pauses an active `term net` session for the length of the transfer.
fn dispatch_netfs_serve_frame(
    info: &BootInfo,
    nic_index: usize,
    nic: &nic::Nic,
    peer_mac: [u8; 6],
    own_mac: [u8; 6],
    tag: u8,
    payload: &[u8],
) -> bool {
    if !matches!(
        tag,
        NETFS_TAG_GET_REQUEST | NETFS_TAG_PUT_REQUEST | NETFS_TAG_PUT_DATA | NETFS_TAG_PUT_STOP
    ) {
        return false;
    }
    if term_net_denied(nic_index) {
        return true;
    }
    match tag {
        NETFS_TAG_GET_REQUEST => {
            let remote_disk_number = payload.first().copied().unwrap_or(0);
            let name = payload.get(1..).unwrap_or(&[]);
            serve_netfs_get_request(info, nic, peer_mac, own_mac, remote_disk_number, name);
        }
        NETFS_TAG_PUT_REQUEST => serve_netfs_put_request(nic, peer_mac, own_mac, payload),
        NETFS_TAG_PUT_DATA => serve_netfs_put_data(peer_mac, payload),
        _ => serve_netfs_put_stop(info, nic, peer_mac, own_mac),
    }
    true
}

use crate::arch::x86::{halt_forever, halt_until_interrupt, reset, wait_for_interrupt};
use crate::audio;
use crate::boot_info::{BootInfo, MemoryRegion};
use crate::console;
use crate::cpu;
use crate::disk;
use crate::fosfs;
use crate::hda;
use crate::interrupts;
use crate::keyboard;
use crate::keyboard::Keyboard;
use crate::nic;
use crate::parallel;
use crate::pci;
use crate::pmem;
use crate::uhci;
use crate::usb_storage;
use crate::{print, println};

const LINE_CAPACITY: usize = 256;
const TAPE_ARGUMENT_FILE_CAPACITY: usize = 4096;
const START_FILE_NAME: &[u8] = b"start";
const START_FILE_CAPACITY: usize = 4096;
const TERM_NET_IDLE_TICKS: u64 = 60 * 100;
/// How long (in PIT ticks, ~10ms each) the idle prompt loop must see no NIC
/// activity from `ambient_discovery_poll` before it starts halting the CPU
/// between polls. Kept comfortably above the ~3ms chunk pacing NETFS/KUP
/// senders use (`tools/send_kernel_update.py`'s default `--delay`) so a
/// transfer in flight never sees this loop sleep through the next chunk --
/// see `arch::x86::halt_until_interrupt`'s doc comment for why a halted
/// iteration still wakes within one PIT tick regardless.
const AMBIENT_IDLE_GRACE_TICKS: u64 = 2;
const HOSTNAME_FILE_NAME: &[u8] = b"hostname";
/// Generous for a short, hand-typed machine name; kept small since it is
/// duplicated into every peer table entry (see `PEER_HOSTNAME_CAPACITY`).
const HOSTNAME_CAPACITY: usize = 16;
/// `HOSTNAME_CAPACITY` plus room for a 1-based interface number appended by
/// `interface_hostname` -- two digits comfortably covers `nic::MAX_NICS`
/// (currently 32).
const INTERFACE_HOSTNAME_CAPACITY: usize = HOSTNAME_CAPACITY + 2;

/// This machine's configured base hostname (before a per-interface number is
/// appended -- see `interface_hostname`), loaded once at boot by
/// `load_hostname`. Empty (`HOSTNAME_LEN == 0`) means no `hostname` file was
/// found anywhere, in which case discovery frames carry no hostname payload
/// and every peer is shown by bare MAC address alone, exactly as before this
/// feature existed.
static mut HOSTNAME: [u8; HOSTNAME_CAPACITY] = [0; HOSTNAME_CAPACITY];
static mut HOSTNAME_LEN: usize = 0;

const SETTINGS_FILE_NAME: &[u8] = b"settings";
const SETTINGS_FILE_CAPACITY: usize = 512;
/// Bit N set means `term net` ignores a `HELLO` arriving on NIC index N
/// (zero-based internally; the `settings` file uses the one-based numbers `nic
/// list` prints). Loaded once at boot by `load_settings`; zero (the default)
/// leaves every interface answering, exactly as before the file existed.
static mut TERM_NET_DENY_MASK: u32 = 0;

pub fn run(info: &BootInfo) -> ! {
    let keyboard_available = Keyboard::initialize().is_some();
    interrupts::initialize();
    crate::debug::report_and_clear_previous_crash();
    disk::initialize(info);
    disk::discover_secondary_disks(info);
    load_hostname(info);
    load_settings(info);
    crate::debug::load_config(info);
    crate::debug::maybe_crash_on_boot();
    crate::debug::confirm_boot(info);
    crate::debug::arm_watchdog();
    if keyboard_available {
        println!("PS/2 keyboard initialized. Type 'help' for commands.");
    } else {
        println!("PS/2 keyboard unavailable; serial input remains available.");
        println!("Type 'help' for commands.");
    }
    if nic::count() > 0 {
        println!("Detected Ethernet interfaces:");
        nic_list();
    }
    run_start_file(info);
    // Automatically wait for a remote `term net` client at every boot,
    // exactly as if a local operator had just typed `term net`: this
    // makes every fos machine remotely reachable by default with no
    // manual step, which matters most for a machine with no attached
    // screen/keyboard at all. A local operator who wants the ordinary
    // interactive shell right away, without waiting, can press F12 (or
    // F4/F6/F7/F8/F10) immediately to give up waiting, exactly as they
    // would to leave a manually started `term net` session; if a remote
    // client does connect first, a remote disconnect returns to waiting and
    // a local escape returns to the local shell below.
    term_net(info);
    let mut editor = LineEditor::new();
    prompt();
    let mut last_activity = interrupts::ticks();
    let mut last_network_activity = interrupts::ticks();

    loop {
        crate::debug::watchdog_pet(); // the idle prompt loop is the foreground's heartbeat
        crate::debug::mark_stable_if_due(info);
        let byte = console::read_serial_byte().or_else(keyboard::pop_char);
        if ambient_discovery_poll(info) {
            last_network_activity = interrupts::ticks();
        }
        let Some(byte) = byte else {
            if interrupts::ticks().wrapping_sub(last_activity) >= TERM_NET_IDLE_TICKS {
                println!(
                    "\nNo local shell activity for 60 seconds; waiting for a remote terminal client again."
                );
                term_net(info);
                last_activity = interrupts::ticks();
                last_network_activity = interrupts::ticks();
                prompt();
            } else if interrupts::ticks().wrapping_sub(last_network_activity)
                >= AMBIENT_IDLE_GRACE_TICKS
            {
                // Nothing typed locally, and no NIC activity recently enough
                // to suspect a NETFS/KUP transfer might still be arriving in
                // paced chunks -- safe to sleep until the next interrupt
                // instead of spinning. See `AMBIENT_IDLE_GRACE_TICKS` and
                // `arch::x86::halt_until_interrupt`'s doc comments for why
                // this can't miss a wakeup for longer than one PIT tick.
                halt_until_interrupt();
            }
            continue;
        };
        editor.feed(byte, info);
        last_activity = interrupts::ticks();
    }
}

/// Sleeps the CPU until the next interrupt, for a polling loop whose latest
/// iteration found nothing to do (no frame, no key, no work due). Without it
/// those loops spin at 100% CPU. Only sleeps when every NIC has an enabled
/// receive interrupt (`nic::all_rx_interrupt_driven`), so an arriving frame
/// wakes the CPU at once; the 100 Hz PIT tick and the keyboard IRQ bound every
/// other wait to 10 ms or less. Serial input has no interrupt, so loops that
/// read COM1 (`term`, `chat`) deliberately do not call this: a 16-byte UART
/// FIFO could overflow during a 10 ms sleep.
fn idle_wait() {
    if nic::all_rx_interrupt_driven() {
        wait_for_interrupt();
    }
}

/// Where a `start` script was found (or why it wasn't run), returned by
/// [`locate_start_file`] so both the silent boot-time call and the
/// interactive `start` command can share one lookup while still reporting
/// their own outcomes appropriately.
enum StartFileLookup {
    Found(disk::Device, u64),
    Missing,
    Error(disk::Error),
}

/// Looks for the optional `start` script on the boot disk first, falling
/// back to the first secondary disk (`disk2`, the same disk the "second
/// (non-boot) disk" config/log convention already targets) only if the
/// boot disk has none at all -- letting a deployment keep its boot script
/// alongside other disk2-resident config/log files instead of duplicating
/// it onto disk1 too. Only the boot disk's own result can produce
/// `StartFileLookup::Error`: it always carries a valid FOSFS volume, so a
/// read error there is a real problem worth reporting. disk2, in
/// contrast, is very often attached blank and unformatted (the normal
/// state before a deployment's first `format disk2`) -- `fosfs::validate`
/// fails on any such disk, and treating that the same as a genuine error
/// would print a scary, misleading message on every single boot of a
/// perfectly ordinary machine with an unused secondary disk. So *any*
/// outcome on disk2 other than actually finding the file (including every
/// error) is simply treated as "no start script there", exactly like
/// disk1's own `Ok(None)`.
fn locate_start_file(info: &BootInfo) -> StartFileLookup {
    match fosfs::file_length(info, disk::Device::Boot, START_FILE_NAME) {
        Ok(Some(length)) => return StartFileLookup::Found(disk::Device::Boot, length),
        Ok(None) => {}
        Err(error) => return StartFileLookup::Error(error),
    }
    if disk::secondary_count() == 0 {
        return StartFileLookup::Missing;
    }
    let device = disk::Device::Secondary(0);
    match fosfs::file_length(info, device, START_FILE_NAME) {
        Ok(Some(length)) => StartFileLookup::Found(device, length),
        _ => StartFileLookup::Missing,
    }
}

/// Looks for an optional `hostname` file, checking the boot disk first and
/// then *every* secondary disk in order (disk2, disk3, ...) -- unlike
/// `locate_start_file`, which only ever falls back to the first secondary
/// disk, since a `start` script is expected to sit right next to whatever
/// disk holds other boot-time config, while a hostname is just as likely to
/// be dropped onto whichever secondary disk happens to be handy on a given
/// machine. Any error (including an unformatted/blank disk, the common
/// state for a secondary disk with no config on it yet) is silently treated
/// as "no hostname there", the same reasoning `locate_start_file` already
/// applies to disk2. Called once at boot; the result is cached in
/// `HOSTNAME`/`HOSTNAME_LEN` for `interface_hostname` to use for the rest of
/// the session.
fn load_hostname(info: &BootInfo) {
    let devices = core::iter::once(disk::Device::Boot)
        .chain((0..disk::secondary_count()).map(disk::Device::Secondary));
    for device in devices {
        let length = match fosfs::file_length(info, device, HOSTNAME_FILE_NAME) {
            Ok(Some(length)) => length,
            _ => continue,
        };
        if length > HOSTNAME_CAPACITY as u64 {
            println!(
                "hostname: ignoring a {length}-byte hostname file (limit is {HOSTNAME_CAPACITY} bytes)."
            );
            continue;
        }
        let mut contents = [0u8; HOSTNAME_CAPACITY];
        let Ok(Some(read_length)) = fosfs::read(
            info,
            device,
            HOSTNAME_FILE_NAME,
            &mut contents[..length as usize],
        ) else {
            continue;
        };
        let name = trim_trailing_whitespace(&contents[..read_length as usize]);
        if name.is_empty() {
            continue;
        }
        unsafe {
            HOSTNAME[..name.len()].copy_from_slice(name);
            HOSTNAME_LEN = name.len();
        }
        let text = core::str::from_utf8(name).unwrap_or("<non-UTF-8 hostname>");
        println!(
            "hostname: configured as '{text}' (interfaces appear as '{text}1', '{text}2', ...)."
        );
        return;
    }
}

/// Looks for an optional `settings` file on the boot disk and then every
/// secondary disk in order (the first one found wins), like `load_hostname`.
/// Format: one option per line, `#` comments and blank lines ignored. The only
/// option so far is `deny-term-net N[,N...]`: one-based NIC numbers on which
/// `term net` must not accept a remote terminal and KUP must not accept a
/// kernel update -- for a router that should not offer a shell or a kernel
/// push on its outbound port, and NETFS must not serve or accept files there.
/// Discovery replies are a separate service and are not affected. Unreadable or blank disks are silently
/// skipped, as for `hostname`.
fn load_settings(info: &BootInfo) {
    let devices = core::iter::once(disk::Device::Boot)
        .chain((0..disk::secondary_count()).map(disk::Device::Secondary));
    for device in devices {
        let length = match fosfs::file_length(info, device, SETTINGS_FILE_NAME) {
            Ok(Some(length)) => length,
            _ => continue,
        };
        if length > SETTINGS_FILE_CAPACITY as u64 {
            println!(
                "settings: ignoring a {length}-byte file (limit is {SETTINGS_FILE_CAPACITY} bytes)."
            );
            continue;
        }
        let mut contents = [0u8; SETTINGS_FILE_CAPACITY];
        let Ok(Some(read_length)) = fosfs::read(
            info,
            device,
            SETTINGS_FILE_NAME,
            &mut contents[..length as usize],
        ) else {
            continue;
        };
        let (mask, no_msi) = parse_settings(&contents[..read_length as usize]);
        unsafe { TERM_NET_DENY_MASK = mask };
        if no_msi {
            nic::set_msi_disabled(true);
            println!("settings: MSI disabled; NICs keep their legacy IRQ lines.");
        }
        if mask != 0 {
            print!("settings: term net, KUP and NETFS denied on NIC");
            for bit in 0..32 {
                if mask & (1 << bit) != 0 {
                    print!(" {}", bit + 1);
                }
            }
            println!(".");
        }
        return;
    }
}

/// Parses a `settings` file's text and returns the `term net` deny mask.
/// Unknown options and bad NIC numbers are reported and skipped rather than
/// rejecting the whole file, so one typo cannot silently drop the rest.
fn parse_settings(contents: &[u8]) -> (u32, bool) {
    let mut deny_mask = 0u32;
    let mut no_msi = false;
    for line in contents.split(|&byte| byte == b'\n') {
        let line = trim_trailing_whitespace(trim_start(line));
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let (option, values) = take_word(line);
        if option == b"no-msi" {
            no_msi = true;
            continue;
        }
        if option != b"deny-term-net" {
            println!(
                "settings: unknown option '{}'.",
                core::str::from_utf8(option).unwrap_or("?")
            );
            continue;
        }
        // Commas and spaces both separate numbers: `1,2` and `1, 2` work.
        for value in values.split(|&byte| byte == b',' || byte == b' ') {
            if value.is_empty() {
                continue;
            }
            match parse_number(value) {
                Some(number @ 1..=32) => deny_mask |= 1 << (number - 1),
                _ => println!(
                    "settings: bad NIC number '{}' (use 1-32).",
                    core::str::from_utf8(value).unwrap_or("?")
                ),
            }
        }
    }
    (deny_mask, no_msi)
}

/// True when the `settings` file denies `term net` on this zero-based NIC.
fn term_net_denied(index: usize) -> bool {
    index < 32 && unsafe { TERM_NET_DENY_MASK } & (1 << index) != 0
}

/// Formats the configured hostname (see `load_hostname`) with a 1-based
/// interface number appended (e.g. `myhost1`, `myhost2`), matching `nic
/// list`'s own 1-based numbering -- this is what lets several NICs on one
/// machine appear as distinct, individually addressable names on the wire.
/// Returns 0 (nothing written) if no hostname is configured, so a machine
/// with no `hostname` file behaves exactly as it always has.
fn interface_hostname(index: usize, out: &mut [u8]) -> usize {
    let name_len = unsafe { HOSTNAME_LEN };
    if name_len == 0 {
        return 0;
    }
    let hostname = &raw const HOSTNAME;
    let name = unsafe { &(&*hostname)[..name_len] };
    out[..name_len].copy_from_slice(name);
    name_len + write_decimal(&mut out[name_len..], index + 1)
}


/// Runs the optional FOSFS `start` script before the automatic remote-terminal
/// wait. Lines use the ordinary shell dispatcher, so a deployment script has
/// exactly the same command syntax and error reporting as an interactive one.
/// Silent when no start file exists on either disk -- a machine with no
/// start script should boot normally with no scary message.
fn run_start_file(info: &BootInfo) {
    match locate_start_file(info) {
        StartFileLookup::Found(device, length) => execute_start_file(info, device, length),
        StartFileLookup::Missing => {}
        StartFileLookup::Error(error) => {
            println!("start: could not inspect the file: {error:?}");
        }
    }
}

/// Interactive `start` command: reruns the exact same optional boot script
/// `run_start_file` runs automatically at boot, letting an operator restart
/// a `tape run`-driven script from the top -- e.g. after pressing F12 to
/// leave a long-running tape it started -- without a full machine reset.
/// Unlike the silent boot-time call, it reports plainly when no start file
/// exists at all, since an operator explicitly asking for one deserves to
/// know why nothing happened.
fn start_command(info: &BootInfo) {
    match locate_start_file(info) {
        StartFileLookup::Found(device, length) => execute_start_file(info, device, length),
        StartFileLookup::Missing => {
            println!("start: no start file found on disk1 or disk2.");
        }
        StartFileLookup::Error(error) => {
            println!("start: could not inspect the file: {error:?}");
        }
    }
}

/// Reads and runs the already-located `start` script from `device`.
fn execute_start_file(info: &BootInfo, device: disk::Device, length: u64) {
    run_script(info, device, START_FILE_NAME, "start", length, b"");
}

/// How many scripts may be running inside one another (the `start` file
/// counts as the first). Each level keeps a `START_FILE_CAPACITY`-byte copy of
/// its script on the kernel stack, and tapes later run nested inside the same
/// call chain, so this is deliberately small -- it also stops a script that
/// names itself from recursing until the stack guard page is hit.
const MAX_SCRIPT_DEPTH: usize = 3;
static mut SCRIPT_DEPTH: usize = 0;

/// Reads and runs one FOSFS script file (`start`, or a `NAME.sh` found by
/// `run_external_command`). Every non-empty, non-`#` line goes through the
/// ordinary `execute` dispatcher, so a script line may itself name another
/// script or tape. `label` prefixes every message so output says which
/// script is talking.
fn run_script(
    info: &BootInfo,
    device: disk::Device,
    file_name: &[u8],
    label: &str,
    length: u64,
    arguments: &[u8],
) {
    if length > START_FILE_CAPACITY as u64 {
        println!(
            "{label}: {length}-byte file is too large; the script limit is {START_FILE_CAPACITY} bytes."
        );
        return;
    }
    if unsafe { SCRIPT_DEPTH } >= MAX_SCRIPT_DEPTH {
        println!("{label}: scripts are nested too deeply (limit {MAX_SCRIPT_DEPTH}).");
        return;
    }

    let mut contents = [0u8; START_FILE_CAPACITY];
    let length = match fosfs::read(info, device, file_name, &mut contents[..length as usize]) {
        Ok(Some(length)) => length as usize,
        Ok(None) => return,
        Err(error) => {
            println!("{label}: could not read the file: {error:?}");
            return;
        }
    };
    println!("{label}: running {length}-byte script.");
    unsafe { SCRIPT_DEPTH += 1 };

    let mut line_start = 0;
    let mut line_number = 1;
    for index in 0..=length {
        if index != length && contents[index] != b'\n' {
            continue;
        }
        let mut line = &contents[line_start..index];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = trim_start(line);
        if !line.is_empty() && line[0] != b'#' {
            let mut expanded = [0u8; LINE_CAPACITY];
            match expand_script_arguments(line, arguments, &mut expanded) {
                Some(length) => {
                    print!("{label} line {line_number}: ");
                    console::write_bytes(&expanded[..length]);
                    println!();
                    execute(&expanded[..length], info);
                }
                None => println!(
                    "{label} line {line_number}: too long after expanding arguments; skipped."
                ),
            }
        }
        line_start = index + 1;
        line_number += 1;
    }
    unsafe { SCRIPT_DEPTH -= 1 };
}

/// Expands `$1`..`$9` (the script's words, split on spaces), `$*` (the whole
/// argument text) and `$$` (a literal `$`) in one script line into `out`,
/// returning the expanded length, or `None` if it would not fit. A missing
/// argument expands to nothing, and any other `$` is kept as typed. Expansion
/// is plain text substitution with no quoting, so an argument containing
/// spaces becomes several words -- the same trade the tape startup-argument
/// ABI makes. Arguments of a nested script are whatever its calling line
/// expanded to.
fn expand_script_arguments(line: &[u8], arguments: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut length = 0;
    let mut push = |bytes: &[u8]| -> Option<()> {
        out.get_mut(length..length + bytes.len())?.copy_from_slice(bytes);
        length += bytes.len();
        Some(())
    };
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        let next = line.get(index + 1).copied();
        match (byte, next) {
            (b'$', Some(b'$')) => {
                push(b"$")?;
                index += 2;
            }
            (b'$', Some(b'*')) => {
                push(arguments)?;
                index += 2;
            }
            (b'$', Some(digit @ b'1'..=b'9')) => {
                let mut rest = arguments;
                let mut word: &[u8] = b"";
                for _ in 0..(digit - b'0') {
                    (word, rest) = take_word(rest);
                }
                push(word)?;
                index += 2;
            }
            _ => {
                push(&[byte])?;
                index += 1;
            }
        }
    }
    Some(length)
}

/// Which kind of file a bare command word resolved to.
#[derive(Clone, Copy, PartialEq)]
enum ExternalKind {
    Script,
    Tape,
}

/// Longest bare command word that can still have `.sh`/`.bin` appended and
/// remain a legal FOSFS name (`fosfs::MAX_NAME_LENGTH` is 16).
const EXTERNAL_NAME_LIMIT: usize = 12;

/// Fallback for a command word that is not a builtin: look for `NAME.sh`
/// (a script) and then `NAME.bin` (a tape) on disk1, then each secondary disk
/// in order -- the same walk `load_hostname` uses, so a deployment can keep
/// its own scripts and tapes on whichever disk is handy. The first disk with
/// either file wins; on one disk `.sh` beats `.bin`, with a note, rather than
/// guessing silently. Builtins are always matched first by `execute`, so a
/// file can never shadow `reset` or `halt`. Disk errors (including a blank,
/// unformatted secondary disk) just mean "not here". Returns false when
/// nothing matched so the caller can print "Unknown command".
fn run_external_command(info: &BootInfo, name: &[u8], arguments: &[u8]) -> bool {
    if name.len() > EXTERNAL_NAME_LIMIT
        || !name
            .iter()
            .all(|byte| matches!(*byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        return false;
    }
    let mut script_name = [0u8; EXTERNAL_NAME_LIMIT + 3];
    let mut tape_name = [0u8; EXTERNAL_NAME_LIMIT + 4];
    script_name[..name.len()].copy_from_slice(name);
    script_name[name.len()..name.len() + 3].copy_from_slice(b".sh");
    tape_name[..name.len()].copy_from_slice(name);
    tape_name[name.len()..name.len() + 4].copy_from_slice(b".bin");
    let script_name = &script_name[..name.len() + 3];
    let tape_name = &tape_name[..name.len() + 4];

    let devices = core::iter::once(disk::Device::Boot)
        .chain((0..disk::secondary_count()).map(disk::Device::Secondary));
    for device in devices {
        let script = matches!(fosfs::file_length(info, device, script_name), Ok(Some(_)));
        let tape = matches!(fosfs::file_length(info, device, tape_name), Ok(Some(_)));
        let kind = match (script, tape) {
            (false, false) => continue,
            (true, true) => {
                println!(
                    "note: both {0}.sh and {0}.bin exist; running the script.",
                    core::str::from_utf8(name).unwrap_or("?")
                );
                ExternalKind::Script
            }
            (true, false) => ExternalKind::Script,
            (false, true) => ExternalKind::Tape,
        };
        match kind {
            ExternalKind::Script => match fosfs::file_length(info, device, script_name) {
                Ok(Some(length)) => run_script(
                    info,
                    device,
                    script_name,
                    core::str::from_utf8(script_name).unwrap_or("script"),
                    length,
                    arguments,
                ),
                _ => return false,
            },
            ExternalKind::Tape => {
                if tape_load_disk(info, device, tape_name, false) {
                    tape_run(info, arguments);
                }
            }
        }
        return true;
    }
    false
}

/// One command line's editing state: the buffer of characters typed so
/// far, how many of them are in use, and whether the previous byte was a
/// bare `\r` (so a `\r\n` pair from a terminal that sends both is treated
/// as a single Enter, not two). Factored out of `run`'s loop so
/// `term_net` can feed it bytes arriving from a remote network peer
/// through the exact same line-editing and command-dispatch logic the
/// interactive local prompt uses, instead of reimplementing it.
struct LineEditor {
    buffer: [u8; LINE_CAPACITY],
    length: usize,
    saw_carriage_return: bool,
}

impl LineEditor {
    fn new() -> Self {
        LineEditor {
            buffer: [0u8; LINE_CAPACITY],
            length: 0,
            saw_carriage_return: false,
        }
    }

    /// Feeds one input byte through the same line-editing rules the
    /// interactive shell prompt uses -- echoing printable characters,
    /// handling backspace/delete, and running `execute` once Enter is
    /// seen -- regardless of whether `byte` came from the local keyboard,
    /// COM1, or a remote network peer.
    fn feed(&mut self, byte: u8, info: &BootInfo) {
        match byte {
            b'\n' if self.saw_carriage_return => self.saw_carriage_return = false,
            b'\r' | b'\n' => {
                println!();
                execute(&self.buffer[..self.length], info);
                self.length = 0;
                self.saw_carriage_return = byte == b'\r';
                prompt();
            }
            8 | 127 if self.length > 0 => {
                self.saw_carriage_return = false;
                self.length -= 1;
                console::erase_last_character();
            }
            0x20..=0x7e if self.length < LINE_CAPACITY => {
                self.saw_carriage_return = false;
                self.buffer[self.length] = byte;
                self.length += 1;
                print!("{}", byte as char);
            }
            0x20..=0x7e => {
                self.saw_carriage_return = false;
                println!("\nInput limit is {LINE_CAPACITY} characters.")
            }
            _ => {}
        }
    }
}

fn prompt() {
    print!("fos> ");
}

#[inline(never)]
fn vga_command(arguments: &[u8]) {
    let (mode_word, remainder) = take_word(arguments);
    if mode_word.is_empty() {
        let mode = console::current_vga_mode();
        println!(
            "VGA mode: {} ({} columns x {} rows).",
            mode.name(),
            mode.width(),
            mode.height()
        );
        println!("Available modes: 80x25 (default), 80x50, 132x25 (experimental).");
        println!("Use 'vga d' to return to the default 80x25 mode.");
        return;
    }
    if !remainder.is_empty() {
        println!("Usage: vga [d|80x25|80x50|132x25]");
        return;
    }

    let mode = match mode_word {
        b"d" | b"80x25" => console::VgaMode::Text80x25,
        b"80x50" => console::VgaMode::Text80x50,
        b"132x25" => console::VgaMode::Experimental132x25,
        _ => {
            println!("Usage: vga [d|80x25|80x50|132x25]");
            return;
        }
    };
    console::set_vga_mode(mode);
    println!(
        "VGA mode changed to {} ({} columns x {} rows).",
        mode.name(),
        mode.width(),
        mode.height()
    );
    if mode == console::VgaMode::Experimental132x25 {
        println!("Warning: 132-column timing is experimental and may not work");
        println!("on every VGA adapter or monitor.");
    }
}

/// `execute` recurses (indirectly, via the `start`/boot-script path below)
/// when a script line itself dispatches another command: `start_command` ->
/// `execute_start_file` -> `execute` again for each line. Every command
/// handler this function's match arms call is therefore marked
/// `#[inline(never)]`. Without that, this release profile's aggressive
/// whole-program size optimization (`opt-level = "z"`, `lto = true`,
/// `codegen-units = 1`) happily inlines every handler -- `tape_run`'s own
/// frame, `nic_command`'s frame, `term_command`'s frame, and so on --
/// directly into `execute`'s own stack frame, even though only one match
/// arm ever runs per call. That makes one call to `execute` cost the *sum*
/// of every possible command's stack usage, not just the one actually
/// invoked. A single top-level call absorbs that fine, but two of them
/// stacked (typing `start` interactively, which is itself dispatched
/// through `execute`, and which then calls `execute` again for each script
/// line) doubles that inflated frame on the kernel's fixed 32 KiB stack --
/// confirmed, before this fix, to double-fault running a script that
/// merely `tape run`s a large tape (e.g. `router`) via the interactive
/// `start` command specifically, while the exact same commands typed
/// manually, or run automatically at boot (where `execute` is only ever
/// one level deep), worked every time. `#[inline(never)]` keeps `execute`
/// itself a small dispatcher and lets each handler's own frame exist only
/// when that handler is actually on the call stack, exactly as a plain
/// `match` over function calls should behave.
///
/// This alone was not sufficient, though: even with `execute` shrunk to a
/// thin dispatcher, the second, nested `execute` -> `tape_command` ->
/// `tape_run` chain the interactive `start` path adds is a genuine,
/// unavoidable extra layer of stack depth on top of whatever the running
/// tape itself uses -- and a heavy tape like `router` (multiple NICs,
/// DHCP, health checks, NAT) still exhausted the kernel's original 32 KiB
/// stack at that combined depth. `kernel/src/entry.S`'s `kernel_stack` was
/// doubled to 64 KiB to give this path real headroom; see its comment for
/// why that budget is safe against `linker.ld`'s low-memory-region limit.
fn execute(command: &[u8], info: &BootInfo) {
    let (name, arguments) = take_word(command);
    match name {
        b"" => {}
        b"help" => {
            // Every line here is kept to at most 79 columns (VGA text mode
            // wraps a line at exactly 80, and a println! whose text lands
            // precisely on column 80 wraps *and* leaves a spurious blank
            // row for its own trailing '\n' -- so 79 is the true safe
            // maximum, not 80) and the whole listing to 15 lines, so it
            // always fits a standard 80x25 screen without wrapping or
            // scrolling any of itself out of view. Prefer trimming detail
            // (full syntax remains in README.md/AGENTS.md) over adding a
            // line or letting one run long if this command ever grows.
            //
            // Disabled while connected via `term_printer`: its own output
            // would otherwise be captured and printed like any other
            // command's, and this listing is long enough that running it
            // by habit would waste real paper every time. `print run
            // help` is a deliberate, always-available exception (it
            // temporarily clears this flag around its own nested
            // `execute` call -- see `print_run`) for getting exactly one
            // paper copy, including from inside an active session.
            if unsafe { PRINTER_SESSION_ACTIVE } {
                println!("help is disabled while connected to a printer terminal (it would");
                println!("print this whole listing). Run 'print run help' once instead, or");
                println!("check the manual.");
                return;
            }
            if !arguments.is_empty() {
                help_topic(arguments);
            } else {
                println!(
                    "System: help clear vga about bootinfo memmap memory cpuinfo pci usb"
                );
                println!("      hexdump dmesg halt reset reboot [soft|hard]");
                println!("Interrupts: ticks uptime irqstat pmem palloc pfree");
                println!("Hardware: io|mmio r8|r16|r32 ADDR, w8|w16|w32 ADDR VAL; pci read|write B:D.F");
                println!(
                    "Disk: diskinfo/readsector/format/ls/cat/write/rm [diskN], copy SRC DST"
                );
                println!(
                    "      (disk1 is the boot disk, N may be omitted; disk2, disk3... are extras;"
                );
                println!("      diskN NAME may also be written diskN/NAME)");
                println!(
                    "      start (rerun the boot 'start' script, e.g. after F12 from a tape)"
                );
                println!("Files: NAME [ARGS] runs NAME.sh (script) or NAME.bin (tape) from any disk");
                println!(
                    "Serial: serial (list ports), term [PORT [BAUD]] (raw terminal, F12 to exit)"
                );
                println!(
                    "Ethernet: nic [list|use N|send|recv|sniff|discover|peers|map|watchdog|regs]"
                );
                println!(
                    "      chat [PEER_MAC|N]; copy/tape load|save take HOST[:diskN] peers"
                );
                println!("Audio/USB: audio, hda (status); usb play|capture [diskN] NAME");
                println!("      usb storage attach|promote, usb native (promote/attach all disks)");
                println!(
                    "Tape: tape status | tape run [ARGS...|@FILE|@diskN/FILE] ('load'/'run' work"
                );
                println!(
                    "      without 'tape' too) | tape load disk[N] NAME | tape load net"
                );
                println!("      tape save [diskN] NAME | tape send NAME PEER");
                println!(
                    "Remote: term net (wait for a remote shell over Ethernet; same as at boot)"
                );
                println!(
                    "Printer: printer (list LPTs), term printer|LPTn (raw, F12 exits, F2 kills"
                );
                println!(
                    "      the current line), print [diskN] NAME | print run CMD..."
                );
                println!(
                    "Note: F12 (or F4/F6/F7/F8/F10) gives up on any blocking wait (sniff/term/chat)."
                );
                println!("Details: help TOPIC (nic tape usb disk hw term print audio)");
            }
        }
        b"clear" if no_arguments(arguments) => console::clear(),
        b"vga" => vga_command(arguments),
        b"about" if no_arguments(arguments) => {
            println!("fos is a small educational 32-bit x86 operating system.")
        }
        b"bootinfo" if no_arguments(arguments) => print_boot_info(info),
        b"memmap" if no_arguments(arguments) => print_memory_map(info),
        b"memory" if no_arguments(arguments) => print_memory_summary(info),
        b"cpuinfo" if no_arguments(arguments) => print_cpu_info(),
        b"diskinfo" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                print_disk_info(info, device);
            }
        }
        b"ticks" if no_arguments(arguments) => print_ticks(),
        b"uptime" if no_arguments(arguments) => print_uptime(),
        b"irqstat" if no_arguments(arguments) => print_irq_stats(),
        b"pmem" if no_arguments(arguments) => print_pmem_stats(),
        b"palloc" if no_arguments(arguments) => palloc_command(),
        b"pfree" => pfree_command(arguments),
        b"readsector" => read_sector_command(info, arguments),
        b"hexdump" => hexdump_command(info, arguments),
        b"format" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                filesystem_result(fosfs::format(info, device), "FOSFS formatted.")
            }
        }
        b"ls" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                filesystem_result(fosfs::list(info, device), "")
            }
        }
        b"cat" => cat_command(info, arguments),
        b"write" => write_command(info, arguments),
        b"rm" => remove_command(info, arguments),
        b"copy" => copy_command(info, arguments),
        b"start" if no_arguments(arguments) => start_command(info),
        b"term" => term_command(info, arguments),
        b"serial" if no_arguments(arguments) => print_serial_ports(),
        // `arguments.is_empty()`, not `no_arguments`: that helper prints a complaint
        // when arguments are present, which would fire before `pci read|write`.
        b"pci" if arguments.is_empty() => print_pci_devices(),
        b"pci" => pci_command(arguments),
        b"io" => io_command(arguments),
        b"mmio" => mmio_command(arguments),
        b"dmesg" if no_arguments(arguments) => dmesg_command(),
        b"usb" => usb_command(info, arguments),
        b"printer" if no_arguments(arguments) => print_printer_ports(),
        b"print" => print_command(info, arguments),
        b"nic" => nic_command(arguments),
        b"audio" if no_arguments(arguments) => audio_status(),
        b"hda" if no_arguments(arguments) => hda_status(),
        b"chat" => chat_command(arguments),
        b"tape" => tape_command(info, arguments),
        // `load`/`run` are accepted bare, without the `tape` prefix, as a
        // shorthand for `tape load`/`tape run` -- the two tape subcommands
        // used often enough (loading and running a tape) that spelling out
        // `tape` every time was pure friction. The other subcommands
        // (`status`/`save`/`send`) keep requiring the `tape` prefix, since
        // they are used far less often and `save`/`send` in particular
        // would be easy to mistake for unrelated top-level commands.
        b"load" => tape_load_command(info, arguments),
        b"run" => tape_run(info, arguments),
        b"halt" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Halting CPU. Reset or power-cycle to restart.");
            crate::debug::note_clean_shutdown(info);
            halt_forever();
        }
        b"reset" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Resetting the machine now (8042 keyboard-controller pulse reset)...");
            crate::debug::note_clean_shutdown(info);
            reset();
        }
        b"reboot" => reboot_command(info, arguments),
        b"debug" => debug_command(info, arguments),
        b"clear" | b"about" | b"bootinfo" | b"memmap" | b"memory" | b"cpuinfo" | b"ticks"
        | b"uptime" | b"irqstat" | b"pmem" | b"palloc" | b"serial" | b"dmesg"
        | b"printer" | b"start" | b"audio" | b"hda" => {
            println!("This command does not accept arguments.");
        }

        _ => {
            if !run_external_command(info, name, arguments) {
                println!("Unknown command. Type 'help' for commands.");
            }
        }
    }
}

/// `help TOPIC`: a per-command page for the commands whose syntax does not
/// fit the one-screen overview. Each page stays within 79 columns and well
/// under 25 rows for the same reason the overview does (see `help`).
fn help_topic(topic: &[u8]) {
    match topic {
        b"nic" => {
            println!("nic                 status of the selected NIC");
            println!("nic list            every NIC: link, MAC, health, IRQ/MSI, hostname");
            println!("nic use N           select NIC N (1-based) for send/recv/sniff/tapes");
            println!("nic send | recv     send one test frame / wait for one (F12 gives up)");
            println!("nic sniff           print every frame heard, promiscuous (F12 ends)");
            println!("nic discover        probe for fos peers and number them (F12 ends)");
            println!("nic peers           print the numbered peer table (see chat, tape send)");
            println!("nic map             per-NIC link, and which NICs hear which (cabling)");
            println!("nic watchdog        run the NIC fault watchdog, show recovery counters");
            println!("nic regs [N]        dump registers (RTL8168, 3C905)");
            println!("chat [PEER_MAC|N]   text chat with a peer, /quit leaves");
        }
        b"tape" => {
            println!("tape status                 loaded tape: length, format, entry");
            println!("tape load disk[N] NAME      load NAME from a disk into the tape buffer");
            println!("tape load net               wait for a tape sent over Ethernet");
            println!("tape load HOST[:diskN] NAME fetch from a peer (NETFS)");
            println!("tape run [ARGS|@FILE]       run it; @FILE passes a file as the arguments");
            println!("tape save [diskN] NAME      write the loaded tape to a disk file");
            println!("tape save HOST[:diskN] NAME push it to a peer");
            println!("tape send NAME PEER         send a disk file to a peer as a tape");
            println!("'load' and 'run' work without the 'tape' prefix. NAME alone runs");
            println!("NAME.sh or NAME.bin from any disk.");
        }
        b"usb" => {
            println!("usb                         list UHCI controllers/ports, enumerate");
            println!("usb play [diskN] NAME       play a PCM file on a USB audio device");
            println!("usb capture [diskN] NAME    record from a USB audio device");
            println!("usb storage                 inspect the first USB mass-storage device");
            println!("usb storage attach          add it as the next diskN");
            println!("usb storage promote         switch disk1 to the native USB driver");
            println!("usb native                  promote/attach every USB disk at once");
            println!("Resets the controllers: can break BIOS USB-boot disk access until a");
            println!("hardware reset ('reset' or 'reboot hard').");
        }
        b"disk" => {
            println!("diskinfo [diskN]            geometry and FOSFS partition");
            println!("readsector LBA              hex-dump one raw sector");
            println!("format [diskN]              create an empty FOSFS (destroys files)");
            println!("ls [diskN]                  list files");
            println!("cat [diskN] NAME            print a file");
            println!("write [diskN] NAME TEXT     create a file from printable ASCII");
            println!("rm [diskN] NAME             delete a file");
            println!("copy SRC DST [NEW]          copy a file: disk1/NAME disk2[/NEW]; either");
            println!("                            side may be a peer HOST[:diskN]/NAME");
            println!("                            (older: copy SRC_DISK NAME DST [NEW])");
            println!("disk1 is the boot disk; names are lowercase, up to 16 characters.");
        }
        b"hw" => {
            println!("pci                         list every PCI function");
            println!("pci read B:D.F [OFF [LEN]]  read config space");
            println!("pci write B:D.F OFF VALUE   write config space");
            println!("io r8|r16|r32 PORT          read an I/O port");
            println!("io w8|w16|w32 PORT VALUE    write an I/O port");
            println!("mmio r8|r16|r32 ADDR        read mapped memory (unmapped is refused)");
            println!("mmio w8|w16|w32 ADDR VALUE  write mapped memory");
            println!("hexdump ADDR LEN            dump physical memory");
            println!("irqstat                     PIC pending/in-service/mask, IRQ counts");
        }
        b"term" => {
            println!("term [PORT [BAUD]]          raw terminal on a serial port (F12 exits)");
            println!("term net                    wait for a remote shell over Ethernet");
            println!("term printer | LPTn         raw terminal to a parallel printer");
            println!("serial                      list serial ports");
        }
        b"print" => {
            println!("printer                     list parallel ports");
            println!("print [diskN] NAME          print a file");
            println!("print run CMD...            print another command's output");
        }
        b"audio" => {
            println!("audio                       status of the active audio device");
            println!("hda                         Intel HDA status (interrupt mode, codecs)");
            println!("usb play|capture            USB audio (see help usb)");
        }
        _ => println!("No help for that. Topics: nic tape usb disk hw term print audio"),
    }
}

/// `reboot` (bare, or `reboot soft`) warm-restarts the machine through
/// `crate::reboot::soft_reboot` -- the "reset the OS, not the machine"
/// command this project's design discussion settled on: it re-runs
/// `boot/stage2.S`'s whole boot sequence (E820, A20, `select_kernel_slot`'s
/// A/B fallback, and a fresh kernel load) without a hardware reset, so it
/// picks up a pending KUP update exactly like a real reboot would. `reboot
/// hard` instead performs the existing, unconditional 8042-pulse hardware
/// reset -- kept as an explicit, always-available fallback for the case a
/// soft reboot itself needs to be ruled out (e.g. it hangs, or something
/// about the running kernel's own state is suspect). `reset` remains a
/// permanent alias for `reboot hard`, predating this command and already
/// relied on by existing scripts/tests.
fn reboot_command(info: &BootInfo, arguments: &[u8]) {
    match arguments {
        b"" | b"soft" => {
            println!("Soft-rebooting: re-entering the boot loader without a hardware reset...");
            crate::debug::note_clean_shutdown(info);
            crate::reboot::soft_reboot(info);
        }
        b"hard" => {
            println!("Resetting the machine now (8042 keyboard-controller pulse reset)...");
            crate::debug::note_clean_shutdown(info);
            reset();
        }
        _ => println!("Usage: reboot [soft|hard]"),
    }
}

/// `debug status` prints the currently loaded [`crate::debug::DebugConfig`]
/// and A/B slot metadata; `debug crash` deliberately raises a real CPU
/// exception (`int3`, vector 3, no error code) so an operator can verify
/// `reboot-on-crash` actually reboots (or correctly halts, if disabled)
/// *before* trusting it unattended on real hardware -- exactly the kind of
/// thing that's much cheaper to check on purpose than to wait for a real
/// crash to happen. Undocumented in `help`'s own listing (it is a
/// diagnostic tool for this feature specifically, not routine command
/// surface), but always available, matching the `debug` file's own
/// "off unless configured" default -- `debug crash` still raises a real
/// exception even with no `debug` file present, it just then halts instead
/// of rebooting, exactly as any other unconfigured crash would.
fn debug_command(info: &BootInfo, arguments: &[u8]) {
    let (subcommand, rest) = take_word(arguments);
    match subcommand {
        b"status" if no_arguments(rest) => {
            println!(
                "debug: reboot-on-crash={}, reboot-delay={}s, kernel-update={}.",
                crate::debug::reboot_on_crash(),
                crate::debug::reboot_delay_ticks() / 100,
                crate::debug::kernel_update_enabled()
            );
            println!(
                "debug: watchdog={}s ({}), unclean boots in a row: {}.",
                crate::debug::watchdog_seconds(),
                if crate::debug::watchdog_armed() { "armed" } else { "off" },
                crate::debug::unclean_boots()
            );
        }
        b"hang" if no_arguments(rest) => {
            println!("debug: deliberately spinning with interrupts on; only the watchdog can end this.");
            loop {
                core::hint::spin_loop();
            }
        }
        b"crash" if no_arguments(rest) => {
            println!("debug: deliberately raising a test exception (int3) now...");
            unsafe {
                core::arch::asm!("int3", options(nomem, nostack));
            }
        }
        b"slots" if no_arguments(rest) => {
            print_slot_metadata(info);
        }
        _ => println!("Usage: debug status | debug crash | debug hang | debug slots"),
    }
}

/// Prints the current A/B kernel-slot metadata ([`crate::debug::SlotMetadata`])
/// -- the currently booted slot, the active/pending slots, the fallback
/// state machine's own `attempted`/`confirmed` flags, and the on-disk
/// crash-loop counter. Undocumented in `help`, same as the rest of `debug`;
/// useful for verifying a network kernel update (or the fallback path)
/// actually did what it claimed, both by a real operator and by this
/// feature's own end-to-end test.
fn print_slot_metadata(info: &BootInfo) {
    let Some(metadata) = crate::debug::SlotMetadata::read(info) else {
        println!("debug: no A/B slot metadata found on disk1.");
        return;
    };
    match metadata.pending_slot {
        Some(pending) => println!(
            "debug: booted slot {}; active={}, pending={}, attempted={},",
            info.booted_slot, metadata.active_slot, pending, metadata.attempted,
        ),
        None => println!(
            "debug: booted slot {}; active={}, pending=none, attempted={},",
            info.booted_slot, metadata.active_slot, metadata.attempted,
        ),
    }
    println!(
        "confirmed={}, crash_count={}, slot0_sectors={}, slot1_sectors={}.",
        metadata.confirmed, metadata.crash_count, metadata.slot_sectors[0], metadata.slot_sectors[1]
    );
}

fn print_ticks() {
    println!(
        "Timer ticks: {} (at {} Hz)",
        interrupts::ticks(),
        interrupts::tick_frequency_hz()
    );
}

fn print_uptime() {
    let ticks = interrupts::ticks();
    let frequency = u64::from(interrupts::tick_frequency_hz());
    println!(
        "Uptime: {}.{:02} seconds ({} ticks)",
        ticks / frequency,
        (ticks % frequency) * 100 / frequency,
        ticks
    );
}

fn print_irq_stats() {
    let (queued_scancodes, dropped_scancodes) = interrupts::scancode_queue_stats();
    println!(
        "IRQ0 (timer): {} fired; IRQ1 (keyboard): {} fired ({} queued,",
        interrupts::timer_irq_count(),
        interrupts::keyboard_irq_count(),
        queued_scancodes,
    );
    println!(
        "{} dropped); spurious IRQ7/IRQ15: {} fired",
        dropped_scancodes,
        interrupts::spurious_irq_count()
    );
    let keyboard_restore_failures = keyboard::bios_restore_failures();
    if keyboard_restore_failures != 0 {
        println!("PS/2 Scan Code Set 1 restoration after BIOS disk calls failed");
        println!("{keyboard_restore_failures} time(s).");
    }
    let (irr, isr, imr) = interrupts::pic_registers();
    println!("PIC pending {irr:04x} in-service {isr:04x} mask {imr:04x}");
    println!("(bit N = IRQ N; a pending bit that is also masked has no handler).");
    for vector in interrupts::MSI_FIRST_VECTOR..interrupts::MSI_FIRST_VECTOR + interrupts::MSI_VECTORS {
        if interrupts::msi_irq_registered(vector) {
            println!(
                "MSI vector 0x{vector:02x}: {} message(s) received.",
                interrupts::msi_irq_count(vector)
            );
        }
    }
    for irq in 3..16 {
        if interrupts::pci_irq_registered(irq) {
            println!(
                "PCI IRQ{irq}: {} dispatch(es) (device handlers only acknowledge",
                interrupts::pci_irq_count(irq)
            );
            println!("and defer receive work).");
        }
        if interrupts::pci_irq_storm_masked(irq) {
            println!("IRQ{irq} MASKED by the storm guard (over 500 in one tick).");
        }
    }
}

fn print_pmem_stats() {
    let (total, free) = pmem::stats();
    let used = total - free;
    println!(
        "Physical frames: {total} total, {free} free, {used} used ({} KiB/frame)",
        pmem::FRAME_SIZE / 1024
    );
}

fn palloc_command() {
    match pmem::allocate() {
        Some(address) => println!("Allocated frame at 0x{address:08x}."),
        None => println!("No free physical frames remain."),
    }
}

fn pfree_command(arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(address) = parse_number(value) else {
        println!("Usage: pfree ADDRESS (as returned by palloc)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: pfree ADDRESS");
        return;
    }
    pmem::free(address);
    println!("Frame at 0x{address:08x} released (if it was allocated and in range).");
}

fn print_cpu_info() {
    let vendor = cpu::vendor();
    print!("CPUID vendor: ");
    console::write_bytes(&vendor);
    let (edx, ecx) = cpu::features();
    println!("\nCPUID feature bits: EDX=0x{edx:08x} ECX=0x{ecx:08x}");
}

/// Display names for the four legacy COM ports, in BIOS Data Area index
/// order; shared by `serial` (listing) and `term` (port selection).
const SERIAL_PORT_NAMES: [&[u8]; 4] = [b"COM1", b"COM2", b"COM3", b"COM4"];

/// Display names for PCI-discovered serial ports (see `console::detect_pci_ports`
/// / `pci::find_serial_ports`), in discovery order.
const PCI_PORT_NAMES: [&[u8]; console::MAX_PCI_SERIAL_PORTS] = [b"PCI0", b"PCI1", b"PCI2", b"PCI3"];

/// Display names for the four legacy LPT ports, in BIOS Data Area index
/// order; shared by `printer` (listing) and `term` (port selection). ISA
/// only -- see `parallel`'s module doc comment for why there is no PCI
/// parallel-card discovery to mirror `PCI_PORT_NAMES` above.
const PRINTER_PORT_NAMES: [&[u8]; 4] = [b"LPT1", b"LPT2", b"LPT3", b"LPT4"];

fn print_serial_ports() {
    let ports = console::detect_ports();
    for (index, name) in SERIAL_PORT_NAMES.iter().enumerate() {
        let name = core::str::from_utf8(name).unwrap_or("?");
        match ports[index] {
            Some(base) if index == 0 => {
                println!("{name}: 0x{base:04x} (local shell console, fixed at 115200 8N1)");
            }
            Some(base) => {
                println!("{name}: 0x{base:04x} (free for 'term'; baud chosen when entering it)")
            }
            None => println!("{name}: not detected"),
        }
    }
    let (pci_ports, pci_count) = console::detect_pci_ports();
    if pci_count == 0 {
        println!("PCI: no additional serial controllers found on the PCI bus.");
    } else {
        for index in 0..pci_count {
            let name = core::str::from_utf8(PCI_PORT_NAMES[index]).unwrap_or("?");
            let base = pci_ports[index];
            println!(
                "{name}: 0x{base:04x} (found via PCI enumeration; free for 'term';"
            );
            println!("baud chosen when entering it)");
        }
    }
}

/// Dumps every PCI function found on the bus: location, vendor/device ID,
/// class/subclass/prog-if, header type, and BAR0's raw value. This exists
/// so an unmarked or newly acquired card's real vendor/device ID (needed to
/// add it to a driver's `pci::find_devices` match list, e.g. `rtl8139.rs`/
/// `e1000.rs`) can be read directly from fos itself, without needing a
/// second machine or `lspci`. Unlike most commands, this one's output is
/// allowed to scroll past one screen -- like `ls`/`memmap`, the number of
/// devices on a real bus is inherently unbounded, not a fixed-size listing
/// that could easily have been trimmed to fit.
fn print_pci_devices() {
    let mut devices = [pci::PciDeviceInfo {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0,
        device_id: 0,
        class: 0,
        subclass: 0,
        prog_if: 0,
        header_type: 0,
        bar0: 0,
        has_msi: false,
        revision: 0,
        subsystem: 0,
        interrupt_line: 0,
        interrupt_pin: 0,
        command: 0,
        status: 0,
    }; pci::MAX_PCI_DEVICES];
    let found = pci::scan_devices(&mut devices);
    let shown = found.min(devices.len());
    if shown == 0 {
        println!("No PCI devices found.");
        return;
    }
    println!("Found {found} PCI function(s):");
    for info in &devices[..shown] {
        println!(
            "{:02x}:{:02x}.{} {:04x}:{:04x} class={:02x}/{:02x} prog_if={:02x}",
            info.bus, info.device, info.function, info.vendor_id, info.device_id, info.class,
            info.subclass, info.prog_if,
        );
        println!(
            "  hdr={:02x} bar0=0x{:08x} msi={} rev={:02x} sub={:04x}:{:04x} int={:02x}/{} cmd={:04x} st={:04x}",
            info.header_type,
            info.bar0,
            if info.has_msi { "yes" } else { "no" },
            info.revision,
            info.subsystem & 0xffff,
            info.subsystem >> 16,
            info.interrupt_line,
            info.interrupt_pin,
            info.command,
            info.status,
        );
    }
    if found > shown {
        println!("({} more found but not shown; MAX_PCI_DEVICES exceeded)", found - shown);
    }
}

/// Parses `BB:DD.F` (hexadecimal bus, device, function), as the `pci` dump
/// prints them.
fn parse_pci_address(text: &[u8]) -> Option<(u8, u8, u8)> {
    let colon = text.iter().position(|&byte| byte == b':')?;
    let dot = text.iter().position(|&byte| byte == b'.')?;
    if dot < colon {
        return None;
    }
    let hex = |digits: &[u8]| -> Option<u32> {
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        digits.iter().try_fold(0u32, |value, &digit| {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return None,
            };
            Some(value * 16 + u32::from(nibble))
        })
    };
    let bus = hex(&text[..colon])?;
    let device = hex(&text[colon + 1..dot])?;
    let function = hex(&text[dot + 1..])?;
    (device < 32 && function < 8).then_some((bus as u8, device as u8, function as u8))
}

/// `pci read B:D.F [OFFSET [LENGTH]]` dumps a function's configuration space
/// (default the first 64 bytes, at most 256); `pci write B:D.F OFFSET VALUE`
/// writes one dword. Raw debugging aids: find where an interrupt is routed, read
/// a capability, flip the MSI enable bit -- without a diagnostic kernel push.
/// Offsets are rounded down to a dword.
fn pci_command(arguments: &[u8]) {
    let (subcommand, rest) = take_word(arguments);
    let (address_word, rest) = take_word(rest);
    let Some((bus, device, function)) = parse_pci_address(address_word) else {
        println!("Usage: pci | pci read B:D.F [OFFSET [LENGTH]] | pci write B:D.F OFFSET VALUE");
        println!("(B:D.F in hex as the pci listing shows it, e.g. 03:00.0)");
        return;
    };
    match subcommand {
        b"read" => {
            let (offset_word, rest) = take_word(rest);
            let (length_word, rest) = take_word(rest);
            let offset = if offset_word.is_empty() { Some(0) } else { parse_number(offset_word) };
            let length = if length_word.is_empty() { Some(64) } else { parse_number(length_word) };
            let (Some(offset), Some(length)) = (offset, length) else {
                println!("pci read: OFFSET and LENGTH must be numbers.");
                return;
            };
            if !rest.is_empty() || offset > 0xff {
                println!("Usage: pci read B:D.F [OFFSET [LENGTH]] (offset 0-255)");
                return;
            }
            let start = offset & !3;
            let end = (offset + length.clamp(1, 256)).min(256);
            let mut row = start & !15;
            while row < end {
                print!("{row:02x}:");
                for dword in 0..4u32 {
                    let at = row + dword * 4;
                    if at >= start && at < end {
                        let value = pci::config_read32(bus, device, function, at as u8);
                        print!(" {:02x} {:02x} {:02x} {:02x}", value & 0xff, (value >> 8) & 0xff, (value >> 16) & 0xff, value >> 24);
                    } else {
                        print!("            ");
                    }
                }
                println!();
                row += 16;
            }
        }
        b"write" => {
            let (offset_word, rest) = take_word(rest);
            let (value_word, rest) = take_word(rest);
            let (Some(offset), Some(value)) = (parse_number(offset_word), parse_number(value_word)) else {
                println!("Usage: pci write B:D.F OFFSET VALUE (32-bit write)");
                return;
            };
            if !rest.is_empty() || offset > 0xfc {
                println!("Usage: pci write B:D.F OFFSET VALUE (offset 0-252)");
                return;
            }
            let offset = offset & !3;
            pci::config_write32(bus, device, function, offset as u8, value);
            println!(
                "{bus:02x}:{device:02x}.{function} [{offset:02x}] <- {value:08x}; reads back {:08x}.",
                pci::config_read32(bus, device, function, offset as u8)
            );
        }
        _ => println!("Usage: pci | pci read B:D.F [OFFSET [LENGTH]] | pci write B:D.F OFFSET VALUE"),
    }
}

/// Splits `r8`/`w16`/... into (is_write, width_in_bytes).
fn parse_access(word: &[u8]) -> Option<(bool, u32)> {
    let write = match word.first()? {
        b'r' => false,
        b'w' => true,
        _ => return None,
    };
    match &word[1..] {
        b"8" => Some((write, 1)),
        b"16" => Some((write, 2)),
        b"32" => Some((write, 4)),
        _ => None,
    }
}

/// `io r8|r16|r32 PORT` / `io w8|w16|w32 PORT VALUE`: raw x86 port I/O, for
/// reading a card's registers from its I/O BAR (the `pci` dump shows the base)
/// without a diagnostic kernel. A write to the wrong port can hang or reset the
/// machine; this is a debugging tool.
fn io_command(arguments: &[u8]) {
    let (access, rest) = take_word(arguments);
    let (port_word, rest) = take_word(rest);
    let (value_word, rest) = take_word(rest);
    let usage = "Usage: io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE";
    let (Some((write, width)), Some(port)) = (parse_access(access), parse_number(port_word)) else {
        println!("{usage}");
        return;
    };
    if port > 0xffff || !rest.is_empty() || write != !value_word.is_empty() {
        println!("{usage}");
        return;
    }
    let port = port as u16;
    if write {
        let Some(value) = parse_number(value_word) else {
            println!("{usage}");
            return;
        };
        unsafe {
            match width {
                1 => crate::arch::x86::outb(port, value as u8),
                2 => crate::arch::x86::outw(port, value as u16),
                _ => crate::arch::x86::outl(port, value),
            }
        }
        println!("port 0x{port:04x} <- 0x{value:x}");
    } else {
        let value = unsafe {
            match width {
                1 => u32::from(crate::arch::x86::inb(port)),
                2 => u32::from(crate::arch::x86::inw(port)),
                _ => crate::arch::x86::inl(port),
            }
        };
        println!("port 0x{port:04x} = 0x{value:0w$x}", w = width as usize * 2);
    }
}

/// `mmio r8|r16|r32 ADDR` / `mmio w8|w16|w32 ADDR VALUE`: a volatile access to
/// physical memory, for device registers in a memory BAR or the local APIC.
/// Only addresses the kernel has mapped are accepted (the low 2 GiB, minus the
/// stack guard page, plus MMIO a driver registered); anything else would page-
/// fault, so it is refused with a message instead. Writes also refuse kernel
/// text and read-only data. Accesses must be naturally aligned.
fn mmio_command(arguments: &[u8]) {
    let (access, rest) = take_word(arguments);
    let (address_word, rest) = take_word(rest);
    let (value_word, rest) = take_word(rest);
    let usage = "Usage: mmio r8|r16|r32 ADDR | mmio w8|w16|w32 ADDR VALUE";
    let (Some((write, width)), Some(address)) = (parse_access(access), parse_number(address_word)) else {
        println!("{usage}");
        return;
    };
    if !rest.is_empty() || write != !value_word.is_empty() {
        println!("{usage}");
        return;
    }
    if address % width != 0 {
        println!("mmio: 0x{address:x} is not {width}-byte aligned.");
        return;
    }
    let allowed = if write { crate::paging::is_writable(address) } else { crate::paging::is_readable(address) };
    if !allowed {
        println!(
            "mmio: 0x{address:x} is not accessible ({}); only mapped addresses can be {}.",
            crate::paging::describe_address(address),
            if write { "written" } else { "read" }
        );
        return;
    }
    if write {
        let Some(value) = parse_number(value_word) else {
            println!("{usage}");
            return;
        };
        unsafe {
            match width {
                1 => core::ptr::write_volatile(address as *mut u8, value as u8),
                2 => core::ptr::write_volatile(address as *mut u16, value as u16),
                _ => core::ptr::write_volatile(address as *mut u32, value),
            }
        }
        println!("[0x{address:08x}] <- 0x{value:x}");
    } else {
        let value = unsafe {
            match width {
                1 => u32::from(core::ptr::read_volatile(address as *const u8)),
                2 => u32::from(core::ptr::read_volatile(address as *const u16)),
                _ => core::ptr::read_volatile(address as *const u32),
            }
        };
        println!("[0x{address:08x}] = 0x{value:0w$x}", w = width as usize * 2);
    }
}

/// `nic regs [N]`: the selected (or Nth, 1-based) NIC's register and ring dump.
fn nic_regs(arguments: &[u8]) {
    let (word, rest) = take_word(arguments);
    let nic = if word.is_empty() {
        nic::get().ok()
    } else if let (Some(number), true) = (parse_number(word), rest.is_empty()) {
        number.checked_sub(1).and_then(|index| nic::get_at(index as usize))
    } else {
        None
    };
    match nic {
        Some(nic) => nic.dump_registers(),
        None => println!("Usage: nic regs [N] (N from 'nic list', numbering starts at 1)"),
    }
}

/// Replays `console`'s dmesg ring buffer: every byte any `println!`/
/// `print!` in this kernel has sent out since boot, seeded with whatever
/// `stage1.S`/`stage2.S` real-mode narration was still on screen when
/// `console::initialize` first ran (see `console::seed_klog_from_vga`).
/// Reports how many older bytes have already been overwritten first, so
/// an operator does not mistake a wrapped ring for the complete bootup
/// process -- on this kernel's own tight low-memory budget (see AGENTS.md's
/// "Low-memory map") the ring is only a few KiB, so a busy session can
/// evict even the boot narration itself; running `dmesg` early is the best
/// chance to still see it. Unlike most commands, this one's output is
/// allowed to scroll past one screen, exactly like `pci`/`ls`/`memmap`.
fn dmesg_command() {
    let dropped = console::dmesg_dropped_bytes();
    if dropped == 0 {
        println!("dmesg: {} byte(s) of kernel log since boot, nothing lost:", console::dmesg_len());
    } else {
        println!(
            "dmesg: {} byte(s) of kernel log ({dropped} older byte(s) already overwritten):",
            console::dmesg_len()
        );
    }
    console::print_dmesg();
}

/// `usb` reports every discovered UHCI (USB 1.1) host controller and, for
/// each of its two root ports, drives a fresh port reset and full
/// enumeration (address assignment, device/configuration descriptor
/// reads, `SET_CONFIGURATION`) every time this command runs -- the same
/// "read directly from hardware, print it" ethos as `pci`, deliberately
/// re-run from a known state each call rather than cached, since a real
/// device can be unplugged/replugged between commands. This command's
/// own job stops at enumeration -- isochronous transfers and USB Audio
/// Class descriptor parsing (see `usb play`/`usb capture` below, and
/// AGENTS.md's "UHCI" section) are deliberately not involved here at all.
fn print_usb_devices() {
    let count = uhci::count();
    if count == 0 {
        println!("No UHCI controllers found.");
        return;
    }
    println!("Found {count} UHCI controller(s):");
    for index in 0..count {
        let Some(controller) = uhci::get(index) else {
            continue;
        };
        print!("Controller {}: io_base=0x{:04x}", controller.instance(), controller.io_base());
        match controller.irq() {
            Some(irq) => print!(" irq={irq}"),
            None => print!(" irq=none"),
        }
        println!(" running={}", if controller.is_running() { "yes" } else { "no" });
        for port in 0..uhci::UHCI_PORTS {
            enumerate_usb_port(port + 1, controller, port);
        }
    }
}

/// Resets one root port and, if a full-speed device answers, drives the
/// standard enumeration sequence far enough to read its real device and
/// configuration descriptors: `GET_DESCRIPTOR` (8 bytes, to learn
/// `bMaxPacketSize0`) -> `SET_ADDRESS` -> `GET_DESCRIPTOR` (full device
/// descriptor) -> `GET_DESCRIPTOR` (configuration, header then full) ->
/// `SET_CONFIGURATION`. `port_number` is the 1-based number printed to the
/// operator; `port` is the 0-based index `Uhci`'s own API uses.
fn enumerate_usb_port(port_number: u8, controller: &uhci::Uhci, port: u8) {
    let low_speed = match controller.reset_port(port) {
        Ok(low_speed) => low_speed,
        Err(uhci::Error::NotPresent) => {
            println!("  port {port_number}: no device connected.");
            return;
        }
        Err(error) => {
            println!("  port {port_number}: reset failed ({error:?}).");
            return;
        }
    };
    println!(
        "  port {port_number}: connected, {}.",
        if low_speed { "low-speed" } else { "full-speed" }
    );
    if low_speed {
        // Realistically only HID keyboards/mice; not this command's
        // concern, and not worth the extra low-speed-specific handling
        // UHCI's spec otherwise requires (a different max packet size
        // ceiling, and low-speed devices must be enumerated more
        // carefully around babble). See AGENTS.md.
        println!("    low-speed devices are not enumerated further.");
        return;
    }

    let mut first8 = [0u8; 8];
    // The USB spec mandates assuming a max packet size of 8 for exactly
    // this first request, before the device's real value (the 8th byte
    // of its own device descriptor) is known.
    if let Err(error) = controller.get_descriptor(0, 8, uhci::USB_DESCRIPTOR_DEVICE, &mut first8) {
        println!("    initial descriptor read failed: {error:?}");
        return;
    }
    let max_packet0 = first8[7];

    // A fixed device address: this command only ever enumerates one
    // device at a time (each call resets the port first), so there is
    // never a second already-addressed device to collide with.
    const DEVICE_ADDRESS: u8 = 1;
    if let Err(error) = controller.set_address(DEVICE_ADDRESS) {
        println!("    SET_ADDRESS failed: {error:?}");
        return;
    }

    let mut device_descriptor = [0u8; 18];
    if let Err(error) =
        controller.get_descriptor(DEVICE_ADDRESS, max_packet0, uhci::USB_DESCRIPTOR_DEVICE, &mut device_descriptor)
    {
        println!("    device descriptor read failed: {error:?}");
        return;
    }
    let vendor = u16::from_le_bytes([device_descriptor[8], device_descriptor[9]]);
    let product = u16::from_le_bytes([device_descriptor[10], device_descriptor[11]]);
    println!(
        "    device: vendor=0x{vendor:04x} product=0x{product:04x} class=0x{:02x} max_packet0={max_packet0}",
        device_descriptor[4]
    );

    let mut config_header = [0u8; 9];
    if let Err(error) = controller.get_descriptor(
        DEVICE_ADDRESS,
        max_packet0,
        uhci::USB_DESCRIPTOR_CONFIGURATION,
        &mut config_header,
    ) {
        println!("    configuration descriptor header read failed: {error:?}");
        return;
    }
    let total_length =
        (u16::from_le_bytes([config_header[2], config_header[3]]) as usize).min(uhci::MAX_DESCRIPTOR_BYTES);

    let mut config = [0u8; uhci::MAX_DESCRIPTOR_BYTES];
    let read = match controller.get_descriptor(
        DEVICE_ADDRESS,
        max_packet0,
        uhci::USB_DESCRIPTOR_CONFIGURATION,
        &mut config[..total_length],
    ) {
        Ok(read) => read,
        Err(error) => {
            println!("    configuration descriptor read failed: {error:?}");
            return;
        }
    };
    println!("    configuration descriptor ({read} bytes):");
    dump_bytes(0, &config[..read]);

    // bConfigurationValue is the configuration descriptor's 6th byte.
    let config_value = config.get(5).copied().unwrap_or(0);
    match controller.set_configuration(DEVICE_ADDRESS, max_packet0, config_value) {
        Ok(()) => println!("    configured (bConfigurationValue={config_value})."),
        Err(error) => println!("    SET_CONFIGURATION failed: {error:?}"),
    }
}

/// `usb` (no arguments) prints every controller/port, exactly as before;
/// `usb play [diskN] NAME` streams a raw PCM file to whatever USB Audio
/// Class playback device it can find, over a real isochronous OUT
/// transfer -- see `uhci.rs`'s `IsocOutStream` and AGENTS.md's "UHCI"
/// section for the design.
fn usb_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: &str =
        "Usage: usb | usb play [diskN] NAME | usb capture [diskN] NAME | usb storage | usb native";

    if arguments.is_empty() {
        print_usb_devices();
        return;
    }

    let (first, remainder) = take_word(arguments);
    if first.eq_ignore_ascii_case(b"native") && remainder.is_empty() {
        usb_native_command(info);
        return;
    }
    if first.eq_ignore_ascii_case(b"play") {
        let (device, remainder) = parse_disk_and_one_argument(remainder);
        let (name, trailing) = take_word(remainder);
        if name.is_empty() || !trailing.is_empty() {
            println!("{USAGE}");
            return;
        }
        usb_play_command(info, device, name);
        return;
    }
    if first.eq_ignore_ascii_case(b"capture") {
        let (device, remainder) = parse_disk_and_one_argument(remainder);
        let (name, trailing) = take_word(remainder);
        if name.is_empty() || !trailing.is_empty() {
            println!("{USAGE}");
            return;
        }
        usb_capture_command(info, device, name);
        return;
    }
    if first.eq_ignore_ascii_case(b"storage") {
        let (second, trailing) = take_word(remainder);
        if second.is_empty() {
            usb_storage_command();
            return;
        }
        if second.eq_ignore_ascii_case(b"attach") && trailing.is_empty() {
            usb_storage_attach_command(info);
            return;
        }
        if second.eq_ignore_ascii_case(b"promote") && trailing.is_empty() {
            usb_storage_promote_command(info);
            return;
        }
        println!("Usage: usb storage [attach|promote]");
        return;
    }

    println!("{USAGE}");
}

/// `usb native`: the short, self-explanatory alias for "switch every
/// BIOS-backed disk this boot can confirm is a discovered USB device over
/// to its native driver" -- generalizing `usb storage promote` (always
/// exactly `disk1`) to every disk that has one, via
/// `disk::promote_all_and_attach`. Named for what actually happens: the
/// disk itself is the same one BIOS already booted from (or already found
/// as an ordinary secondary disk) -- only its *mode* switches, from
/// BIOS's "USB Legacy Support" emulation to this driver's own native
/// Bulk-Only Transport reads/writes. Also attaches any leftover
/// discovered device that matched no existing disk, exactly like `usb
/// storage attach` would, so this one command covers both jobs `usb
/// storage attach`/`usb storage promote` used to require running
/// separately.
fn usb_native_command(info: &BootInfo) {
    println!("Resetting UHCI controllers to discover every attached USB");
    println!("Mass Storage device; this can break BIOS's own USB-drive");
    println!("emulation for any disk still served that way.");
    let report = disk::promote_all_and_attach(info);
    for result in report.results() {
        let Some(result) = result else { continue };
        let Some(device) = result.device else {
            if let disk::PromoteOutcome::AttachFailed(error) = result.outcome {
                println!("A leftover USB device could not be attached: {error:?}");
            }
            continue;
        };
        let reason: &str = match result.outcome {
            disk::PromoteOutcome::AlreadyPromoted => "already native USB",
            disk::PromoteOutcome::Promoted => "now reads/writes through the native USB driver",
            disk::PromoteOutcome::NoMatch => "no matching USB device found; still BIOS-backed",
            disk::PromoteOutcome::Attached => {
                let sectors = disk::device_sector_count(info, device);
                println!(
                    "disk{}: a new USB device, attached natively ({sectors} sector(s)).",
                    device_number(device)
                );
                continue;
            }
            disk::PromoteOutcome::AttachFailed(_) => continue,
        };
        println!("disk{}: {reason}.", device_number(device));
    }
}

/// Discovers a USB Mass Storage device and attaches it as a new
/// secondary disk (see `disk::attach_usb_disk`'s own doc comment for why
/// this is a deliberate, explicit operator action rather than something
/// `discover_secondary_disks` ever does automatically at boot). On
/// success the new disk is immediately usable through the ordinary
/// `diskN` selector, exactly like a BIOS-backed secondary disk.
fn usb_storage_attach_command(info: &BootInfo) {
    println!("Resetting UHCI controllers to discover the device; this");
    println!("can break BIOS's own USB-drive emulation for any disk");
    println!("(including 'disk1') that BIOS is still serving over USB.");
    match disk::attach_usb_disk() {
        Ok(index) => {
            let disk_number = index + 2;
            let sectors = disk::device_sector_count(info, disk::Device::Secondary(index));
            println!("Attached as disk{disk_number} ({sectors} sector(s)).");
        }
        Err(disk::UsbAttachError::NoDevice) => {
            println!("No USB Mass Storage device found.");
        }
        Err(disk::UsbAttachError::NotReady) => {
            println!("Device did not report ready.");
        }
        Err(disk::UsbAttachError::Capacity(error)) => {
            println!("READ CAPACITY failed: {error:?}");
        }
        Err(disk::UsbAttachError::Full) => {
            println!("No free secondary disk slot available.");
        }
        Err(disk::UsbAttachError::AlreadyAttached(device)) => {
            println!("This USB device is already attached as disk{}", device_number(device));
            println!("(its handle was refreshed after the port reset).");
        }
    }
}

/// Discovers a USB Mass Storage device and, only if its capacity and
/// first-sector bytes exactly match `disk1`'s own BIOS-read baseline,
/// switches `disk1` itself over to the native driver -- see
/// `disk::promote_boot_to_usb`'s own doc comment for the full
/// cross-validation this performs and why it is safe to trust more than
/// a bare "assume the only device found is the boot device" guess. This
/// is the step that actually removes `disk1`'s own dependency on BIOS's
/// "USB Legacy Support" staying intact once any UHCI controller has been
/// reset.
fn usb_storage_promote_command(info: &BootInfo) {
    println!("Resetting UHCI controllers to discover the device; this");
    println!("can break BIOS's own USB-drive emulation for any disk");
    println!("still served that way. Comparing its capacity and first");
    println!("sector against disk1's own BIOS-read baseline first.");
    match disk::promote_boot_to_usb(info) {
        Ok(()) => println!("disk1 now reads/writes through the native USB driver."),
        Err(disk::BootPromoteError::AlreadyPromoted) => {
            println!("disk1 is already USB-backed.");
        }
        Err(disk::BootPromoteError::BiosReadFailed) => {
            println!("Could not read disk1's boot sector via BIOS.");
        }
        Err(disk::BootPromoteError::NoDevice) => {
            println!("No USB Mass Storage device found.");
        }
        Err(disk::BootPromoteError::NotReady) => {
            println!("Device did not report ready.");
        }
        Err(disk::BootPromoteError::Capacity(error)) => {
            println!("READ CAPACITY failed: {error:?}");
        }
        Err(disk::BootPromoteError::NotSameDisk) => {
            println!("Discovered device does not match disk1's own boot");
            println!("sector/capacity; disk1 was left untouched.");
        }
    }
}

/// Standalone verification for `kernel/src/usb_storage.rs`, mirroring
/// `usb play`/`usb capture`'s own "prove the driver against real/QEMU
/// hardware with a dedicated command before any deeper integration"
/// precedent: enumerates the first Bulk-Only Transport Mass Storage
/// device found, prints its `INQUIRY` string and `READ CAPACITY` result,
/// and hex-dumps its first 512-byte sector. Deliberately does not touch
/// FOSFS or `disk.rs` -- that integration is separate, later work.
fn usb_storage_command() {
    let Some(mut device) = usb_storage::open() else {
        println!("No USB Mass Storage (Bulk-Only Transport, SCSI");
        println!("transparent) device found.");
        return;
    };

    if !device.wait_until_ready(10) {
        println!("Device did not report ready after 10 attempts.");
        let mut sense = [0u8; 18];
        if let Ok(len) = device.request_sense(&mut sense) {
            println!("REQUEST SENSE:");
            dump_bytes(0, &sense[..len]);
        }
        return;
    }

    let mut inquiry = [0u8; 36];
    match device.inquiry(&mut inquiry) {
        Ok(len) => {
            let vendor = core::str::from_utf8(&inquiry[8..16.min(len)]).unwrap_or("?").trim();
            let product = core::str::from_utf8(&inquiry[16..32.min(len)]).unwrap_or("?").trim();
            println!("INQUIRY: vendor='{vendor}' product='{product}'");
        }
        Err(error) => {
            println!("INQUIRY failed: {error:?}");
            return;
        }
    }

    let (block_count, block_size) = match device.read_capacity_10() {
        Ok(capacity) => capacity,
        Err(error) => {
            println!("READ CAPACITY failed: {error:?}");
            return;
        }
    };
    println!("READ CAPACITY: {block_count} block(s) of {block_size} bytes");

    let mut sector = [0u8; usb_storage::BLOCK_SIZE];
    match device.read_10(0, &mut sector) {
        Ok(()) => {
            println!("First sector:");
            dump_bytes(0, &sector);
        }
        Err(error) => println!("READ(10) failed: {error:?}"),
    }
}

/// Streams a raw PCM file already matching a discovered playback
/// device's exact negotiated format (no resampling or reformatting --
/// the same expectation every other audio backend here already has) to
/// its isochronous OUT endpoint. Reuses the shared `pmem::TAPE_BASE`/
/// `TAPE_LEN` staging buffer `copy`/`tape load`/NETFS already document
/// sharing: because that buffer is one contiguous physical block, the
/// isochronous engine's TD buffer pointers reference it directly, with
/// no separate packet-buffer pool or copy step needed.
fn usb_play_command(info: &BootInfo, device: disk::Device, name: &[u8]) {
    // The address `find_uac_device` just assigned from the shared allocator
    // (`uhci::allocate_device_address`); it changes on every discovery.
    let found = uhci::find_uac_device()
        .and_then(|(controller, uac)| Some((controller, uac.address, uac.playback?)));
    let Some((controller, device_address, endpoint)) = found else {
        println!("No USB Audio Class playback device found (needs PCM,");
        println!("16-bit, a fixed rate divisible by 1000, and a");
        println!("Synchronous/Adaptive isochronous OUT endpoint).");
        return;
    };
    println!(
        "Found endpoint 0x{:02x}: {} Hz/{}-bit/{}ch, {} bytes/packet.",
        endpoint.endpoint_address, endpoint.sample_rate, endpoint.bits_per_sample, endpoint.channels,
        endpoint.packet_bytes
    );

    let frame_bytes = u32::from(endpoint.channels) * (u32::from(endpoint.bits_per_sample) / 8);
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) if length == 0 => {
            println!("'{}' is empty; nothing to play.", core::str::from_utf8(name).unwrap_or("?"));
            return;
        }
        Ok(Some(length)) if length > pmem::TAPE_LEN as u64 => {
            println!("File is too large for the shared staging buffer.");
            return;
        }
        Ok(Some(length)) => length as usize,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length as u32 % frame_bytes != 0 {
        println!(
            "'{}' length is not a whole number of {}-byte frames.",
            core::str::from_utf8(name).unwrap_or("?"),
            frame_bytes
        );
        return;
    }

    let Some(address) = allocate_tape_buffer(length as u32) else { return };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }

    let endpoint_number = endpoint.endpoint_address & 0x0f;
    let mut stream = controller.start_isoc_out(device_address, endpoint_number, endpoint.packet_bytes, address, length);

    println!("Playing {length} byte(s); press F12 to stop early.");
    let start_ticks = interrupts::ticks();
    let mut aborted = false;
    loop {
        if matches!(keyboard::poll_term_event(), Some(keyboard::KeyEvent::Escape)) {
            aborted = true;
            break;
        }
        poll_background_work(None);
        if stream.service(controller) == uhci::IsocStatus::Finished {
            break;
        }
    }
    stream.stop(controller);
    // Courteous teardown: quiesce the streaming interface back to its
    // zero-bandwidth alternate setting. A failure here isn't reported --
    // the stream itself already finished or was aborted either way.
    let _ = controller.set_interface(device_address, 8, endpoint.interface_number, 0);

    let elapsed_ms = interrupts::ticks().wrapping_sub(start_ticks) * 10; // 100 Hz PIT
    if aborted {
        println!("Playback stopped early after {elapsed_ms} ms.");
    } else {
        println!("Playback complete ({elapsed_ms} ms).");
    }
    let errors = stream.error_count();
    if errors > 0 {
        println!("Warning: {errors} isochronous packet(s) failed (lost audio).");
    }
}

/// Captures raw isochronous IN audio from a discovered USB Audio Class
/// capture device into the shared `pmem::TAPE_BASE`/`TAPE_LEN` staging
/// buffer (the same one `usb play` reuses for the opposite direction),
/// until either that buffer's 4 MiB capacity is reached or the operator
/// presses F12, then writes exactly however many bytes were actually
/// received out to the named FOSFS file. Raw device format, no
/// resampling/reformatting -- the same expectation every other audio
/// backend here already has for playback.
fn usb_capture_command(info: &BootInfo, device: disk::Device, name: &[u8]) {
    // The address `find_uac_device` just assigned from the shared allocator
    // (`uhci::allocate_device_address`); it changes on every discovery.
    let found = uhci::find_uac_device()
        .and_then(|(controller, uac)| Some((controller, uac.address, uac.capture?)));
    let Some((controller, device_address, endpoint)) = found else {
        println!("No USB Audio Class capture device found (needs PCM,");
        println!("16-bit, a fixed rate divisible by 1000, and a");
        println!("Synchronous/Adaptive isochronous IN endpoint).");
        return;
    };
    println!(
        "Found endpoint 0x{:02x}: {} Hz/{}-bit/{}ch, {} bytes/packet.",
        endpoint.endpoint_address, endpoint.sample_rate, endpoint.bits_per_sample, endpoint.channels,
        endpoint.packet_bytes
    );

    let frame_bytes = u32::from(endpoint.channels) * (u32::from(endpoint.bits_per_sample) / 8);
    let capacity = (pmem::TAPE_LEN / frame_bytes) * frame_bytes;
    let Some(address) = allocate_tape_buffer(capacity) else { return };

    let endpoint_number = endpoint.endpoint_address & 0x0f;
    let mut stream =
        controller.start_isoc_in(device_address, endpoint_number, endpoint.packet_bytes, address, capacity as usize);

    println!("Capturing up to {capacity} byte(s); press F12 to stop early.");
    let start_ticks = interrupts::ticks();
    let mut aborted = false;
    loop {
        if matches!(keyboard::poll_term_event(), Some(keyboard::KeyEvent::Escape)) {
            aborted = true;
            break;
        }
        poll_background_work(None);
        if stream.service(controller) == uhci::IsocStatus::Finished {
            break;
        }
    }
    stream.stop(controller);
    // Courteous teardown: quiesce the streaming interface back to its
    // zero-bandwidth alternate setting. A failure here isn't reported --
    // the stream itself already finished or was aborted either way.
    let _ = controller.set_interface(device_address, 8, endpoint.interface_number, 0);

    let elapsed_ms = interrupts::ticks().wrapping_sub(start_ticks) * 10; // 100 Hz PIT
    let received = stream.received_bytes();
    if aborted {
        println!("Capture stopped early after {elapsed_ms} ms ({received} byte(s)).");
    } else {
        println!("Capture complete ({elapsed_ms} ms, {received} byte(s)).");
    }
    let errors = stream.error_count();
    if errors > 0 {
        println!("Warning: {errors} isochronous packet(s) failed (lost audio).");
    }

    if received == 0 {
        println!("Nothing captured; not writing a file.");
        return;
    }
    let captured = unsafe { core::slice::from_raw_parts(address as *const u8, received) };
    match fosfs::write(info, device, name, captured) {
        Ok(fosfs::WriteOutcome::Written) => {
            println!("Wrote {received} byte(s) to '{}'.", core::str::from_utf8(name).unwrap_or("?"));
        }
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// `audio` reports every discovered audio device's status through the
/// format-agnostic `audio.rs` facade (not `es1370::get_at()` directly, so
/// this command keeps working unchanged once a second codec backend joins
/// `audio::Audio`): whether any device was found, and per device its
/// index, driver name and format, I/O base, how many playback
/// interrupts/periods it has serviced -- the ISR-driven equivalent of
/// `nic`'s own interrupt counters, useful for confirming the driver is
/// actually being interrupted and refilling its DMA ring, not just
/// silently idle -- and, once a tape has started streaming into it
/// (`tape_audio_submit`/`tape_audio_submit_at`), the network-fed ring's
/// current backlog and underrun count. Unlike `nic`/`nic list` (one
/// "active" interface plus a full list), there is no single "active"
/// audio device to report first -- every discovered device is reported
/// equally, since the whole point of more than one is to drive them all
/// simultaneously as independent stereo pairs (see `audio.rs`'s own doc
/// comment on why there is no `set_active` here). Like `pci`, this is
/// allowed to scroll past one screen once several devices are attached,
/// since the number of physical cards is inherently unbounded.
fn audio_status() {
    match audio::get() {
        Err(audio::Status::NotPresent) => {
            println!("No supported audio controller found on the PCI bus.");
            return;
        }
        Err(audio::Status::InitFailed) => {
            println!("An audio controller was found but could not be initialized.");
            return;
        }
        Ok(_) => {}
    }
    let count = audio::count();
    println!("{count} audio device(s) found:");
    for index in 0..count {
        let Some(device) = audio::get_at(index) else {
            continue;
        };
        let format = device.format();
        println!(
            "[{index}] {} at base 0x{:08x}: {} Hz / {}ch / {}-bit.",
            device.driver_name(),
            device.io_base(),
            format.sample_rate,
            format.channels,
            format.bits_per_sample
        );
        let (irq, interrupts, periods) = device.interrupt_stats();
        match irq {
            Some(irq) => {
                let raw = crate::interrupts::pci_irq_count(irq);
                println!(
                    "  IRQ {irq}: {raw} raw fire(s), {interrupts} recognized as DAC1."
                );
            }
            // Was safe to assume "no IRQ line" meant "not actually
            // playing" back when only ES1370/HDA existed here (IRQ
            // registration is required for either to ever reach this
            // facade at all) -- but it is the *normal*, expected state
            // for a USB Audio device, which is never silent just
            // because it has no IRQ (see `uhci::UsbAudioDevice::
            // interrupt_stats`'s own doc comment: it is foreground-
            // polled, not IRQ-driven, by design).
            None => println!("  No legacy PCI IRQ line (foreground-polled, not IRQ-driven, for this backend)."),
        }
        println!("  DAC1: {periods} periods refilled since boot.");
        println!(
            "  Underruns (ring empty on refill): {}",
            device.underrun_count()
        );
        println!(
            "  Network-fed ring backlog: {} of {} byte(s) queued.",
            device.ring_backlog(),
            audio::RING_CAPACITY_BYTES
        );
        // A separate format line, not folded into the playback one above:
        // ES1370/HDA's ADC always shares that same format, but a USB
        // Audio device's capture endpoint is not guaranteed to (the real
        // TerraTec Aureon Dual's own microphone is mono against a stereo
        // playback format) -- see `Audio::capture_format`'s own doc
        // comment.
        let capture_format = device.capture_format();
        println!(
            "  ADC: {} Hz / {}ch / {}-bit.",
            capture_format.sample_rate,
            capture_format.channels,
            capture_format.bits_per_sample
        );
        println!(
            "  {} periods captured; overruns (ring full): {}.",
            device.capture_periods(),
            device.capture_overrun_count()
        );
        println!(
            "  Capture ring backlog: {} of {} byte(s) queued.",
            device.capture_backlog(),
            audio::RING_CAPACITY_BYTES
        );
        println!("  Raw STATUS register: 0x{:08x}", device.raw_status());
    }
}

fn hda_status() {
    match hda::initialize_all() {
        Err(hda::Status::NotPresent) => {
            println!("No Intel HD Audio controller found.");
            return;
        }
        Err(hda::Status::InitFailed) => {
            println!("An Intel HD Audio controller was found but could not be reset.");
            return;
        }
        Ok(count) => {
            println!("{count} Intel HD Audio controller(s) discovered:");
        }
    }
    for index in 0..hda::count() {
        let Some(controller) = hda::get(index) else {
            continue;
        };
        hda_status_one(controller);
    }
}

fn hda_status_one(controller: &hda::Hda) {
    println!(
        "Controller {}: MMIO base 0x{:08x}.",
        controller.instance(),
        controller.mmio_base()
    );
    match controller.msi_vector() {
        Some(vector) => {
            let count = crate::interrupts::msi_irq_count(vector);
            println!("  MSI enabled, vector 0x{vector:02x}: {count} fire(s).");
        }
        None => match controller.irq() {
            Some(irq) => println!("  Legacy PCI IRQ line: {irq}."),
            None => println!("  No legacy PCI IRQ line assigned."),
        },
    }
    println!(
        "  CORB/RIRB command transport up: {} input, {} output stream(s).",
        controller.input_stream_count(),
        controller.output_stream_count()
    );
    if controller.has_playback() {
        let format = controller.format();
        let (irq, interrupts, periods) = controller.interrupt_stats();
        println!(
            "  Playback active: {} Hz / {}ch / {}-bit.",
            format.sample_rate, format.channels, format.bits_per_sample
        );
        // Delivery mechanism (MSI vs. legacy IRQ) was already reported
        // above; a legacy `pci_irq_count(irq)` "raw fire(s)" figure would
        // be misleadingly stuck at 0 here whenever MSI is what's actually
        // delivering these interrupts, since that counter only tracks
        // PIC-dispatched fires.
        if controller.msi_vector().is_none() {
            match irq {
                Some(irq) => {
                    let raw = crate::interrupts::pci_irq_count(irq);
                    println!("  IRQ {irq}: {raw} raw fire(s), {interrupts} recognized.");
                }
                None => println!("  No legacy PCI IRQ line assigned; playback is silent."),
            }
        } else {
            println!("  {interrupts} interrupt(s) recognized as playback (via MSI above).");
        }
        println!("  {periods} period(s) refilled since boot.");
        println!(
            "  Underruns (ring empty on refill): {}",
            controller.underrun_count()
        );
        println!(
            "  Network-fed ring backlog: {} of {} byte(s) queued.",
            controller.ring_backlog(),
            audio::RING_CAPACITY_BYTES
        );
    } else {
        println!("  No usable playback stream (no codec/DAC/pin found, or no free output stream).");
    }
    if controller.has_capture() {
        let format = controller.format();
        println!(
            "  Capture active: {} Hz / {}ch / {}-bit.",
            format.sample_rate, format.channels, format.bits_per_sample
        );
        println!(
            "  {} period(s) captured since boot.",
            controller.capture_periods()
        );
        println!(
            "  Overruns (ring full on drain): {}",
            controller.capture_overrun_count()
        );
        println!(
            "  Capture ring backlog: {} of {} byte(s) queued.",
            controller.capture_backlog(),
            audio::RING_CAPACITY_BYTES
        );
    } else {
        println!("  No usable capture stream (no codec/ADC/pin found, or no free input stream).");
    }
    let codecs = controller.codecs();
    if codecs.iter().flatten().count() == 0 {
        println!("  No codec responded on STATESTS after reset.");
        return;
    }
    println!("  Codec(s) discovered:");
    for codec in codecs.iter().flatten() {
        println!(
            "  [cad {}] vendor:device 0x{:04x}:0x{:04x}",
            codec.address, codec.vendor_id, codec.device_id
        );
        match codec.afg_nid {
            Some(afg_nid) => println!("    Audio Function Group: nid {afg_nid}"),
            None => println!("    No Audio Function Group found."),
        }
        for widget in codec.widgets().iter().flatten() {
            let stereo = if widget.stereo { "stereo" } else { "mono" };
            print!("    nid {}: {} ({stereo})", widget.nid, widget.widget_type.name());
            if widget.widget_type == hda::WidgetType::Pin {
                print!(
                    " out={} in={}",
                    widget.pin_output_capable as u8, widget.pin_input_capable as u8
                );
            }
            match widget.connection {
                Some(source) => println!(", conn -> nid {source}"),
                None => println!(),
            }
        }
    }
}

fn print_printer_ports() {
    let ports = parallel::detect_ports();
    for (index, name) in PRINTER_PORT_NAMES.iter().enumerate() {
        let name = core::str::from_utf8(name).unwrap_or("?");
        match ports[index] {
            Some(base) => {
                println!("{name}: 0x{base:04x} (free for 'term printer'/'term {name}')");
                let status = parallel::Printer::new(base).status();
                println!(
                    "  status=0x{status:02x}: busy={} paper_out={} select={} error={}",
                    (status & 0x80 != 0) as u8,
                    (status & 0x20 != 0) as u8,
                    (status & 0x10 != 0) as u8,
                    (status & 0x08 == 0) as u8,
                );
            }
            None => println!("{name}: not detected"),
        }
    }
}

/// The first detected LPT port, for `term printer`/`print`'s "no port
/// named explicitly" default -- mirrors how bare `term` defaults to COM1.
fn default_printer_port() -> Option<(&'static [u8], u16)> {
    let ports = parallel::detect_ports();
    let index = ports.iter().position(Option::is_some)?;
    Some((PRINTER_PORT_NAMES[index], ports[index].unwrap()))
}

/// One human-readable line per [`parallel::PrinterFault`], shown once when
/// a fault first appears (not on every retry -- see `drain_print_relay`)
/// and reused by both `term_printer` and `print_command`.
fn printer_fault_message(fault: parallel::PrinterFault) -> &'static str {
    match fault {
        parallel::PrinterFault::NotSelected => {
            "Printer offline (not selected); waiting... (F12 to give up)"
        }
        parallel::PrinterFault::PaperOut => "Printer out of paper; waiting... (F12 to give up)",
        parallel::PrinterFault::ErrorLine => {
            "Printer reports an error condition; waiting... (F12 to give up)"
        }
        parallel::PrinterFault::Busy => {
            "Printer busy longer than expected; still waiting... (F12 to give up)"
        }
    }
}

/// Writes one byte to `printer`, retrying with the *same* byte on a fault
/// (paper out, offline, a latched error, or an unexpectedly long busy
/// spell) until it clears or the operator gives up with F12 -- "pause,
/// don't abort" (see `term_printer`'s doc comment). `last_fault` is
/// threaded through by the caller across every byte in a session (not
/// reset per call) so the same ongoing fault is reported only once, not
/// re-announced for every subsequent byte or keystroke, and a recovery is
/// announced exactly once too. Returns `false` if the operator gave up.
fn write_to_printer_with_retry(
    printer: &parallel::Printer,
    byte: u8,
    last_fault: &mut Option<parallel::PrinterFault>,
) -> bool {
    loop {
        match printer.write_byte(byte) {
            Ok(()) => {
                if last_fault.take().is_some() {
                    println!("Printer recovered; resuming.");
                }
                return true;
            }
            Err(fault) => {
                if *last_fault != Some(fault) {
                    println!("{}", printer_fault_message(fault));
                    *last_fault = Some(fault);
                }
                if matches!(keyboard::poll_term_event(), Some(keyboard::KeyEvent::Escape)) {
                    return false;
                }
            }
        }
    }
}

/// Drains the entire print-relay queue to `printer`, one byte at a time,
/// via `write_to_printer_with_retry` -- used for a command's own captured
/// output (queued by `console::relay_push` while the print relay is
/// active), never for `term_printer`'s live keystroke echo, which writes
/// directly instead (see `term_printer`'s own doc comment for why).
///
/// Returns `false` if the operator gave up mid-fault, leaving whatever was
/// still queued undelivered (silently dropped, exactly like every other
/// best-effort output queue in this kernel -- see `console::relay_push`'s
/// own doc comment).
fn drain_print_relay(
    printer: &parallel::Printer,
    last_fault: &mut Option<parallel::PrinterFault>,
) -> bool {
    let mut chunk = [0u8; 256];
    loop {
        let queued = console::peek_print_relay(&mut chunk);
        if queued == 0 {
            return true;
        }
        let mut written = 0;
        while written < queued {
            if !write_to_printer_with_retry(printer, chunk[written], last_fault) {
                console::discard_print_relay(written);
                return false;
            }
            written += 1;
        }
        console::discard_print_relay(written);
    }
}

/// `print [diskN] NAME` prints one FOSFS file's raw contents; `print run
/// CMD...` runs an ordinary command and prints only its output (never the
/// echoed command line itself, unlike `term_printer`'s own transcript) --
/// the intended way to get a one-off paper copy of something like `help`
/// without leaving it permanently disabled-by-habit inside a printer
/// session (see `execute`'s `help` arm). Usable at any time, independent
/// of whether a `term_printer` session is currently open.
#[inline(never)]
fn print_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: &str = "Usage: print [diskN] NAME | print run CMD...";

    let (first, remainder) = take_word(arguments);
    if first.eq_ignore_ascii_case(b"run") {
        if remainder.is_empty() {
            println!("{USAGE}");
            return;
        }
        print_run(remainder, info);
        return;
    }

    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, trailing) = take_word(arguments);
    if name.is_empty() || !trailing.is_empty() {
        println!("{USAGE}");
        return;
    }
    print_file(info, device, name);
}

/// Reads a whole file through the shared tape staging buffer
/// (`pmem::TAPE_BASE`/`TAPE_LEN`, exactly like `copy_file` -- see that
/// function's doc comment for why sharing it with the tape subsystem is
/// safe) and queues its raw bytes onto the print relay via `console::
/// print_relay_push_raw`, bypassing VGA/serial entirely: a file being
/// printed is not also a transcript of anything typed, so there is
/// nothing to echo.
///
/// Nesting-aware, exactly like `print_run` (see its doc comment for why):
/// if a `term_printer` session's own relay is already active (this was
/// typed as a command from inside one), this only queues the file's bytes
/// and leaves enabling/draining/disabling to that outer session, so the
/// file's content lands on the same paper as everything else the session
/// prints instead of splitting into its own separate, premature print job.
/// Otherwise (typed at the ordinary local prompt via `print [diskN]
/// NAME`), it owns the whole enable-queue-drain-disable cycle itself.
fn print_file(info: &BootInfo, device: disk::Device, name: &[u8]) {
    let Some((printer_name, base)) = default_printer_port() else {
        println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
        return;
    };
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length > u32::MAX as u64 {
        println!("File is too large to print through the shared tape-buffer scratch area.");
        return;
    }
    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    invalidate_tape_after_scratch_use();

    let owns_relay = !console::print_relay_active();
    let printer_name = core::str::from_utf8(printer_name).unwrap_or("?");
    println!(
        "Printing {length} byte(s) from disk{} '{}' to {printer_name}...",
        device_number(device),
        core::str::from_utf8(name).unwrap_or("?")
    );
    if owns_relay {
        console::enable_print_relay();
    }
    console::print_relay_push_raw(buffer);
    if !owns_relay {
        // The outer `term_printer` session owns draining/disabling this
        // queue; just leave the bytes queued for it.
        return;
    }
    let printer = parallel::Printer::new(base);
    let mut last_fault = None;
    let finished = drain_print_relay(&printer, &mut last_fault);
    console::disable_print_relay();
    if finished {
        println!("Done.");
    } else {
        println!("Gave up printing '{}'.", core::str::from_utf8(name).unwrap_or("?"));
    }
}

/// `print run CMD...` runs `command` with the print-relay queue enabled
/// (so its ordinary console output is captured, exactly as
/// `term_printer`'s own committed commands are) and then drains that
/// queue to the printer -- but, unlike `term_printer`, never injects the
/// command line itself into the queue: this is meant purely as "print
/// what this command would have shown," not a transcript entry.
///
/// Temporarily clears `PRINTER_SESSION_ACTIVE` around the nested
/// `execute` call so `print run help` (the documented, deliberate
/// exception to `help`'s printer-session refusal -- see that arm's doc
/// comment) works even when typed *from inside* an active `term_printer`
/// session, not only from the ordinary local prompt; the previous value
/// is restored afterward regardless of which one that was.
///
/// Nesting-aware: `console::print_relay_active()` is checked *before* any
/// of the above, since a `term_printer` session's own committed-line flow
/// already enables the print relay and queues that line's transcript
/// before calling `execute` (which is what reaches this function for
/// `print run ...` typed inside a session). Calling `enable_print_relay`
/// again here would discard that not-yet-drained transcript (it resets
/// the queue), and draining/disabling here would end the outer session's
/// print job early, splitting one line's worth of output across two
/// separate physical print jobs instead of one. When nested, this instead
/// just runs `command` and leaves every bit of enabling, draining, and
/// disabling to the outer `term_printer` loop.
fn print_run(command: &[u8], info: &BootInfo) {
    let Some((printer_name, base)) = default_printer_port() else {
        println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
        return;
    };
    let owns_relay = !console::print_relay_active();
    let printer_name = core::str::from_utf8(printer_name).unwrap_or("?");
    println!(
        "Running '{}' and printing its output to {printer_name}...",
        core::str::from_utf8(command).unwrap_or("?")
    );
    if owns_relay {
        console::enable_print_relay();
        unsafe {
            ACTIVE_PRINT_BASE = Some(base);
            ACTIVE_PRINT_FAULT = None;
        }
    }
    let was_active = unsafe { PRINTER_SESSION_ACTIVE };
    unsafe {
        PRINTER_SESSION_ACTIVE = false;
    }
    execute(command, info);
    unsafe {
        PRINTER_SESSION_ACTIVE = was_active;
    }
    if !owns_relay {
        return;
    }
    let printer = parallel::Printer::new(base);
    let finished = drain_print_relay(&printer, active_print_fault_mut());
    console::disable_print_relay();
    unsafe {
        ACTIVE_PRINT_BASE = None;
        ACTIVE_PRINT_FAULT = None;
    }
    if finished {
        println!("Done.");
    } else {
        println!("Gave up printing the command's output.");
    }
}


/// A minimal raw serial terminal emulator: once entered, the chosen port
/// stops being available for anything else and instead becomes a
/// dedicated link to whatever is on the other end of the wire, byte for
/// byte, with no local line editing or echo -- the remote side is expected
/// to echo your keystrokes itself, exactly as a real terminal program
/// would.
///
/// With no arguments, `term` reuses the single COM1 port most real
/// machines actually have, at its fixed 115200 baud (see below). Because
/// this repurposes COM1, a serial-only session controlling fos through
/// that same port would lose its ability to type further commands once
/// `term` starts; local VGA + PS/2 keyboard input is unaffected and
/// remains the way to invoke `term` and to leave it again with F12 (or F4/F6/F7/F8/F10)
/// (chosen specifically because function keys sit outside the
/// Shift/AltGr layers that a Nordic keyboard puts most punctuation
/// behind).
///
/// `term COM2`/`COM3`/`COM4`/`PCI0`/`PCI1`/... [BAUD] uses an additional
/// port instead, if one was detected (see `serial`): COM2-4 via the BIOS
/// Data Area, PCI ports via PCI configuration-space enumeration
/// (`console::detect_pci_ports`). None of those are ever used by the
/// interactive shell, so -- unlike COM1, whose rate must stay fixed so it
/// keeps matching whatever terminal program is already attached to it --
/// their baud rate can be freely chosen; it must evenly divide 115200
/// (the UART's fixed clock) so the requested rate is reproduced exactly,
/// e.g. 115200, 57600, 38400, 19200, 9600, 4800, 2400, 1200.
///
/// `term net` is unrelated to any serial port: it waits for a remote
/// terminal client to say hello over Ethernet (see `term_net`), then
/// hands local keyboard/VGA control over entirely so the connected peer
/// drives this same live shell -- unlike `term COM1`/etc, which is a dumb
/// byte pipe with no shell involvement at all.
#[inline(never)]
fn term_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: &str =
        "Usage: term [COM1|COM2|COM3|COM4|PCI0|PCI1|...|printer|LPT1|...|net] [BAUD]";

    let (port_word, remainder) = take_word(arguments);
    let (baud_word, remainder) = take_word(remainder);
    if !remainder.is_empty() {
        println!("{USAGE}");
        return;
    }

    let baud = if baud_word.is_empty() {
        None
    } else {
        match parse_number(baud_word) {
            Some(baud) => Some(baud),
            None => {
                println!("Invalid baud rate.");
                return;
            }
        }
    };

    if port_word.eq_ignore_ascii_case(b"net") {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        term_net(info);
        return;
    }

    if port_word.eq_ignore_ascii_case(b"printer") {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        let ports = parallel::detect_ports();
        let Some(index) = ports.iter().position(Option::is_some) else {
            println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
            return;
        };
        term_printer(info, PRINTER_PORT_NAMES[index], ports[index].unwrap());
        return;
    }

    if let Some(index) = PRINTER_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        let ports = parallel::detect_ports();
        let Some(base) = ports[index] else {
            let name = core::str::from_utf8(PRINTER_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected by the BIOS at boot; see 'printer'.");
            return;
        };
        term_printer(info, PRINTER_PORT_NAMES[index], base);
        return;
    }

    if port_word.is_empty() || SERIAL_PORT_NAMES[0].eq_ignore_ascii_case(port_word) {
        if baud.is_some() {
            println!("COM1's baud rate is fixed at 115200 (it is shared with the local shell");
            println!("console, so changing it mid-session would desync anyone attached to it).");
            println!("Use COM2, COM3, COM4, or a PCI port for a custom baud rate.");
            return;
        }
        println!("Entering raw terminal mode on COM1 at 115200 baud: your keystrokes are sent");
        println!("byte-for-byte, with no local echo or line editing -- the far end echoes them");
        println!("back. Press F12 (or F4/F6/F7/F8/F10) to return to the fos shell.");
        println!("If you are controlling fos over this same serial link, note that link");
        println!("is about to become the remote connection instead: use the VGA console");
        println!("to get back in that case.");
        term_loop(console::write_serial_byte, console::read_serial_byte);
        println!("\nLeft terminal mode; back at the fos shell.");
        return;
    }

    if let Some(index) = SERIAL_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        let ports = console::detect_ports();
        let Some(base) = ports[index] else {
            let name = core::str::from_utf8(SERIAL_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected by the BIOS at boot; see 'serial'.");
            return;
        };
        run_uart_term(SERIAL_PORT_NAMES[index], base, baud);
        return;
    }

    if let Some(index) = PCI_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        let (pci_ports, pci_count) = console::detect_pci_ports();
        if index >= pci_count {
            let name = core::str::from_utf8(PCI_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected via PCI enumeration; see 'serial'.");
            return;
        }
        run_uart_term(PCI_PORT_NAMES[index], pci_ports[index], baud);
        return;
    }

    println!("{USAGE}");
}

/// Shared by every non-COM1 port (COM2-4 and PCI-discovered ports): each
/// gets its own freshly configured `console::Uart` rather than the shared
/// console link, so it may use any baud rate the caller asks for.
fn run_uart_term(name_bytes: &[u8], base: u16, baud: Option<u32>) {
    let name = core::str::from_utf8(name_bytes).unwrap_or("?");
    let baud = baud.unwrap_or(console::UART_CLOCK);
    let uart = console::Uart::new(base);
    if !uart.configure(baud) {
        println!("Could not initialize {name} at {baud} baud (rate must evenly divide 115200,");
        println!("e.g. 115200, 57600, 38400, 19200, 9600, 4800, 2400, 1200).");
        return;
    }
    println!("Entering raw terminal mode on {name} at {baud} baud: your keystrokes are sent");
    println!("byte-for-byte, with no local echo or line editing -- the far end echoes them");
    println!("back. Press F12 (or F4/F6/F7/F8/F10) to return to the fos shell.");
    term_loop(|byte| uart.write_byte(byte), || uart.read_byte());
    println!("\nLeft terminal mode; back at the fos shell.");
}

/// Shared by every `term` port: forwards keystrokes via `write` until F12 (or F4/F6/F7/F8/F10)
/// is pressed, and shows bytes arriving via `read` locally (VGA only, so
/// they are never re-echoed back over the same wire they arrived on).
fn term_loop(write: impl Fn(u8), read: impl Fn() -> Option<u8>) {
    loop {
        match keyboard::poll_term_event() {
            Some(keyboard::KeyEvent::Escape) => break,
            Some(keyboard::KeyEvent::Char(byte)) => write(byte),
            // This raw byte pipe has no line concept for F2 to discard;
            // only `term_printer`'s own typing loop gives it meaning.
            Some(keyboard::KeyEvent::KillLine) | None => {}
        }
        if let Some(byte) = read() {
            console::write_vga_only(&[byte]);
        }
    }
}

/// A full remote shell over Ethernet: unlike `term`'s raw byte pipe, this
/// hands the actual local shell -- prompt, line editing, and every
/// existing command (`nic`, `chat`, `tape`, ...) -- over to whichever
/// remote peer connects, so a client only needs to speak the small
/// `TERM_TAG_*` framing below, not reimplement any command logic.
///
/// Waits (F12 (or F4/F6/F7/F8/F10) to give up, same convention as `nic discover`/`chat`/`term`)
/// for a `TERM_TAG_HELLO` from any peer, then locks onto that peer's MAC
/// for the rest of the session -- a second `HELLO` from someone else while
/// already connected is ignored, since only one remote client can drive
/// this single-threaded shell at a time. Once connected:
///
/// - Everything the shell would normally print to VGA/COM1 (prompts,
///   echoed keystrokes, command output) is instead captured by
///   `console::enable_relay` and forwarded as `TERM_TAG_DATA` frames, so
///   the remote peer sees exactly what a local operator would.
/// - Incoming `TERM_TAG_DATA` payload bytes are fed one at a time into a
///   fresh `LineEditor`, exactly as if they had been typed locally,
///   including running `execute` on Enter.
/// - Local VGA/PS/2 input is *not* read at all while connected (this
///   session is exclusive, not mirrored): the only way to interrupt it
///   locally is F12 (or F4/F6/F7/F8/F10), which sends `TERM_TAG_BYE` to the peer and
///   returns control to the local shell for good. If the *peer* ends the
///   session first (its own `TERM_TAG_BYE`, or it simply vanishes and a
///   later `HELLO` from someone else arrives), this does not return either
///   -- it goes back to waiting for a new client instead, so the only way
///   to actually leave `term net` and reach the local shell is the local
///   escape key, never a remote disconnect.
///
/// Like `tape_load_net`, this answers any `CHAT_TAG_DISCOVER_REQUEST` it
/// sees the whole time it runs -- both before a client has connected and
/// during an active session -- so simply leaving `term net` running is
/// enough to make this host discoverable, exactly as `chat`/`tape load
/// net` already do. While waiting, probes are answered on every
/// initialized NIC, not only the active terminal NIC.
///
/// `shell::run` calls this once automatically at every boot, before the
/// ordinary interactive prompt, so every fos machine is reachable over
/// `term net` by default with no manual step -- this matters most for a
/// machine with no attached screen/keyboard. A local operator watching a
/// real screen presses F12 (or F4/F6/F7/F8/F10) to skip the wait and get
/// the interactive prompt immediately; after that local escape, one minute
/// without local keyboard or serial activity re-enters the wait. There is
/// deliberately no serial equivalent for the escape key, since reading one
/// byte here to mean "give up" would silently eat the first character of
/// whatever a serial operator was about to type.
fn term_net(info: &BootInfo) {
    if nic_or_report().is_none() {
        return;
    }
    if (0..nic::count()).all(term_net_denied) {
        println!("term net: denied on every interface by the settings file.");
        return;
    }
    println!("Waiting for a remote terminal client to connect; press F12");
    println!("(or F4/F6/F7/F8/F10) to give up.");

    let mut recv_buffer = [0u8; nic::MAX_FRAME_LEN];
    'outer: loop {
        // Re-checked every time this outer loop starts a fresh wait for a
        // HELLO, not just once at function entry, in case every NIC has
        // somehow gone away since the last session ended.
        if nic_or_report().is_none() {
            return;
        }
        let interface_count = nic::count();

        // A `HELLO` is accepted on *any* interface, not just whichever one
        // happens to be `nic::active_index()` -- unlike a bare `chat`/`nic
        // discover` box, a machine running as a router (see
        // `tapes/router/`) has every one of its NICs doing real work at
        // once, and a remote operator most naturally connects through
        // whichever one is actually reachable from where they are (often a
        // downstream/inbound LAN port, not the uplink NIC that happens to
        // be "active" for ordinary local commands like `nic send`). This
        // mirrors the `CHAT_TAG_DISCOVER_REQUEST` arm just below, which has
        // never had this restriction. The interface the HELLO actually
        // arrived on becomes this session's own NIC for its whole
        // lifetime (see `ACTIVE_TERM_INDEX`), not necessarily the active
        // one.
        let (peer, session_index) = loop {
            if let Some(keyboard::KeyEvent::Escape) = keyboard::poll_term_event() {
                println!("Gave up waiting for a remote terminal client.");
                return;
            }
            // Lets the active NIC's watchdog recover a latched fault even
            // while sitting idle waiting for a client -- previously this
            // wait had no watchdog check at all, so a fault occurring
            // right at boot (before any session ever connects) sat
            // unrecovered indefinitely.
            poll_background_work(nic::get().ok());
            let mut connected_peer = None;
            let mut received_any = false;
            for index in 0..interface_count {
                let Some(interface) = nic::get_at(index) else {
                    continue;
                };
                let Some(received) = interface.receive(&mut recv_buffer) else {
                    continue;
                };
                received_any = true;
                let frame = &recv_buffer[..received];
                if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                    continue;
                }
                let peer_mac: [u8; 6] = frame[6..12].try_into().unwrap();
                match frame[14] {
                    // A denied NIC (settings file) stays silent: no session,
                    // no reply that confirms a terminal service exists.
                    TERM_TAG_HELLO if term_net_denied(index) => {}
                    TERM_TAG_HELLO => {
                        connected_peer = Some((peer_mac, index));
                        break;
                    }
                    CHAT_TAG_DISCOVER_REQUEST => {
                        let interface_mac = interface.mac_address();
                        remember_peer(peer_mac, trim_trailing_zero(&frame[15..]));
                        send_chat_control(
                            interface,
                            &peer_mac,
                            &interface_mac,
                            CHAT_TAG_DISCOVER_REPLY,
                            index,
                        );
                        println!(
                            "\n(replied to a discovery probe from {} on NIC {})",
                            Mac(&peer_mac),
                            index + 1
                        );
                    }
                    tag if dispatch_netfs_serve_frame(
                        info,
                        index,
                        interface,
                        peer_mac,
                        interface.mac_address(),
                        tag,
                        &frame[15..],
                    ) => {}
                    tag if dispatch_kup_frame(
                        info,
                        index,
                        interface,
                        peer_mac,
                        interface.mac_address(),
                        tag,
                        &frame[15..],
                    ) => {}
                    _ => {}
                }
            }
            if let Some(found) = connected_peer {
                break found;
            }
            if !received_any {
                idle_wait();
            }
        };

        let Some(nic) = nic::get_at(session_index) else {
            // Can't happen in practice -- a frame was just received on
            // this very index above, and NICs never disappear once
            // discovered at boot -- but there is no reasonable session to
            // run without it, so just go back to waiting.
            continue 'outer;
        };
        let source = nic.mac_address();

        println!(
            "Remote terminal connected from {} on NIC {}. Local input is",
            Mac(&peer),
            session_index + 1
        );
        println!("disabled; F12 (or F4/F6/F7/F8/F10) disconnects it.");
        console::enable_relay();
        // Lets `poll_remote_tape_abort` recognize this peer's own frames
        // while a nested blocking command (like `tape_run`) has this
        // loop's own `nic.receive` paused -- cleared again below once
        // this session ends, so a tape started from the local shell (no
        // active peer) never mistakes leftover state for one.
        unsafe {
            ACTIVE_TERM_PEER = Some(peer);
            ACTIVE_TERM_SOURCE = Some(source);
            ACTIVE_TERM_INDEX = Some(session_index);
            PENDING_REMOTE_BYE = false;
            TERM_DEFERRED_INPUT_HEAD = 0;
            TERM_DEFERRED_INPUT_TAIL = 0;
            TERM_DEFERRED_INPUT_COUNT = 0;
            TERM_DEFERRED_INPUT_OVERFLOWED = false;
        }
        let mut editor = LineEditor::new();
        prompt();

        // Tracks *why* the session below ended: only a local escape key
        // press should ever fall through to the local shell (see this
        // function's own doc comment) -- a remote `TERM_TAG_BYE` instead
        // loops back to waiting for a new client.
        let mut local_escape = false;
        'session: loop {
            // Set by `poll_remote_tape_abort` if a nested tape's own
            // network poll already consumed this peer's `TERM_TAG_BYE`
            // while this loop's own `nic.receive` below was paused: the
            // real disconnect already happened, so there is no second
            // `TERM_TAG_BYE` left to receive here -- this is exactly as
            // if the `TERM_TAG_BYE` arm just below had matched.
            if unsafe { PENDING_REMOTE_BYE } {
                break 'session;
            }

            drain_deferred_term_input(&mut editor, info);

            // Also drains the print-relay queue and lets the NIC
            // watchdog notice/recover a latched fault -- previously only
            // checked when a human ran `nic`/`nic list`/`nic watchdog`
            // afterward, so a fault occurring mid-session sat
            // unrecovered until this loop happened to return.
            poll_background_work(Some(nic));

            if let Some(keyboard::KeyEvent::Escape) = keyboard::poll_term_event() {
                send_term_control(nic, &peer, &source, TERM_TAG_BYE);
                local_escape = true;
                break 'session;
            }

            let Some(received) = nic.receive(&mut recv_buffer) else {
                idle_wait();
                continue;
            };
            {
                let frame = &recv_buffer[..received];
                if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                    continue;
                }
                let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
                if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
                    remember_peer(frame_source, trim_trailing_zero(&frame[15..]));
                    send_chat_control(
                        nic,
                        &frame_source,
                        &source,
                        CHAT_TAG_DISCOVER_REPLY,
                        session_index,
                    );
                    continue;
                }
                if dispatch_netfs_serve_frame(
                    info,
                    session_index,
                    nic,
                    frame_source,
                    source,
                    frame[14],
                    &frame[15..],
                ) {
                    continue;
                }
                if dispatch_kup_frame(
                    info,
                    session_index,
                    nic,
                    frame_source,
                    source,
                    frame[14],
                    &frame[15..],
                ) {
                    continue;
                }
                if frame_source != peer {
                    continue;
                }
                match frame[14] {
                    TERM_TAG_BYE => break 'session,
                    TERM_TAG_DATA => {
                        let payload = &frame[15..];
                        let end = payload
                            .iter()
                            .rposition(|&byte| byte != 0)
                            .map_or(0, |index| index + 1);
                        for &byte in &payload[..end] {
                            editor.feed(byte, info);
                        }
                    }
                    _ => {}
                }
            }
        }

        console::disable_relay();
        unsafe {
            ACTIVE_TERM_PEER = None;
            ACTIVE_TERM_SOURCE = None;
            ACTIVE_TERM_INDEX = None;
        }
        if local_escape {
            println!("\nRemote terminal disconnected; back at the local fos shell.");
            return;
        }
        println!(
            "\nRemote terminal disconnected; waiting for a new one to connect (F12 (or F4/F6/F7/F8/F10) to stop and use the local shell)."
        );
    }
}

/// The peer and source MAC of the `term_net` session currently driving
/// this shell, if any -- `None` whenever no remote terminal is connected
/// (including the entire time this function is running from a local
/// keyboard). Set and cleared by `term_net` itself around its per-peer
/// session loop; read by `poll_remote_tape_abort` so a nested blocking
/// command like `tape_run`, called deep inside that loop's own
/// `editor.feed`/`execute` call chain with no arguments of its own to
/// thread this through, can still find out who to listen for.
static mut ACTIVE_TERM_PEER: Option<[u8; 6]> = None;
static mut ACTIVE_TERM_SOURCE: Option<[u8; 6]> = None;

/// The interface index actually carrying the active `term_net` session, set
/// and cleared alongside `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`. A `HELLO`
/// is accepted on any NIC (see `term_net`'s own doc comment), so this is not
/// necessarily `nic::active_index()` -- a router's remote operator is
/// commonly connected through a downstream/inbound NIC while a different
/// one serves as the uplink. Every nested-tape polling helper below that
/// needs to talk to *this session's* peer (as opposed to whichever NIC a
/// foreground command like `nic sniff` happens to be using) must resolve
/// the interface through this index, not through `nic::get()`.
static mut ACTIVE_TERM_INDEX: Option<usize> = None;

/// `nic::get_at` applied to `ACTIVE_TERM_INDEX`, or `None` if no `term_net`
/// session is currently active.
fn active_term_nic() -> Option<&'static nic::Nic> {
    unsafe { ACTIVE_TERM_INDEX }.and_then(nic::get_at)
}

/// Set for as long as `term_printer`'s interactive session (or a
/// deliberately nested `execute` call it makes for a committed line) is
/// running, so `execute`'s own `help` arm can refuse to run (see its doc
/// comment) rather than flooding the connected printer with its listing.
/// Never set by `print_command`'s one-shot `print run CMD`, which is the
/// intended way to print `help` deliberately, once.
static mut PRINTER_SESSION_ACTIVE: bool = false;

/// The active printer's base I/O port and its ongoing fault state, for
/// as long as a print job that might run a nested tape (`print_run`'s
/// own-relay path, or `term_printer`'s Enter-committed `execute` call)
/// is in progress -- mirroring `ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`'s
/// exact reasoning: `relay_pending_print_output`, called deep inside a
/// nested tape's own `should_abort`/`read_key` polling with no argument
/// of its own to thread a printer through, needs some way to find one.
/// `ACTIVE_PRINT_FAULT` doubles as the single, continuous "last reported
/// fault" for the whole session (shared with any direct
/// `write_to_printer_with_retry`/`drain_print_relay` calls the owning
/// caller makes itself), so a fault surfaced live during a running tape
/// is not re-announced the moment that tape returns and normal draining
/// resumes.
static mut ACTIVE_PRINT_BASE: Option<u16> = None;
static mut ACTIVE_PRINT_FAULT: Option<parallel::PrinterFault> = None;

/// Drains whatever the print-relay queue currently holds to the printer
/// named by `ACTIVE_PRINT_BASE`, without blocking on a fault -- called
/// from inside a nested tape's own non-blocking `should_abort`/`read_key`
/// polling (see `combined_tape_should_abort`/`poll_remote_tape_input`) so
/// a long-running interactive tape's printed output (e.g. `tapes/calc`'s
/// REPL) reaches paper as it is produced, instead of arriving in one
/// burst only once the tape returns -- mirroring
/// `relay_pending_tape_output`'s equivalent fix for `term_net`. Unlike
/// `drain_print_relay` (used once a command finishes, where blocking
/// retry-until-clear is exactly the desired "pause, don't abort"
/// behavior), a fault here just stops this poll's draining early --
/// blocking here would freeze the *tape* itself, not just its printer
/// output -- leaving the remainder queued for the next poll.
fn relay_pending_print_output() {
    if !console::print_relay_active() {
        return;
    }
    let Some(base) = (unsafe { ACTIVE_PRINT_BASE }) else {
        return;
    };
    let printer = parallel::Printer::new(base);
    let mut chunk = [0u8; 64];
    let queued = console::peek_print_relay(&mut chunk);
    if queued == 0 {
        return;
    }
    let mut written = 0;
    while written < queued {
        match printer.write_byte(chunk[written]) {
            Ok(()) => {
                let had_fault = matches!(unsafe { ACTIVE_PRINT_FAULT }, Some(_));
                if had_fault {
                    unsafe { ACTIVE_PRINT_FAULT = None };
                    println!("Printer recovered; resuming.");
                }
                written += 1;
            }
            Err(fault) => {
                if unsafe { ACTIVE_PRINT_FAULT != Some(fault) } {
                    println!("{}", printer_fault_message(fault));
                    unsafe { ACTIVE_PRINT_FAULT = Some(fault) };
                }
                break;
            }
        }
    }
    console::discard_print_relay(written);
}

/// Set by `poll_remote_tape_abort` when it consumes the connected peer's
/// own `TERM_TAG_BYE` while `term_net`'s own session loop was paused
/// inside a nested blocking command; checked (and reset by starting a
/// new session) at the top of that loop so a disconnect noticed this way
/// is not lost, since the frame carrying it can only ever be received
/// once.
static mut PENDING_REMOTE_BYE: bool = false;

/// `term net`'s editor is suspended while a nested command owns NIC polling.
/// Retain complete data frames from its active peer here so commands sent
/// during (for example) `tape load net` run after that command returns.
/// Four protocol-sized frames cover the expected short command burst without
/// adding a heap or letting a stalled client consume unbounded kernel memory.
const TERM_DEFERRED_INPUT_FRAME_COUNT: usize = 4;
static mut TERM_DEFERRED_INPUT_FRAMES: [[u8; TERM_DATA_CAPACITY];
    TERM_DEFERRED_INPUT_FRAME_COUNT] = [[0; TERM_DATA_CAPACITY];
    TERM_DEFERRED_INPUT_FRAME_COUNT];
static mut TERM_DEFERRED_INPUT_LENS: [usize; TERM_DEFERRED_INPUT_FRAME_COUNT] =
    [0; TERM_DEFERRED_INPUT_FRAME_COUNT];
static mut TERM_DEFERRED_INPUT_HEAD: usize = 0;
static mut TERM_DEFERRED_INPUT_TAIL: usize = 0;
static mut TERM_DEFERRED_INPUT_COUNT: usize = 0;
static mut TERM_DEFERRED_INPUT_OVERFLOWED: bool = false;

/// For a blocking command that received `frame` itself: does it tell this
/// command to stop? That is the active `term net` peer's disconnect
/// (`TERM_TAG_BYE`, which also sets `PENDING_REMOTE_BYE` so the session ends
/// once the command returns) or its escape byte inside a data frame -- the
/// remote equivalent of pressing F12 locally. Without this a command like
/// `nic sniff` or `tape load net` started from a remote session consumed the
/// session's own frames and could only be ended at the machine's keyboard.
fn remote_stop_frame(frame: &[u8]) -> bool {
    if !console::relay_enabled()
        || frame.len() < 15
        || frame[12..14] != CHAT_ETHERTYPE
        || frame[6..12] != unsafe { ACTIVE_TERM_PEER }.unwrap_or([0; 6])
    {
        return false;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            true
        }
        TERM_TAG_DATA => is_remote_abort(&frame[15..]),
        _ => false,
    }
}

/// For a blocking command that polls only the NIC with index `polled`: if the
/// `term net` session that started it lives on a *different* NIC, nothing the
/// command receives could ever be that session's disconnect or escape, so poll
/// the session NIC here (`poll_remote_tape_abort`, which also relays output
/// live). True means the command should stop.
fn remote_stop_via_session_nic(polled: usize) -> bool {
    match unsafe { ACTIVE_TERM_INDEX } {
        Some(session) if console::relay_enabled() && session != polled => poll_remote_tape_abort(),
        _ => false,
    }
}

/// Saves one active-peer terminal frame for the suspended `LineEditor`.
/// Oversized frames or a full queue drop the whole newest frame (never a
/// partial command) and arrange one explicit diagnostic once the editor
/// resumes. `TERM_TAG_BYE` remains a session event, never editor input.
fn defer_active_term_input(frame: &[u8]) -> bool {
    if !console::relay_enabled()
        || frame.len() < 15
        || frame[12..14] != CHAT_ETHERTYPE
        || frame[6..12] != unsafe { ACTIVE_TERM_PEER }.unwrap_or([0; 6])
    {
        return false;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            true
        }
        TERM_TAG_DATA => {
            let payload = &frame[15..];
            let length = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            unsafe {
                if length > TERM_DATA_CAPACITY
                    || TERM_DEFERRED_INPUT_COUNT == TERM_DEFERRED_INPUT_FRAME_COUNT
                {
                    TERM_DEFERRED_INPUT_OVERFLOWED = true;
                } else {
                    TERM_DEFERRED_INPUT_FRAMES[TERM_DEFERRED_INPUT_TAIL][..length]
                        .copy_from_slice(&payload[..length]);
                    TERM_DEFERRED_INPUT_LENS[TERM_DEFERRED_INPUT_TAIL] = length;
                    TERM_DEFERRED_INPUT_TAIL =
                        (TERM_DEFERRED_INPUT_TAIL + 1) % TERM_DEFERRED_INPUT_FRAME_COUNT;
                    TERM_DEFERRED_INPUT_COUNT += 1;
                }
            }
            true
        }
        _ => false,
    }
}

/// Replays deferred complete terminal frames only after the blocking command
/// that consumed them has returned to `term_net`.
fn drain_deferred_term_input(editor: &mut LineEditor, info: &BootInfo) {
    if unsafe { TERM_DEFERRED_INPUT_OVERFLOWED } {
        unsafe { TERM_DEFERRED_INPUT_OVERFLOWED = false };
        println!("\nRemote terminal input overflowed while a blocking command");
        println!("owned the NIC; dropped newest frame(s).");
    }
    loop {
        let mut frame = [0u8; TERM_DATA_CAPACITY];
        let length = unsafe {
            if TERM_DEFERRED_INPUT_COUNT == 0 {
                return;
            }
            let head = TERM_DEFERRED_INPUT_HEAD;
            let length = TERM_DEFERRED_INPUT_LENS[head];
            frame[..length].copy_from_slice(&TERM_DEFERRED_INPUT_FRAMES[head][..length]);
            TERM_DEFERRED_INPUT_LENS[head] = 0;
            TERM_DEFERRED_INPUT_HEAD = (head + 1) % TERM_DEFERRED_INPUT_FRAME_COUNT;
            TERM_DEFERRED_INPUT_COUNT -= 1;
            length
        };
        for &byte in &frame[..length] {
            editor.feed(byte, info);
        }
    }
}

/// The byte a remote `term net` peer can send inside a `TERM_TAG_DATA`
/// frame to ask a currently running blocking command (right now, only
/// `tape_run`'s loop) to stop -- the network equivalent of the local
/// "give up" keys (F12/F4/F6/F7/F8/F10) `keyboard::ESCAPE_SCANCODES`
/// recognizes. Plain ASCII Escape was picked because it needs no
/// translation on the client side: `tools/term_net.py --interactive`
/// already forwards a raw Escape keypress as exactly this byte, with no
/// local echo or interpretation of its own.
const REMOTE_ABORT_BYTE: u8 = 0x1b;

/// True when a `TERM_TAG_DATA` payload is the remote escape: a lone `0x1b`
/// (ignoring Ethernet's trailing zero padding). An Escape that begins a longer
/// payload is a terminal escape *sequence* -- `ESC [ D` for the Left arrow,
/// `ESC [ 3 ~` for Delete -- and must reach a tape as ordinary input, so
/// cursor keys can work in tapes over `term net`. `tools/term_net.py` sends
/// the Escape key as its own frame (cbreak mode, one keypress per read), which
/// is what makes this distinction possible.
fn is_remote_abort(payload: &[u8]) -> bool {
    let end = payload
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |index| index + 1);
    payload[..end] == [REMOTE_ABORT_BYTE]
}

/// Copies whatever console output is queued in `console`'s relay buffer
/// and forwards it to `peer` as one `TERM_TAG_DATA` frame -- the same
/// "send, then discard" step
/// `term_net`'s own session loop performs every iteration, factored out
/// so `poll_remote_tape_abort`/`poll_remote_tape_input` can perform it
/// too. Without this, a nested blocking command's own `putc` output
/// (which still reaches `console::write_bytes`/`relay_push` exactly like
/// any other console output) would only ever reach the relay queue, not
/// the wire, until `term_net`'s own outer loop resumed -- fine for a
/// one-shot tape whose whole output arrives in one burst once it returns
/// (see AGENTS.md's `tools/test_term_net_tape_e2e.py` notes), but wrong
/// for a tape that loops for a long time printing incrementally (an
/// interactive REPL like `tapes/calc`, or a future long-running
/// `should_abort` tape that prints progress) -- a remote operator would
/// otherwise see nothing at all until the whole thing finally exits.
/// Calling this from both polling helpers below means any `read_key`- or
/// `should_abort`-based tape gets live output relaying for free, with no
/// ABI change and no per-tape opt-in required.
fn relay_pending_tape_output(nic: &nic::Nic, peer: &[u8; 6], source: &[u8; 6]) {
    let mut chunk = [0u8; TERM_DATA_CAPACITY];
    let queued = console::peek_relay(&mut chunk);
    if queued > 0 && send_term_data(nic, peer, source, &chunk[..queued]) {
        console::discard_relay(queued);
    }
}

/// The one shared "background poll" hook: everything a long-running
/// foreground loop wants done once per iteration, regardless of which
/// command that loop belongs to, gathered in one place instead of each
/// loop separately remembering (or, as several did until this was added,
/// forgetting) its own subset. It:
///
/// - relays any queued printer output (`relay_pending_print_output`);
/// - relays any queued `term_net` output to the connected peer, if this
///   loop is running nested inside an active `term_net` session
///   (`ACTIVE_TERM_PEER`/`ACTIVE_TERM_SOURCE`), exactly the way
///   `poll_remote_tape_abort`/`poll_remote_tape_input` already do for a
///   nested tape -- so a long `nic sniff`/`nic discover`/`chat` typed
///   *as a command* over a remote `term net` session now streams its own
///   output live too, instead of arriving in one burst only once it
///   returns;
/// - and lets the active NIC's watchdog notice and recover from a
///   latched, documented adapter fault (see `nic.rs`'s module doc
///   comment) -- previously only ever checked when a human happened to
///   run `nic`/`nic list`/`nic watchdog` afterward, so a fault occurring
///   mid-capture, mid-chat, or mid-`term-net`-session sat unrecovered
///   until that loop returned and someone thought to check.
///
/// Cheap when idle: `watchdog()` is a handful of register/atomic reads
/// unless a fault is actually latched (see `rtl8139::Rtl8139::watchdog`),
/// and both relay checks are a single peek against an otherwise-empty
/// queue. Takes `Option<&nic::Nic>` rather than requiring one, since a
/// few callers (the tape polling callbacks) may run with no NIC at all.
fn poll_background_work(nic: Option<&nic::Nic>) {
    crate::debug::watchdog_pet();
    relay_pending_print_output();
    // Unconditional (unlike everything below, which needs a NIC): keeps
    // an active USB Audio device's playback (and, if discovered, capture)
    // stream's Frame List window filled, if `audio::initialize()` ever
    // discovered one. There is no IRQ driving either stream on its own --
    // see `uhci::service_active_audio`'s own doc comment.
    uhci::service_active_audio();
    // Resolved through `ACTIVE_TERM_INDEX`, independently of whichever NIC
    // (if any) the caller passed in for its own watchdog check below: the
    // session's peer is reachable only through the specific interface its
    // `HELLO` arrived on (see `term_net`'s own doc comment), which is not
    // necessarily this loop's own NIC (e.g. `usb play`/`usb capture` above
    // pass `None` here, and `nic sniff`/`chat` pass whichever NIC they were
    // told to operate on) -- relaying through the wrong interface would
    // simply never reach the connected peer.
    if console::relay_enabled() {
        if let (Some(peer), Some(source)) =
            (unsafe { ACTIVE_TERM_PEER }, unsafe { ACTIVE_TERM_SOURCE })
        {
            if let Some(relay_nic) = active_term_nic() {
                relay_pending_tape_output(relay_nic, &peer, &source);
            }
        }
    }
    let Some(nic) = nic else { return };
    report_nic_watchdog(nic.watchdog());
}

/// Polls the network card once for a frame from the currently connected
/// `term_net` peer, if any, on behalf of a nested blocking command (see
/// `tape_run`) whose own loop has `term_net`'s *own* session loop --
/// including its `nic.receive` -- paused for as long as it runs. Mirrors
/// that loop's own frame handling closely enough to stay a reasonable
/// network citizen even mid-tape (still answering discovery probes from
/// someone else, still noticing this peer hanging up, still relaying
/// this tape's own printed output live via `relay_pending_tape_output`).
/// Frames that belong to the tape rather than the terminal session are
/// preserved in the deferred raw-frame queue instead of being lost here.
///
/// Returns whether the caller's blocking command should stop: either the
/// peer explicitly asked to (a `TERM_TAG_DATA` frame containing
/// `REMOTE_ABORT_BYTE` as its entire payload (`is_remote_abort`) -- the frame is
/// simply dropped rather than fed to the line editor, the same way a
/// local escape key press is consumed rather than typed as a literal
/// character) or the peer hung up entirely (`TERM_TAG_BYE`, which also
/// records `PENDING_REMOTE_BYE` since this call is the only place that
/// frame will ever be received).
///
/// Also runs `poll_background_work` unconditionally first, so any
/// running tape -- not just one nested inside a `term_net` session --
/// gets live printer-output relaying and NIC watchdog recovery for the
/// duration of its own polling loop, not only once it returns.
///
/// Beyond that, this does nothing but check one flag (and so costs a
/// single extra branch) unless a `term_net` relay session is actually
/// active: a tape started from the local shell has
/// `console::relay_enabled()` false the entire time, so it never
/// touches the network card at all beyond the watchdog check above.
fn poll_remote_tape_abort() -> bool {
    // Resolved through the session's own interface, not `nic::get()`'s
    // active one -- see `ACTIVE_TERM_INDEX`'s doc comment. Falls back to
    // the active NIC (matching the old, session-agnostic behavior) so the
    // watchdog check below still runs when no `term_net` session exists at
    // all, e.g. a tape started from the local shell.
    let session_nic = active_term_nic();
    poll_background_work(session_nic.or_else(|| nic::get().ok()));
    if !console::relay_enabled() {
        return false;
    }
    let (Some(peer), Some(source)) = (unsafe { ACTIVE_TERM_PEER }, unsafe { ACTIVE_TERM_SOURCE })
    else {
        return false;
    };
    let Some(nic) = session_nic else { return false };
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let Some(received) = nic.receive(&mut buffer) else {
        return false;
    };
    let frame = &buffer[..received];
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        tape_network_frame_push(frame);
        return false;
    }
    let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
    if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
        remember_peer(frame_source, trim_trailing_zero(&frame[15..]));
        send_chat_control(
            nic,
            &frame_source,
            &source,
            CHAT_TAG_DISCOVER_REPLY,
            unsafe { ACTIVE_TERM_INDEX }.unwrap_or_else(nic::active_index),
        );
        tape_network_frame_push(frame);
        return false;
    }
    if frame_source != peer {
        tape_network_frame_push(frame);
        return false;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            true
        }
        TERM_TAG_DATA => {
            if is_remote_abort(&frame[15..]) {
                true
            } else {
                defer_active_term_input(frame);
                false
            }
        }
        _ => {
            tape_network_frame_push(frame);
            false
        }
    }
}

/// Sends `data` as one `TERM_TAG_DATA` frame to `destination`.
///
/// The caller retains the corresponding relay bytes when this returns
/// `false`, allowing a later foreground iteration to retry them in order.
fn send_term_data(
    nic: &nic::Nic,
    destination: &[u8; 6],
    source: &[u8; 6],
    data: &[u8],
) -> bool {
    let mut frame = [0u8; 15 + TERM_DATA_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = TERM_TAG_DATA;
    frame[15..15 + data.len()].copy_from_slice(data);
    nic.send(&frame[..15 + data.len()])
}

/// Sends a zero-length `CHAT_ETHERTYPE` control frame with a `TERM_TAG_*`
/// tag (`HELLO` or `BYE`), mirroring `send_chat_control`.
fn send_term_control(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], tag: u8) {
    let mut frame = [0u8; 15];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    nic.send(&frame);
}

/// reserved by IEEE 802 for exactly this purpose: local, non-routable
/// protocol experiments that will never collide with a real assigned
/// ethertype), used to mark `nic send`'s frames as fos's own test traffic
/// rather than anything meaningful to a real network.
const TEST_ETHERTYPE: [u8; 2] = [0x88, 0xb5];
const TEST_PAYLOAD: &[u8] = b"fos raw ethernet test frame";

/// `nic` (no arguments) reports the active interface; `nic list` shows every
/// initialized interface; and `nic use N` selects one by its 1-based list
/// number. `nic send` transmits one broadcast test frame (see
/// `TEST_ETHERTYPE`); `nic recv` polls briefly for one incoming frame and
/// hex-dumps it if one arrives; `nic sniff` runs a continuous
/// promiscuous-mode capture, printing every frame seen until F12 (or
/// F4/F6/F7/F8/F10) stops it; `nic discover` broadcasts a `chat` discovery
/// probe through every initialized interface and lists whoever answers;
/// `nic peers` shows the peer list discovered so far without sending
/// anything. These shell subcommands intentionally stay at the raw-Ethernet
/// layer; ARP/IPv4/UDP/TCP/DHCP live in networking tapes and the shared
/// `tapes/tape-net` library instead.
#[inline(never)]
fn nic_command(arguments: &[u8]) {
    let (subcommand, remainder) = take_word(arguments);
    match subcommand {
        b"" if remainder.is_empty() => nic_status(),
        b"list" if remainder.is_empty() => nic_list(),
        b"use" => nic_use(remainder),
        b"send" if remainder.is_empty() => nic_send(),
        b"recv" if remainder.is_empty() => nic_recv(),
        b"sniff" if remainder.is_empty() => nic_sniff(),
        b"discover" if remainder.is_empty() => nic_discover(),
        b"peers" if remainder.is_empty() => print_peers(),
        b"watchdog" if remainder.is_empty() => nic_watchdog(),
        b"regs" => nic_regs(remainder),
        b"map" if remainder.is_empty() => nic_map(),
        _ => println!("Usage: nic [list|use N|send|recv|sniff|discover|peers|watchdog|regs [N]|map]"),
    }
}

/// Ethertype and magic of the probes `nic map` sends (IEEE 802 local
/// experimental range, one above `TEST_ETHERTYPE`'s neighbours).
const MAP_ETHERTYPE: [u8; 2] = [0x88, 0xb9];
const MAP_MAGIC: &[u8; 6] = b"FOSMAP";
const MAP_MAX_NICS: usize = 16;
const MAP_MAX_FOREIGN: usize = 4;
/// How long the probes of one NIC are listened for, in 100 Hz ticks.
const MAP_LISTEN_TICKS: u64 = 4;

/// `nic map`: which of this machine's NICs have a link, and which hear which.
///
/// The link line comes from the driver where it can tell (an `STATUS`
/// register or the 3Com media status). The "sees" line comes from the wire:
/// every NIC in turn broadcasts a few tiny probe frames carrying its own number
/// while all the others listen, so two NICs on one cable -- or on one switch --
/// find each other regardless of what the drivers can report. Frames from
/// anything else are counted as "other hosts": a NIC that sees exactly one
/// other of ours and nothing else is very likely on a direct cable. Takes about
/// a second; run it before a test so you know which port is which.
fn nic_map() {
    let count = nic::count().min(MAP_MAX_NICS);
    if count == 0 {
        println!("No Ethernet controller found.");
        return;
    }
    let mut macs = [[0u8; 6]; MAP_MAX_NICS];
    for index in 0..count {
        if let Some(nic) = nic::get_at(index) {
            macs[index] = nic.mac_address();
        }
    }
    // heard[a][b]: NIC b received a probe sent by NIC a.
    let mut heard = [[false; MAP_MAX_NICS]; MAP_MAX_NICS];
    let mut foreign = [[[0u8; 6]; MAP_MAX_FOREIGN]; MAP_MAX_NICS];
    let mut foreign_count = [0usize; MAP_MAX_NICS];
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];

    println!("Probing {count} NIC(s) for links and cable connections...");
    // Poll every NIC for `ticks` ticks, noting probes and foreign sources.
    let mut listen = |ticks: u64,
                      heard: &mut [[bool; MAP_MAX_NICS]; MAP_MAX_NICS],
                      foreign: &mut [[[u8; 6]; MAP_MAX_FOREIGN]; MAP_MAX_NICS],
                      foreign_count: &mut [usize; MAP_MAX_NICS]| {
        let start = interrupts::ticks();
        loop {
            let mut any = false;
            for listener in 0..count {
                let Some(nic) = nic::get_at(listener) else { continue };
                while let Some(length) = nic.receive(&mut buffer) {
                    any = true;
                    if length < 14 {
                        continue;
                    }
                    let source: [u8; 6] = buffer[6..12].try_into().unwrap();
                    if buffer[12..14] == MAP_ETHERTYPE
                        && length >= 21
                        && &buffer[14..20] == MAP_MAGIC
                    {
                        let sender = buffer[20] as usize;
                        if sender < count && sender != listener && macs[sender] == source {
                            heard[sender][listener] = true;
                        }
                        continue;
                    }
                    if macs[..count].contains(&source) {
                        continue; // one of our own NICs: not another host
                    }
                    let known = foreign[listener][..foreign_count[listener]].contains(&source);
                    if !known && foreign_count[listener] < MAP_MAX_FOREIGN {
                        foreign[listener][foreign_count[listener]] = source;
                        foreign_count[listener] += 1;
                    } else if !known {
                        foreign_count[listener] += 1; // more than we list: keep counting
                    }
                }
            }
            crate::debug::watchdog_pet();
            if interrupts::ticks().wrapping_sub(start) >= ticks && !any {
                break;
            }
        }
    };

    // A card that has only just come up may still be negotiating its link, so
    // probes sent in the first moments are lost: if a whole round hears
    // nothing at all, wait a second and try again (up to three rounds).
    for round in 0..3 {
        if round > 0 {
            println!("Nothing heard yet; the links may still be coming up. Retrying...");
            let start = interrupts::ticks();
            while interrupts::ticks().wrapping_sub(start) < 100 {
                crate::debug::watchdog_pet();
                core::hint::spin_loop();
            }
        }
        for sender in 0..count {
            let Some(nic) = nic::get_at(sender) else { continue };
            for _ in 0..3 {
                let mut probe = [0u8; 60];
                probe[0..6].fill(0xff);
                probe[6..12].copy_from_slice(&macs[sender]);
                probe[12..14].copy_from_slice(&MAP_ETHERTYPE);
                probe[14..20].copy_from_slice(MAP_MAGIC);
                probe[20] = sender as u8;
                let _ = nic.send(&probe);
                listen(MAP_LISTEN_TICKS, &mut heard, &mut foreign, &mut foreign_count);
            }
        }
        listen(10, &mut heard, &mut foreign, &mut foreign_count);
        let heard_anything = heard.iter().any(|row| row.iter().any(|&seen| seen))
            || foreign_count.iter().any(|&seen| seen > 0);
        if heard_anything {
            break;
        }
    }

    for index in 0..count {
        let Some(nic) = nic::get_at(index) else { continue };
        let mac = macs[index];
        let link = match (nic.link_status(), nic.link_up()) {
            (Some((speed, full)), _) => {
                let mut text = [0u8; 24];
                let mut writer = StackText::new(&mut text);
                let _ = core::fmt::Write::write_fmt(
                    &mut writer,
                    format_args!("{speed}M {}", if full { "full" } else { "half" }),
                );
                (writer.as_str().len(), text)
            }
            (None, _) if nic.link_status_is_reliable() => {
                let mut text = [0u8; 24];
                text[..4].copy_from_slice(b"down");
                (4, text)
            }
            (None, Some(true)) => {
                let mut text = [0u8; 24];
                text[..2].copy_from_slice(b"up");
                (2, text)
            }
            (None, Some(false)) => {
                let mut text = [0u8; 24];
                text[..4].copy_from_slice(b"down");
                (4, text)
            }
            (None, None) => {
                let mut text = [0u8; 24];
                text[..7].copy_from_slice(b"unknown");
                (7, text)
            }
        };
        println!(
            "NIC {}: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} {} link {}",
            index + 1,
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
            if index == nic::active_index() { "(active)" } else { "" },
            core::str::from_utf8(&link.1[..link.0]).unwrap_or("?"),
        );
        let mut line = [0u8; 80];
        let mut writer = StackText::new(&mut line);
        let mut any = false;
        for other in 0..count {
            if other != index && (heard[other][index] || heard[index][other]) {
                let one_way = heard[other][index] != heard[index][other];
                let _ = core::fmt::Write::write_fmt(
                    &mut writer,
                    format_args!("{}NIC {}{}", if any { ", " } else { "" }, other + 1,
                                 if one_way { " (one way)" } else { "" }),
                );
                any = true;
            }
        }
        let others = foreign_count[index];
        let connected_to_us = (0..count).filter(|&o| o != index && (heard[o][index] || heard[index][o])).count();
        if any {
            let direct = connected_to_us == 1 && others == 0;
            let _ = core::fmt::Write::write_fmt(
                &mut writer,
                format_args!(" -- {}", if direct { "direct cable" } else { "same network" }),
            );
        }
        if others > 0 {
            let _ = core::fmt::Write::write_fmt(
                &mut writer,
                format_args!("{}{others} other host(s)", if any { "; " } else { "" }),
            );
        }
        if !any && others == 0 {
            println!("    sees nothing (no cable, or only silent peers)");
        } else {
            println!("    sees {}", writer.as_str());
        }
    }
}

/// A tiny `fmt::Write` sink over a stack buffer (no heap in this kernel).
struct StackText<'a> {
    buffer: &'a mut [u8],
    length: usize,
}

impl<'a> StackText<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer, length: 0 }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buffer[..self.length]).unwrap_or("")
    }
}

impl core::fmt::Write for StackText<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let room = self.buffer.len() - self.length;
        let take = bytes.len().min(room);
        self.buffer[self.length..self.length + take].copy_from_slice(&bytes[..take]);
        self.length += take;
        Ok(())
    }
}

fn nic_status() {
    match nic::get() {
        Ok(nic) => {
            report_nic_watchdog(nic.watchdog());
            let mac = nic.mac_address();
            println!(
                "{} found: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                nic.driver_name(),
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );
            if let Some(link_up) = nic.link_up() {
                println!(
                    "3Com media link: {} (raw link-beat status; check cable/switch if down).",
                    if link_up { "up" } else { "down" }
                );
            }
            print_nic_interrupt_status(nic);
            print_nic_health(nic);
            let count = nic::count();
            if count > 1 {
                println!(
                    "Active interface: {} of {}. Use 'nic list' or 'nic use N' to select another.",
                    nic::active_index() + 1,
                    count
                );
            }
        }
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.")
        }
        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
        }
    }
}

fn nic_list() {
    let count = nic::count();
    if count == 0 {
        nic_status();
        return;
    }
    for index in 0..count {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let mac = nic.mac_address();
        println!(
            "{}NIC {}: {} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            if index == nic::active_index() {
                '*'
            } else {
                ' '
            },
            index + 1,
            nic.driver_name(),
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
        let mut hostname_buf = [0u8; INTERFACE_HOSTNAME_CAPACITY];
        let hostname_len = interface_hostname(index, &mut hostname_buf);
        if hostname_len > 0 {
            let name = core::str::from_utf8(&hostname_buf[..hostname_len])
                .unwrap_or("<non-UTF-8 hostname>");
            println!("  hostname: {name}");
        }
        report_nic_watchdog(nic.watchdog());
        print_nic_interrupt_status(nic);
        print_nic_health(nic);
    }
}

fn nic_watchdog() {
    match nic::watchdog() {
        Ok(result) => report_nic_watchdog(result),
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.")
        }

        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
        }
    }
}

/// Recoveries seen since the last message printed, and when that was. A card
/// that keeps faulting under load (a flooded receive path, a bad cable) used
/// to print two lines for every recovery and bury the terminal -- and, over
/// `term net`, delay the very traffic being recovered. The first one is
/// reported at once, later ones are counted and summarized at most every
/// `NIC_WATCHDOG_REPORT_TICKS` (5 s).
static mut NIC_WATCHDOG_SUPPRESSED: u32 = 0;
static mut NIC_WATCHDOG_LAST_REPORT: u64 = 0;
const NIC_WATCHDOG_REPORT_TICKS: u64 = 500;

fn report_nic_watchdog(result: nic::WatchdogResult) {
    if matches!(result, nic::WatchdogResult::Healthy) {
        return;
    }
    let now = interrupts::ticks();
    let (quiet, suppressed) = unsafe {
        let quiet = NIC_WATCHDOG_LAST_REPORT != 0
            && now.wrapping_sub(NIC_WATCHDOG_LAST_REPORT) < NIC_WATCHDOG_REPORT_TICKS;
        if quiet {
            NIC_WATCHDOG_SUPPRESSED += 1;
            (true, 0)
        } else {
            NIC_WATCHDOG_LAST_REPORT = now;
            let suppressed = NIC_WATCHDOG_SUPPRESSED;
            NIC_WATCHDOG_SUPPRESSED = 0;
            (false, suppressed)
        }
    };
    if quiet {
        return;
    }
    match result {
        nic::WatchdogResult::Healthy => {}
        nic::WatchdogResult::Recovered => {
            println!("NIC watchdog: recovered a documented adapter fault in");
            println!("foreground; polling remains available.");
        }
        nic::WatchdogResult::Failed => {
            println!("NIC watchdog: bounded adapter reset/rebuild failed; polling");
            println!("remains available, but this card may need a reset.");
        }
    }
    if suppressed > 0 {
        println!("({suppressed} more since the last message.)");
    }
}

fn print_nic_health(nic: &nic::Nic) {
    if let Some((speed, full_duplex)) = nic.link_status() {
        println!(
            "Link: {} Mb/s {}.",
            speed,
            if full_duplex { "full duplex" } else { "half duplex" }
        );
    } else if nic.link_status_is_reliable() {
        println!("Link: down (no carrier).");
    }
    let health = nic.health();
    println!(
        "NIC health: Rx {} frame(s), Tx {} frame(s), {} recovery/recoveries,",
        health.rx_progress, health.tx_progress, health.recoveries,
    );
    println!(
        "{} failed; {}.",
        health.recovery_failures,
        if health.recovery_pending {
            "foreground recovery pending"
        } else {
            "no recovery pending"
        }
    );
    if health.rx_overruns > 0 {
        println!(
            "{} receive overrun event(s): frames were lost while busy.",
            health.rx_overruns
        );
    }
}

fn print_nic_interrupt_status(nic: &nic::Nic) {
    let stats = nic.interrupt_stats();
    if let Some(vector) = nic.msi_vector() {
        println!(
            "NIC MSI vector 0x{vector:02x}: {} notification(s), {} device error notification(s),",
            stats.count, stats.errors,
        );
        println!(
            "foreground work {}.",
            if stats.pending { "pending" } else { "not pending" }
        );
        return;
    }
    match stats.irq {
        Some(irq) if interrupts::pci_irq_registered(irq) => {
            if let nic::Nic::ThreeCom(card) = nic {
                println!("3Com status register 0x{:04x} (bit 0 = IntLatch).", card.raw_status());
            }
            println!(
                "NIC IRQ{irq}: {} notification(s), {} device error notification(s),",
                stats.count, stats.errors,
            );
            println!(
                "foreground work {}.",
                if stats.pending { "pending" } else { "not pending" }
            );
        }
        Some(irq) => println!(
            "NIC IRQ{irq}: legacy line is unavailable or reserved; polling remains active."
        ),
        None => println!("NIC has no firmware-assigned legacy IRQ; polling remains active."),
    }
}

fn nic_use(arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(index) = parse_number(value) else {
        println!("Usage: nic use N (see 'nic list'; numbering starts at 1)");
        return;
    };
    if !remainder.is_empty() || index == 0 || !nic::set_active(index as usize - 1) {
        println!("No such NIC. Use 'nic list' to see available interfaces.");
        return;
    }
    let nic = nic::get().unwrap();
    println!(
        "Active interface is now {} {}.",
        nic::active_index() + 1,
        nic.driver_name()
    );
}

fn nic_send() {
    let Some(nic) = nic_or_report() else { return };
    let source = nic.mac_address();
    let mut frame = [0u8; 14 + TEST_PAYLOAD.len()];
    frame[0..6].fill(0xff); // Broadcast destination.
    frame[6..12].copy_from_slice(&source);
    frame[12..14].copy_from_slice(&TEST_ETHERTYPE);
    frame[14..].copy_from_slice(TEST_PAYLOAD);

    if nic.send(&frame) {
        println!(
            "Sent a {}-byte broadcast test frame (ethertype 0x{:02x}{:02x}).",
            frame.len(),
            TEST_ETHERTYPE[0],
            TEST_ETHERTYPE[1]
        );
    } else {
        println!("Transmit failed (card reported an error, or timed out).");
    }
}

/// Writes `bytes` to `printer` live, one at a time, via
/// `write_to_printer_with_retry`. Returns `false` the moment the operator
/// gives up mid-fault (F12), leaving the remainder of `bytes` unsent --
/// the same "pause, don't abort, but stop entirely on a deliberate give
/// up" contract every other printer-writing path in this module follows.
fn print_bytes_live(
    printer: &parallel::Printer,
    bytes: &[u8],
    last_fault: &mut Option<parallel::PrinterFault>,
) -> bool {
    for &byte in bytes {
        if !write_to_printer_with_retry(printer, byte, last_fault) {
            return false;
        }
    }
    true
}

/// A fresh, transient `&mut` into `ACTIVE_PRINT_FAULT`, obtained via a raw
/// pointer rather than held across a call: `term_printer` fetches one
/// immediately before each individual `print_bytes_live`/
/// `drain_print_relay` call rather than caching one long-lived reference
/// across its whole session, specifically so a nested tape's own
/// `relay_pending_print_output` (touching the same static through its own
/// short-lived unsafe reads/writes while `execute()` is running) is never
/// aliased against a reference `term_printer` itself is still holding.
fn active_print_fault_mut() -> &'static mut Option<parallel::PrinterFault> {
    unsafe { &mut *core::ptr::addr_of_mut!(ACTIVE_PRINT_FAULT) }
}

/// `term printer`/`term LPTn`: an exclusive terminal mode that runs the
/// live shell against a physical printer instead of a screen or a network
/// peer, echoing true teletype-style: every typed character (including
/// the prompt and the line's own trailing newline) is written to the
/// physical printer the instant it is typed, exactly like a real
/// teletype/telex terminal, not buffered until Enter. This means F2
/// (`KeyEvent::KillLine`) can no longer act as a genuine undo -- ink
/// already on the paper cannot be erased -- so it instead just abandons
/// the local input buffer (as if the operator had never typed it) and
/// prints a fresh `"\r\n" + "fos> "` break, the same way a real teletype
/// operator would simply start a fresh line after a visible mistake
/// rather than pretend it never happened. Backspace remains screen/
/// buffer-only and is never itself sent to the printer, so backspacing
/// over an already-printed character leaves it visible on the physical
/// paper -- an intentional, documented limitation of live echo, not a
/// bug.
///
/// `last_fault` is one variable held for the entire session, threaded
/// through both the live-typed-character path and `drain_print_relay`
/// (used for each command's own captured output) below, so an ongoing
/// fault is reported once across the whole session, not re-announced the
/// moment output-draining begins right after a fault was already being
/// reported during typing.
///
/// `help` refuses to run while `PRINTER_SESSION_ACTIVE` is set (see its
/// arm in `execute`), to avoid burning a whole sheet of paper by habit;
/// `print run help` still works, as a deliberate one-shot exception.
#[inline(never)]
fn term_printer(info: &BootInfo, name: &[u8], base: u16) {
    let name = core::str::from_utf8(name).unwrap_or("?");
    println!("Entering printer terminal mode on {name}. Every keystroke is echoed to the");
    println!("printer live as you type, teletype style; F2 abandons the current line (it");
    println!("cannot un-print already-inked characters) and starts a fresh one. Backspace");
    println!("edits the line on screen only and is never itself sent to the printer.");
    println!("Commands run here print their own output too; 'help' is disabled here --");
    println!("run 'print run help' once for a paper copy instead. Press F12 (or");
    println!("F4/F6/F7/F8/F10) to leave.");

    let printer = parallel::Printer::new(base);
    unsafe {
        ACTIVE_PRINT_BASE = Some(base);
        ACTIVE_PRINT_FAULT = None;
    }
    let mut buffer = [0u8; LINE_CAPACITY];
    let mut length = 0usize;
    prompt();
    if !print_bytes_live(&printer, b"fos> ", active_print_fault_mut()) {
        println!();
        println!("Leaving printer terminal mode; back at the fos shell.");
        unsafe {
            ACTIVE_PRINT_BASE = None;
            ACTIVE_PRINT_FAULT = None;
        }
        return;
    }
    loop {
        match keyboard::poll_term_event() {
            Some(keyboard::KeyEvent::Escape) => {
                println!();
                println!("Leaving printer terminal mode; back at the fos shell.");
                unsafe {
                    ACTIVE_PRINT_BASE = None;
                    ACTIVE_PRINT_FAULT = None;
                }
                return;
            }
            Some(keyboard::KeyEvent::KillLine) => {
                while length > 0 {
                    length -= 1;
                    console::erase_last_character();
                }
                if !print_bytes_live(&printer, b"\r\nfos> ", active_print_fault_mut()) {
                    println!();
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    unsafe {
                        ACTIVE_PRINT_BASE = None;
                        ACTIVE_PRINT_FAULT = None;
                    }
                    return;
                }
            }
            Some(keyboard::KeyEvent::Char(b'\r' | b'\n')) => {
                println!();
                if !print_bytes_live(&printer, b"\r\n", active_print_fault_mut()) {
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    unsafe {
                        ACTIVE_PRINT_BASE = None;
                        ACTIVE_PRINT_FAULT = None;
                    }
                    return;
                }
                unsafe {
                    PRINTER_SESSION_ACTIVE = true;
                }
                console::enable_print_relay();
                execute(&buffer[..length], info);
                let finished = drain_print_relay(&printer, active_print_fault_mut());
                console::disable_print_relay();
                unsafe {
                    PRINTER_SESSION_ACTIVE = false;
                }
                length = 0;
                if !finished {
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    unsafe {
                        ACTIVE_PRINT_BASE = None;
                        ACTIVE_PRINT_FAULT = None;
                    }
                    return;
                }
                prompt();
                if !print_bytes_live(&printer, b"fos> ", active_print_fault_mut()) {
                    println!();
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    unsafe {
                        ACTIVE_PRINT_BASE = None;
                        ACTIVE_PRINT_FAULT = None;
                    }
                    return;
                }
            }
            Some(keyboard::KeyEvent::Char(8 | 127)) if length > 0 => {
                length -= 1;
                console::erase_last_character();
            }
            Some(keyboard::KeyEvent::Char(byte @ 0x20..=0x7e)) if length < LINE_CAPACITY => {
                buffer[length] = byte;
                length += 1;
                print!("{}", byte as char);
                if !print_bytes_live(&printer, &[byte], active_print_fault_mut()) {
                    println!();
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    unsafe {
                        ACTIVE_PRINT_BASE = None;
                        ACTIVE_PRINT_FAULT = None;
                    }
                    return;
                }
            }
            Some(keyboard::KeyEvent::Char(_)) | None => {}
        }
    }
}

/// Shared by every `nic` subcommand that needs the card itself (`send`,
/// `recv`, `sniff`): reports either failure case the same way `nic`
/// (status) does, and returns `None` so callers can just early-return.
fn nic_or_report() -> Option<&'static nic::Nic> {
    match nic::get() {
        Ok(nic) => {
            report_nic_watchdog(nic.watchdog());
            Some(nic)
        }
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.");
            None
        }
        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
            None
        }
    }
}

fn nic_recv() {
    let Some(nic) = nic_or_report() else { return };
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut attempts = 20_000_000u32;
    loop {
        if let Some(length) = nic.receive(&mut buffer) {
            let frame = &buffer[..length];
            if length >= 14 {
                println!(
                    "Received {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    frame[4],
                    frame[5],
                    frame[6],
                    frame[7],
                    frame[8],
                    frame[9],
                    frame[10],
                    frame[11],
                    frame[12],
                    frame[13]
                );
            } else {
                println!("Received {length} bytes (too short to hold an Ethernet header).");
            }
            dump_bytes(0, frame);
            return;
        }
        attempts -= 1;
        if attempts == 0 {
            println!("No frame received (timed out waiting).");
            return;
        }
    }
}

/// Puts the card into promiscuous mode (`Rtl8139::set_promiscuous`) and
/// prints every frame it captures -- addressed to this card or not --
/// until F12 (or F4/F6/F7/F8/F10) stops the capture, the same escape key `term` uses and for
/// the same reason (layout-independent, unlike a Shift/AltGr combination
/// on a Nordic keyboard). Promiscuous mode is always turned back off
/// before returning, whether the capture ran to completion or not, so a
/// later `nic send`/`nic recv` is not left seeing unrelated traffic.
fn nic_sniff() {
    let Some(nic) = nic_or_report() else { return };

    println!("Enabling promiscuous mode: the card will now hand over every frame it sees");
    println!("on the wire, not just frames addressed to it (unicast/multicast/broadcast).");
    println!("Press F12 (or F4/F6/F7/F8/F10) to stop capturing and return to the fos shell.");
    nic.set_promiscuous(true);

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut captured = 0u32;
    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            break;
        }
        // Relays live output to a connected `term_net` peer (if `nic
        // sniff` was itself typed as a remote command) and lets the NIC
        // watchdog recover a latched fault -- previously this capture
        // loop did neither, so a remote session saw nothing until the
        // capture stopped, and a mid-capture fault went unrecovered.
        poll_background_work(Some(nic));
        if remote_stop_via_session_nic(nic::active_index()) {
            break;
        }
        if let Some(length) = nic.receive(&mut buffer) {
            captured += 1;
            let frame = &buffer[..length];
            if remote_stop_frame(frame) {
                break;
            }
            if length >= 14 {
                println!(
                    "#{captured} {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    frame[4],
                    frame[5],
                    frame[6],
                    frame[7],
                    frame[8],
                    frame[9],
                    frame[10],
                    frame[11],
                    frame[12],
                    frame[13]
                );
            } else {
                println!("#{captured} {length} bytes (too short to hold an Ethernet header).");
            }
            dump_bytes(0, frame);
        } else {
            idle_wait();
        }
    }

    nic.set_promiscuous(false);
    println!("\nPromiscuous mode disabled; captured {captured} frame(s). Back at the fos shell.");
}

/// A dedicated ethertype for `chat` and its discovery probes, distinct from
/// `TEST_ETHERTYPE`, so a `nic sniff` capture (or a stray `nic recv`) never
/// mistakes fos's own chat traffic for `nic send`'s test frames or vice
/// versa. `0x88b5`/`0x88b6` are the two ethertypes IEEE 802 reserves
/// together for exactly this kind of local, non-routable protocol
/// experiment.
const CHAT_ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const CHAT_TEXT_CAPACITY: usize = 200;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

/// Every `CHAT_ETHERTYPE` frame carries a one-byte message tag right after
/// the Ethernet header, ahead of any per-message data, so one ethertype can
/// carry both ordinary chat lines and the small discovery exchange below
/// without inventing a second ethertype fos has no IEEE reservation for.
const CHAT_TAG_TEXT: u8 = 0x01;
const CHAT_TAG_DISCOVER_REQUEST: u8 = 0x02;
const CHAT_TAG_DISCOVER_REPLY: u8 = 0x03;

/// `tape load`/`tape send`'s wire protocol, sharing `CHAT_ETHERTYPE` rather
/// than inventing a third reserved ethertype: a paper-tape-style transfer
/// is `TAPE_TAG_START` (payload: the transfer's total length, 4 bytes
/// little-endian) once, then any number of `TAPE_TAG_DATA` frames (payload:
/// a 4-byte little-endian sequence number, then up to `TAPE_CHUNK_LEN` raw
/// bytes), then one `TAPE_TAG_STOP` (no payload) to close it.
const TAPE_TAG_START: u8 = 0x10;
const TAPE_TAG_DATA: u8 = 0x11;
const TAPE_TAG_STOP: u8 = 0x12;

/// Data bytes carried in one `TAPE_TAG_DATA` frame: comfortably under
/// `nic::MAX_FRAME_LEN` once the Ethernet header (14 bytes), tag
/// (1 byte), and sequence number (4 bytes) are accounted for, and a round
/// enough number to reason about by hand.
const TAPE_CHUNK_LEN: usize = 1024;

/// How often (in PIT ticks, 100/second -- see `interrupts::ticks`)
/// `tape_load_net` prints a progress line while a transfer is in flight.
const TAPE_STATUS_TICKS: u64 = 200; // ~2 seconds.

/// Spacing used by the in-kernel `tape send` producer. The receiver is a
/// polling-only NIC driver with a finite ring, so waiting one PIT tick
/// between frames keeps a real RTL8139 from overrunning that ring in the
/// same way `tools/send_tape.py --delay 0.003` does. This is deliberately
/// conservative because the PIT is only 100 Hz; a future acknowledged
/// transfer protocol could use a tighter, receiver-driven window instead.
const TAPE_SEND_PACE_TICKS: u64 = 1; // ~10 ms.

/// `term net`'s wire protocol, sharing `CHAT_ETHERTYPE` rather than
/// inventing a fourth reserved ethertype: a client says hello with
/// `TERM_TAG_HELLO` (no payload) to open a session, either side then sends
/// any number of `TERM_TAG_DATA` frames (payload: raw bytes, unlike
/// `tape`'s chunks there is no sequence number -- a dropped frame here is
/// just like a dropped keystroke or a torn screen update, not corrupted
/// data, so it is not worth the extra bookkeeping), and either side may
/// close the session with `TERM_TAG_BYE` (no payload).
const TERM_TAG_HELLO: u8 = 0x20;
const TERM_TAG_DATA: u8 = 0x21;
const TERM_TAG_BYE: u8 = 0x22;

/// Data bytes carried in one `TERM_TAG_DATA` frame in either direction:
/// generous for a burst of shell output (a whole `println!` line or two)
/// while staying comfortably under `nic::MAX_FRAME_LEN` once the
/// Ethernet header (14 bytes) and tag (1 byte) are accounted for.
const TERM_DATA_CAPACITY: usize = 512;

/// NETFS: a small network filesystem layered on top of hostname-based peer
/// discovery, sharing `CHAT_ETHERTYPE` rather than inventing yet another
/// reserved ethertype. Use case: `copy hostname1:disk2/file.name disk2`
/// (or `tape load`/`tape save`/`tape run`'s `@HOST[:diskN]/NAME`) fetches or
/// stores a file on a discovered peer's own FOSFS disk over raw Ethernet.
/// GET (fetch) mirrors `TAPE_TAG_*`'s proven shape exactly -- a `_REQUEST`,
/// then one `_START` declaring the total length and a checksum, then any
/// number of sequence-numbered `_DATA` chunks, then one `_STOP` -- and PUT
/// (store) is the same shape with the length/checksum carried up front in
/// the `_REQUEST` itself (the sender already knows both before it starts),
/// closing with either a `_ACK` or a `_ERROR` instead of a bare `_STOP`, so
/// the client learns whether the remote FOSFS write actually succeeded.
/// Exactly like `TAPE_TAG_*`, there is deliberately no retransmission: a
/// gap or a checksum mismatch aborts the whole attempt rather than risking
/// a silently corrupt file.
///
/// Deliberate scope limits, not yet lifted: a peer only serves NETFS
/// requests while sitting idle at its own local prompt (`ambient_discovery_
/// poll`, the one loop this shares with ambient discovery itself) -- not
/// while it is itself running `chat`, `nic discover`, `term net`, or a
/// tape, mirroring how those other loops already only reply to discovery
/// probes, never NETFS ones. And a remote host reference (see
/// `parse_remote_host_token`) only accepts a known hostname or `nic peers`
/// number, never a raw MAC address, because a MAC's own colons would
/// collide with the `HOST:diskN` separator.
const NETFS_TAG_GET_REQUEST: u8 = 0x30;
const NETFS_TAG_GET_START: u8 = 0x31;
const NETFS_TAG_GET_DATA: u8 = 0x32;
const NETFS_TAG_GET_STOP: u8 = 0x33;
const NETFS_TAG_GET_ERROR: u8 = 0x34;
const NETFS_TAG_PUT_REQUEST: u8 = 0x35;
const NETFS_TAG_PUT_DATA: u8 = 0x36;
const NETFS_TAG_PUT_STOP: u8 = 0x37;
const NETFS_TAG_PUT_ACK: u8 = 0x38;
const NETFS_TAG_PUT_ERROR: u8 = 0x39;

/// One-byte error codes carried in a `NETFS_TAG_*_ERROR` frame's payload.
const NETFS_ERROR_NOT_FOUND: u8 = 1;
const NETFS_ERROR_DISK_UNAVAILABLE: u8 = 2;
const NETFS_ERROR_TOO_LARGE: u8 = 3;
const NETFS_ERROR_INVALID_NAME: u8 = 4;
const NETFS_ERROR_DIRECTORY_FULL: u8 = 5;
const NETFS_ERROR_DATA_FULL: u8 = 6;
const NETFS_ERROR_DISK_ERROR: u8 = 7;
const NETFS_ERROR_CHECKSUM_MISMATCH: u8 = 8;
const NETFS_ERROR_BUSY: u8 = 9;

/// KUP ("kernel update"): a debug-mode-only sibling of NETFS's PUT, sharing
/// `CHAT_ETHERTYPE` and the exact same "`_REQUEST` carrying the length and
/// checksum up front, then sequence-numbered `_DATA` chunks, then `_STOP`,
/// answered with `_ACK` or `_ERROR`" shape (see `NETFS_TAG_PUT_*` above),
/// but installing the received bytes as a whole new kernel image into
/// whichever A/B slot (`debug::SlotMetadata`) is not currently
/// `active_slot`, instead of writing an ordinary FOSFS file. A successful
/// `_ACK` is followed by this host resetting itself a short delay later, so
/// the very next boot is `select_kernel_slot`'s (`boot/stage2.S`) own
/// "first attempt at a fresh pending slot" case -- if the new kernel
/// crashes before ever reaching `debug::confirm_boot`'s milestone, the boot
/// *after that* automatically falls back to the still-known-good slot this
/// update never touched, entirely without physical intervention.
///
/// Deliberately gated behind `debug::kernel_update_enabled()` (the `debug`
/// file's `kernel-update` option) at every call site that dispatches these
/// tags (`ambient_discovery_poll`, and both of `term_net`'s own receive
/// loops via `dispatch_kup_frame`): unlike NETFS, which is always
/// available, letting an arbitrary LAN peer overwrite this machine's own
/// boot media is powerful enough that it must be an explicit, per-machine
/// opt-in, not an ambient default. A host with the option off does not even
/// acknowledge these frames -- indistinguishable, from a client's own
/// timeout, from "not present on this LAN segment at all".
const KUP_TAG_PUT_REQUEST: u8 = 0x3a;
const KUP_TAG_PUT_DATA: u8 = 0x3b;
const KUP_TAG_PUT_STOP: u8 = 0x3c;
const KUP_TAG_PUT_ACK: u8 = 0x3d;
const KUP_TAG_PUT_ERROR: u8 = 0x3e;

/// One-byte error codes carried in a `KUP_TAG_PUT_ERROR` frame's payload;
/// deliberately its own small enum rather than reusing `NETFS_ERROR_*`,
/// since a kernel image is not a FOSFS file and several NETFS codes (bad
/// name, directory full, ...) have no KUP equivalent.
const KUP_ERROR_TOO_LARGE: u8 = 1;
const KUP_ERROR_CHECKSUM_MISMATCH: u8 = 2;
const KUP_ERROR_DISK_ERROR: u8 = 3;
const KUP_ERROR_BUSY: u8 = 4;

/// The FOSFS name-length limit (`fosfs::MAX_NAME_LENGTH`, not itself
/// `pub`), duplicated here for the NETFS wire payload's own fixed-size
/// frame buffers -- the same reasoning already used elsewhere in this file
/// for the same constant (see e.g. this module's own "16 lowercase
/// letters..." messages).
const NETFS_NAME_CAPACITY: usize = 16;

/// How long (in PIT ticks) a NETFS client waits for its peer to respond at
/// all, or to keep making progress, before giving up -- generous enough to
/// tolerate a slow FOSFS write through the BIOS disk trampoline on the
/// serving side, unlike `tape`'s own tighter, purely receive-side timeout.
const NETFS_TIMEOUT_TICKS: u64 = 1000; // ~10 seconds.

/// Computes the RFC 1071 16-bit Internet checksum over `data`, the same
/// algorithm `tapes/tape-net` already uses for IPv4/TCP/UDP -- reused here
/// (reimplemented, not shared code, since tapes and the kernel are separate
/// crate boundaries) purely as a whole-file integrity check: NETFS has no
/// retransmission, so a receiver that computes a different checksum over
/// the reassembled bytes than the sender declared up front knows the
/// transfer was corrupted somewhere and discards it, rather than risking a
/// silently wrong file. This is not authentication or security of any
/// kind -- NETFS has none, matching this project's existing "no auth at
/// all" chat/discovery/tape-transfer protocols.
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let [last] = *chunks.remainder() {
        sum += (last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Every way a client-side NETFS `netfs_get`/`netfs_put` call can fail.
enum NetfsError {
    /// The initial request frame itself could not be sent.
    TransmitFailed,
    /// No response (or no further progress) arrived within
    /// `NETFS_TIMEOUT_TICKS`, or the local operator gave up (F12/etc).
    NoReply,
    /// An out-of-order or short data chunk arrived; this protocol has no
    /// retransmission, so the whole attempt is abandoned instead.
    Gap,
    /// The remote file is larger than the caller's own destination buffer
    /// (checked locally, before any data is transferred).
    TooLargeLocally,
    /// The reassembled bytes didn't match the checksum declared up front.
    ChecksumMismatch,
    /// The peer reported one of the `NETFS_ERROR_*` codes.
    Remote(u8),
}

impl core::fmt::Display for NetfsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetfsError::TransmitFailed => write!(f, "transmit failed (card reported an error, or timed out)"),
            NetfsError::NoReply => write!(f, "no response from the peer; gave up"),
            NetfsError::Gap => write!(f, "gap in the transfer; aborted (NETFS has no retransmission)"),
            NetfsError::TooLargeLocally => write!(f, "remote file is too large for this destination"),
            NetfsError::ChecksumMismatch => write!(f, "checksum mismatch; discarded"),
            NetfsError::Remote(NETFS_ERROR_NOT_FOUND) => write!(f, "remote: file not found"),
            NetfsError::Remote(NETFS_ERROR_DISK_UNAVAILABLE) => write!(f, "remote: no such disk attached there"),
            NetfsError::Remote(NETFS_ERROR_TOO_LARGE) => write!(f, "remote: file too large for its own tape buffer"),
            NetfsError::Remote(NETFS_ERROR_INVALID_NAME) => write!(f, "remote: invalid file name"),
            NetfsError::Remote(NETFS_ERROR_DIRECTORY_FULL) => write!(f, "remote: its FOSFS directory is full"),
            NetfsError::Remote(NETFS_ERROR_DATA_FULL) => write!(f, "remote: its FOSFS data region is full"),
            NetfsError::Remote(NETFS_ERROR_DISK_ERROR) => write!(f, "remote: FOSFS disk error"),
            NetfsError::Remote(NETFS_ERROR_CHECKSUM_MISMATCH) => write!(f, "remote: checksum mismatch; it discarded the write"),
            NetfsError::Remote(NETFS_ERROR_BUSY) => write!(f, "remote: already serving another NETFS transfer"),
            NetfsError::Remote(code) => write!(f, "remote: unknown error code {code}"),
        }
    }
}

const MAX_PEERS: usize = 16;

/// A NIC's own hostname payload as carried in a discovery frame's tag byte
/// plus interface number -- distinct from `HOSTNAME_CAPACITY`, which is the
/// *base* name before that number is appended (see `interface_hostname`).
const PEER_HOSTNAME_CAPACITY: usize = INTERFACE_HOSTNAME_CAPACITY;

/// One entry in the peer address book: a MAC address, plus whatever
/// hostname (if any) that peer's own discovery frame carried. `hostname_len
/// == 0` means the peer answered with no hostname payload at all -- either
/// it has no `hostname` file configured, or it predates this feature
/// entirely (`tools/send_tape.py`'s discovery reply, for instance, has no
/// hostname of its own to send).
#[derive(Clone, Copy)]
struct Peer {
    mac: [u8; 6],
    hostname: [u8; PEER_HOSTNAME_CAPACITY],
    hostname_len: u8,
}

const EMPTY_PEER: Peer = Peer {
    mac: [0; 6],
    hostname: [0; PEER_HOSTNAME_CAPACITY],
    hostname_len: 0,
};

/// Every peer any `nic discover` probe or `chat` auto-reply has ever seen on
/// this wire, in first-seen order; the shell's only address book (see
/// `remember_peer`/`peer_by_index`/`print_peers`). A plain array is enough
/// here, matching this project's general preference for small fixed tables
/// over dynamic collections (there is no heap in this kernel).
static mut PEERS: [Peer; MAX_PEERS] = [EMPTY_PEER; MAX_PEERS];
static mut PEER_COUNT: usize = 0;

/// Records `mac` (with an optional `hostname` payload, already trimmed of
/// wire zero-padding by the caller) in the peer table if it is not already
/// there, silently dropping it once the table is full (`MAX_PEERS`) rather
/// than growing -- this is a teaching aid for a handful of QEMU/LAN peers,
/// not an address book meant to scale. A peer already known without a
/// hostname adopts one the first time a frame carrying one arrives (e.g. a
/// `hostname` file added to that machine after it was first seen), but an
/// already-known hostname is never overwritten by a later, different one --
/// this table records the first name a peer answered with, not a live
/// rename feed. Returns whether `mac` was newly added.
fn remember_peer(mac: [u8; 6], hostname: &[u8]) -> bool {
    let peers = &raw mut PEERS;
    let count = &raw mut PEER_COUNT;
    unsafe {
        if let Some(existing) = (&mut *peers)[..*count]
            .iter_mut()
            .find(|peer| peer.mac == mac)
        {
            if existing.hostname_len == 0 && !hostname.is_empty() {
                let length = hostname.len().min(PEER_HOSTNAME_CAPACITY);
                existing.hostname[..length].copy_from_slice(&hostname[..length]);
                existing.hostname_len = length as u8;
            }
            return false;
        }
        if *count >= MAX_PEERS {
            return false;
        }
        let mut peer = EMPTY_PEER;
        peer.mac = mac;
        let length = hostname.len().min(PEER_HOSTNAME_CAPACITY);
        peer.hostname[..length].copy_from_slice(&hostname[..length]);
        peer.hostname_len = length as u8;
        (*peers)[*count] = peer;
        *count += 1;
        true
    }
}

/// Looks up a peer by its 1-based position in `nic peers`' printed list
/// (matching how `chat` accepts a peer number as shorthand for a full MAC
/// address).
fn peer_by_index(index: usize) -> Option<[u8; 6]> {
    let peers = &raw const PEERS;
    let count = unsafe { PEER_COUNT };
    if index == 0 || index > count {
        return None;
    }
    Some(unsafe { (*peers)[index - 1].mac })
}

/// Looks up a peer by the formatted hostname its own discovery frames
/// carried (see `interface_hostname`/`remember_peer`): an exact,
/// case-sensitive match against whatever that peer's own discovery reply
/// contained -- not a fuzzy or prefix match, and not consulted at all for
/// a peer that never sent a hostname (`hostname_len == 0`).
fn peer_by_hostname(name: &[u8]) -> Option<[u8; 6]> {
    let peers = &raw const PEERS;
    let count = unsafe { PEER_COUNT };
    for index in 0..count {
        let peer = unsafe { (*peers)[index] };
        let hostname_len = peer.hostname_len as usize;
        if hostname_len != 0 && hostname_len == name.len() && &peer.hostname[..hostname_len] == name
        {
            return Some(peer.mac);
        }
    }
    None
}

/// Resolves a peer reference the same way `chat`/`tape send` already
/// resolve their own `PEER_MAC|PEER_NUMBER` argument, except a raw MAC
/// address is deliberately not accepted here: used only by NETFS's
/// `HOST[:diskN]` syntax (see `parse_remote_host_token`), where a MAC's
/// own colons would collide with the `:diskN` separator. `HOST` must
/// therefore be a known hostname or a `nic peers` number instead.
fn resolve_peer(word: &[u8]) -> Option<[u8; 6]> {
    if let Some(index) = parse_number(word) {
        return peer_by_index(index as usize);
    }
    peer_by_hostname(word)
}

/// Parses `HOST` or `HOST:diskN` into a resolved peer MAC plus a raw
/// 1-based disk number on that peer (defaulting to `1`, its own boot disk,
/// when no `:diskN` suffix is given) -- the shared building block behind
/// every NETFS-backed command (`copy`, `tape load`, `tape save`, and `tape
/// run`'s `@HOST[:diskN]/NAME` argument source, see `copy_command`).
/// `HOST` must resolve via `resolve_peer`; an unknown host or malformed
/// `:diskN` suffix is never guessed at, only rejected. The disk number
/// itself is deliberately not checked against any locally-known list --
/// this machine has no way to know what disks a remote peer has attached;
/// the peer validates it and reports `NETFS_ERROR_DISK_UNAVAILABLE` if it
/// doesn't exist there.
fn parse_remote_host_token(word: &[u8]) -> Option<([u8; 6], u8)> {
    let (host_word, remote_disk_number) = match word.iter().position(|&byte| byte == b':') {
        Some(colon) => {
            let suffix = word[colon + 1..].strip_prefix(b"disk")?;
            if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let number: u32 = core::str::from_utf8(suffix).ok()?.parse().ok()?;
            if number == 0 || number > u8::MAX as u32 {
                return None;
            }
            (&word[..colon], number as u8)
        }
        None => (word, 1u8),
    };
    let peer = resolve_peer(host_word)?;
    Some((peer, remote_disk_number))
}

fn print_peers() {
    let peers = &raw const PEERS;
    let count = unsafe { PEER_COUNT };
    if count == 0 {
        println!("No peers known yet. Run 'nic discover' to broadcast a probe -- other fos");
        println!("machines only answer it while their own 'chat' or 'nic discover' is running.");
        return;
    }
    println!("Known peers (use the number with 'chat' instead of a full MAC):");
    for index in 0..count {
        let peer = unsafe { (*peers)[index] };
        let hostname_len = peer.hostname_len as usize;
        if hostname_len == 0 {
            println!("  {}: {}", index + 1, Mac(&peer.mac));
        } else {
            let name = core::str::from_utf8(&peer.hostname[..hostname_len])
                .unwrap_or("<non-UTF-8 hostname>");
            println!("  {}: {} ({name})", index + 1, Mac(&peer.mac));
        }
    }
}

/// The gap, in 100 Hz timer ticks, between re-broadcasts of the discovery
/// probe (see `nic_discover`) -- frequent enough that a peer which starts
/// `chat`/`nic discover` a few seconds late is still found before the
/// operator gives up and presses F12 (or F4/F6/F7/F8/F10), but not so frequent that the wire (or
/// the printed peer list, which only grows on genuinely new replies) is
/// spammed.
const DISCOVER_REBROADCAST_TICKS: u64 = 300;

/// How long after the idle prompt loop first starts (see
/// `ambient_discovery_poll`) ambient self-announces use the fast
/// `AMBIENT_BOOT_BROADCAST_TICKS` interval rather than the slow steady-state
/// one -- long enough that two machines booting within this window of each
/// other, but not perfectly in sync, still overlap at least one of each
/// other's announces before either settles down.
const AMBIENT_BOOT_BURST_TICKS: u64 = 30 * 100; // ~30 seconds.
/// Ambient self-announce interval during the post-boot burst window above:
/// the same cadence `nic_discover`'s own dedicated command already uses.
const AMBIENT_BOOT_BROADCAST_TICKS: u64 = DISCOVER_REBROADCAST_TICKS; // ~3 seconds.
/// Ambient self-announce interval once the post-boot burst window has
/// elapsed -- infrequent enough to not be a constant, unbounded background
/// chirp for a machine that just sits at its idle prompt for hours, while
/// still refreshing every peer's shortlist often enough to notice a
/// machine that joined the network later.
const AMBIENT_STEADY_BROADCAST_TICKS: u64 = 60 * 100; // ~60 seconds.

/// Tick (see `interrupts::ticks`) at which the idle prompt loop first called
/// `ambient_discovery_poll`, used only to decide how long the post-boot
/// burst window (`AMBIENT_BOOT_BURST_TICKS`) has left. `None` until that
/// first call.
static mut AMBIENT_DISCOVERY_STARTED_AT: Option<u64> = None;
/// Tick at which `ambient_discovery_poll` should next self-announce;
/// starts at 0 so the very first call always announces immediately.
static mut NEXT_AMBIENT_BROADCAST: u64 = 0;

/// Dispatches one incoming frame's tag if (and only if) it is a KUP tag
/// and `debug::kernel_update_enabled()`, returning whether it was
/// handled -- shared by `ambient_discovery_poll` and both of `term_net`'s
/// own receive loops (the "waiting for a `HELLO`" wait and an active
/// session's own loop), so a kernel update can be pushed while `term_net`
/// is running instead of requiring a local escape to the idle prompt
/// first. KUP's own transfer state
/// (`serve_kup_put_*`) is independent of whatever `term_net` itself is
/// doing with the NIC, and a KUP push is a deliberate, operator-initiated
/// action either way -- the same "shared staging buffer, don't run a
/// local `copy`/`tape load` at the same time" caveat already documented
/// for KUP applies regardless of which loop happens to be polling for it.
fn dispatch_kup_frame(
    info: &BootInfo,
    nic_index: usize,
    nic: &nic::Nic,
    peer_mac: [u8; 6],
    own_mac: [u8; 6],
    tag: u8,
    payload: &[u8],
) -> bool {
    // The `settings` file's `deny-term-net` list also closes KUP on a NIC: a
    // port that must not offer a remote shell must not accept a new kernel
    // either. A denied frame is left unhandled, so it is ignored silently.
    if !crate::debug::kernel_update_enabled() || term_net_denied(nic_index) {
        return false;
    }
    match tag {
        KUP_TAG_PUT_REQUEST => serve_kup_put_request(nic, peer_mac, own_mac, payload),
        KUP_TAG_PUT_DATA => serve_kup_put_data(peer_mac, payload),
        KUP_TAG_PUT_STOP => serve_kup_put_stop(info, nic, peer_mac, own_mac),
        _ => return false,
    }
    true
}

/// The "always on" half of NIC discovery: called every iteration of the
/// idle interactive prompt loop in `run` -- the one loop that, before this,
/// never touched the network card at all, since every other long-running
/// command (`chat`, `term net`, `nic discover`, a running tape, ...) already
/// polls its own NIC and already replies to an incoming discovery probe
/// unconditionally. It always answers a `CHAT_TAG_DISCOVER_REQUEST` and
/// remembers a `CHAT_TAG_DISCOVER_REPLY` silently (no `println!` of its
/// own -- unlike `nic_discover`'s dedicated command, this must never
/// interrupt whatever the operator is typing at the ordinary prompt), and
/// periodically re-announces itself so other machines' own shortlists stay
/// current even when nobody is running `nic discover` at all. There is
/// still no interrupt-driven network service in this kernel (see "Current
/// boundaries" in `AGENTS.md`): this is only "always on" in the sense that
/// the one loop that is always running (the idle prompt) now always
/// participates, not that discovery runs independently of the CPU actually
/// being here to poll for it.
///
/// NETFS requests (`NETFS_TAG_GET_REQUEST`/`PUT_REQUEST`/`PUT_DATA`/
/// `PUT_STOP`) are served here and, through `dispatch_netfs_serve_frame`,
/// by both of `term_net`'s receive loops, but not by `chat`/`nic discover`
/// or a running tape (see "NETFS" above). Unlike routine discovery traffic, NETFS activity is
/// rare and consequential enough to be worth a `println!` here even though
/// discovery replies deliberately stay silent.
///
/// Returns `true` if any NIC actually received a frame this call (whether
/// or not it matched a tag this function understands). `run`'s idle loop
/// uses this to decide whether it is safe to `hlt` until the next
/// interrupt: an in-flight NETFS/KUP transfer arrives in paced chunks only
/// a few milliseconds apart, so as long as frames keep arriving the caller
/// must keep polling at full speed rather than sleeping through the next
/// chunk.
fn ambient_discovery_poll(info: &BootInfo) -> bool {
    let now = interrupts::ticks();
    let started_at = unsafe {
        let started = &raw mut AMBIENT_DISCOVERY_STARTED_AT;
        match *started {
            Some(value) => value,
            None => {
                *started = Some(now);
                now
            }
        }
    };

    let mut received_frame = false;
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    for index in 0..nic::count() {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let Some(length) = nic.receive(&mut buffer) else {
            continue;
        };
        received_frame = true;
        let frame = &buffer[..length];
        if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
            continue;
        }
        let peer_mac: [u8; 6] = frame[6..12].try_into().unwrap();
        let payload = &frame[15..];
        let own_mac = nic.mac_address();
        match frame[14] {
            CHAT_TAG_DISCOVER_REPLY => {
                remember_peer(peer_mac, trim_trailing_zero(payload));
            }
            CHAT_TAG_DISCOVER_REQUEST => {
                remember_peer(peer_mac, trim_trailing_zero(payload));
                send_chat_control(nic, &peer_mac, &own_mac, CHAT_TAG_DISCOVER_REPLY, index);
            }
            tag if dispatch_netfs_serve_frame(
                info, index, nic, peer_mac, own_mac, tag, payload,
            ) => {}
            tag if dispatch_kup_frame(info, index, nic, peer_mac, own_mac, tag, payload) => {}
            _ => {}
        }
    }

    if now < unsafe { NEXT_AMBIENT_BROADCAST } {
        return received_frame;
    }
    for index in 0..nic::count() {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let source = nic.mac_address();
        send_chat_control(nic, &BROADCAST_MAC, &source, CHAT_TAG_DISCOVER_REQUEST, index);
    }
    let interval = if now.wrapping_sub(started_at) < AMBIENT_BOOT_BURST_TICKS {
        AMBIENT_BOOT_BROADCAST_TICKS
    } else {
        AMBIENT_STEADY_BROADCAST_TICKS
    };
    unsafe { NEXT_AMBIENT_BROADCAST = now + interval };
    received_frame
}

/// Broadcasts a discovery probe (`CHAT_TAG_DISCOVER_REQUEST`) through every
/// initialized interface, re-sending it every `DISCOVER_REBROADCAST_TICKS` so
/// machines that only start `chat` or `nic discover` partway through are still
/// found, and keeps listening on every interface -- printing each newly-seen
/// peer as it replies -- until F12 (or F4/F6/F7/F8/F10) stops it, the same
/// convention `nic sniff`/`term`/`chat` all use. Replies to incoming probes
/// go back through the interface where the probe arrived. There is no
/// interrupt-driven network service in this kernel (see "Current boundaries"
/// in `AGENTS.md`), so a reply only ever comes from a fos machine whose own
/// `chat` or `nic discover` happens to be running and polling its card while
/// this loop is open -- it only finds who is listening *right now*, not who
/// exists on the network.
fn nic_discover() {
    let count = nic::count();
    if count == 0 {
        nic_status();
        return;
    }
    println!(
        "Broadcasting a discovery probe on {} interface(s) every few seconds;",
        count
    );
    println!("press F12 (or F4/F6/F7/F8/F10) to stop and see the list.");
    for index in 0..count {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let source = nic.mac_address();
        send_chat_control(nic, &BROADCAST_MAC, &source, CHAT_TAG_DISCOVER_REQUEST, index);
    }
    let mut next_broadcast = interrupts::ticks() + DISCOVER_REBROADCAST_TICKS;

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut discovered = 0u32;
    'discover: loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            break;
        }
        // Relays live output to a connected `term_net` peer (if `nic
        // discover` was itself typed as a remote command) and lets the
        // active NIC's watchdog recover a latched fault -- previously
        // this loop did neither.
        poll_background_work(nic::get().ok());
        if interrupts::ticks() >= next_broadcast {
            for index in 0..count {
                let Some(nic) = nic::get_at(index) else {
                    continue;
                };
                let source = nic.mac_address();
                send_chat_control(
                    nic,
                    &BROADCAST_MAC,
                    &source,
                    CHAT_TAG_DISCOVER_REQUEST,
                    index,
                );
            }
            next_broadcast = interrupts::ticks() + DISCOVER_REBROADCAST_TICKS;
        }
        let mut received_any = false;
        for index in 0..count {
            let Some(nic) = nic::get_at(index) else {
                continue;
            };
            let source = nic.mac_address();
            let Some(length) = nic.receive(&mut buffer) else {
                continue;
            };
            received_any = true;
            let frame = &buffer[..length];
            if remote_stop_frame(frame) {
                break 'discover;
            }
            if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                continue;
            }
            let peer_mac: [u8; 6] = frame[6..12].try_into().unwrap();
            let hostname = trim_trailing_zero(&frame[15..]);
            match frame[14] {
                CHAT_TAG_DISCOVER_REPLY => {
                    if remember_peer(peer_mac, hostname) {
                        discovered += 1;
                        println!("Found peer #{discovered}: {}", Mac(&peer_mac));
                    }
                }
                // A peer's own probe reaching us proves it's listening too,
                // so it belongs in our list exactly as if it had replied to
                // ours. Reply on the same interface so discovery works across
                // isolated networks attached to different local NICs.
                CHAT_TAG_DISCOVER_REQUEST => {
                    if remember_peer(peer_mac, hostname) {
                        discovered += 1;
                        println!("Found peer #{discovered}: {}", Mac(&peer_mac));
                    }
                    send_chat_control(nic, &peer_mac, &source, CHAT_TAG_DISCOVER_REPLY, index);
                }
                _ => continue,
            }
        }
        if !received_any {
            idle_wait();
        }
    }

    println!("\nStopped discovery; found {discovered} new peer(s) this run.");
    print_peers();
}

/// `chat` (no arguments) is an open, broadcast text chat: every frame it
/// sends goes to the broadcast address, and it displays any `CHAT_ETHERTYPE`
/// text frame it receives from anyone. `chat AA:BB:CC:DD:EE:FF` (or, once
/// `nic discover` has found peers, the shorter `chat N`, a 1-based number
/// from `nic peers`) instead targets one peer directly (a real unicast
/// Ethernet address, not a broadcast) and only displays lines whose
/// *source* matches that address, for a private two-machine conversation --
/// every mode reuses the exact same `Rtl8139::send`/`receive` calls, since
/// addressing here is just a different destination MAC, not a different
/// protocol.
///
/// Typed lines are sent on Enter, one line per frame, with the same
/// backspace/echo/length-limit handling the main shell prompt itself uses;
/// incoming lines are printed as they arrive, tagged with the sender's MAC.
/// Because incoming lines can arrive while a local line is only half typed,
/// they may interleave with it on screen -- a known, accepted simplification
/// for a single-scrolling-console chat, not a bug. While running, `chat`
/// also silently answers any discovery probe it sees (see `nic discover`)
/// with a one-line note, so simply leaving a chat session open is enough to
/// make a machine discoverable. Leaves through F12 (or F4/F6/F7/F8/F10) (only available from the
/// local PS/2 keyboard, same as `term`/`nic sniff`) or by typing the line
/// `/quit` (which also works from a serial-only session, since -- unlike
/// `term` -- `chat` never takes over the serial console).
#[inline(never)]
fn chat_command(arguments: &[u8]) {
    let (peer_word, remainder) = take_word(arguments);
    if !remainder.is_empty() {
        println!("Usage: chat [PEER_MAC|PEER_NUMBER]");
        return;
    }
    let peer = if peer_word.is_empty() {
        None
    } else if let Some(mac) = parse_mac(peer_word) {
        Some(mac)
    } else if let Some(index) = parse_number(peer_word) {
        match peer_by_index(index as usize) {
            Some(mac) => Some(mac),
            None => {
                println!(
                    "No peer #{index} known; run 'nic discover' first, or give a full MAC address."
                );
                return;
            }
        }
    } else {
        println!(
            "'{}' is neither a MAC address (aa:bb:cc:dd:ee:ff) nor a known peer",
            core::str::from_utf8(peer_word).unwrap_or("?")
        );
        println!("number; see 'nic peers'.");
        return;
    };

    let Some(nic) = nic_or_report() else { return };
    let destination = peer.unwrap_or(BROADCAST_MAC);
    let source = nic.mac_address();

    match peer {
        Some(mac) => {
            println!("Chatting with {} only. F12 (or F4/F6/F7/F8/F10) or", Mac(&mac));
            println!("'/quit' to leave.");
        }
        None => {
            println!("Broadcasting to everyone listening. F12 (or F4/F6/F7/F8/F10)");
            println!("or '/quit' to leave.");
        }
    }

    let mut line = [0u8; CHAT_TEXT_CAPACITY];
    let mut length = 0usize;
    let mut saw_carriage_return = false;
    let mut recv_buffer = [0u8; nic::MAX_FRAME_LEN];

    'chat: loop {
        // Relays live output to a connected `term_net` peer (if `chat`
        // was itself typed as a remote command) and lets the NIC
        // watchdog recover a latched fault -- previously this loop did
        // neither.
        poll_background_work(Some(nic));
        // Serial bytes take the same path a plain keyboard character
        // would (there is no serial equivalent of the F12 (or F4/F6/F7/F8/F10) escape key);
        // only fall back to polling the keyboard -- which can also report
        // Escape -- once no serial byte is waiting.
        let event = match console::read_serial_byte() {
            Some(byte) => Some(keyboard::KeyEvent::Char(byte)),
            None => keyboard::poll_term_event(),
        };
        match event {
            Some(keyboard::KeyEvent::Escape) => break 'chat,
            Some(keyboard::KeyEvent::Char(b'\n')) if saw_carriage_return => {
                saw_carriage_return = false
            }
            Some(keyboard::KeyEvent::Char(byte @ (b'\r' | b'\n'))) => {
                saw_carriage_return = byte == b'\r';
                println!();
                if &line[..length] == b"/quit" {
                    break 'chat;
                }
                if length > 0 {
                    send_chat_line(nic, &destination, &source, &line[..length]);
                }
                length = 0;
            }
            Some(keyboard::KeyEvent::Char(8 | 127)) if length > 0 => {
                saw_carriage_return = false;
                length -= 1;
                console::erase_last_character();
            }
            Some(keyboard::KeyEvent::Char(byte @ 0x20..=0x7e)) if length < CHAT_TEXT_CAPACITY => {
                saw_carriage_return = false;
                line[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            Some(keyboard::KeyEvent::Char(0x20..=0x7e)) => {
                saw_carriage_return = false;
                println!("\nChat lines are limited to {CHAT_TEXT_CAPACITY} characters.");
            }
            _ => {}
        }

        if let Some(received) = nic.receive(&mut recv_buffer) {
            handle_incoming_chat_frame(nic, &recv_buffer[..received], &source, peer.as_ref());
        }
    }

    println!("Left chat; back at the fos shell.");
}

fn send_chat_line(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], text: &[u8]) {
    let mut frame = [0u8; 15 + CHAT_TEXT_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = CHAT_TAG_TEXT;
    frame[15..15 + text.len()].copy_from_slice(text);
    if !nic.send(&frame[..15 + text.len()]) {
        println!("(message not sent: card reported an error, or timed out)");
    }
}

/// Sends a `CHAT_ETHERTYPE` control frame (a discovery request or reply):
/// the Ethernet header, the one-byte tag, and -- if this machine has a
/// configured hostname (see `load_hostname`) -- that interface's own
/// formatted name (`interface_hostname`) as payload. `interface_index` is
/// this NIC's 0-based position in `nic::get_at`, matching `nic list`'s own
/// numbering once it is displayed 1-based. A machine with no `hostname`
/// file sends exactly the same zero-payload frame this always sent before
/// this feature existed, so an old peer/host tool sees no difference.
fn send_chat_control(
    nic: &nic::Nic,
    destination: &[u8; 6],
    source: &[u8; 6],
    tag: u8,
    interface_index: usize,
) {
    let mut hostname = [0u8; INTERFACE_HOSTNAME_CAPACITY];
    let hostname_len = interface_hostname(interface_index, &mut hostname);
    let mut frame = [0u8; 15 + INTERFACE_HOSTNAME_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + hostname_len].copy_from_slice(&hostname[..hostname_len]);
    nic.send(&frame[..15 + hostname_len]);
}

/// Handles one received frame during `chat`: a `CHAT_TAG_TEXT` line is
/// printed (filtered to `peer`'s source address in a unicast conversation;
/// shown from anyone while broadcasting) after trimming the trailing zero
/// padding `send_chat_line`/`Rtl8139::send`/`tools/send_frame.py` all add to
/// reach Ethernet's 60-byte minimum frame length. A `CHAT_TAG_DISCOVER_REQUEST`
/// is answered automatically with a reply addressed straight back to its
/// sender (never broadcast, to keep replies from also broadcasting to every
/// other listener) and remembered as a peer, exactly as if `nic discover`
/// itself had seen it; anything else (including a stray discovery reply
/// while just chatting) is ignored.
fn handle_incoming_chat_frame(
    nic: &nic::Nic,
    frame: &[u8],
    own_mac: &[u8; 6],
    peer: Option<&[u8; 6]>,
) {
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        return;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    match frame[14] {
        CHAT_TAG_TEXT => {
            if let Some(peer) = peer {
                if &source != peer {
                    return;
                }
            }
            let payload = &frame[15..];
            let text_end = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            let text =
                core::str::from_utf8(&payload[..text_end]).unwrap_or("<non-UTF-8 chat data>");
            println!("\n{}: {text}", Mac(&source));
        }
        CHAT_TAG_DISCOVER_REQUEST => {
            remember_peer(source, trim_trailing_zero(&frame[15..]));
            send_chat_control(nic, &source, own_mac, CHAT_TAG_DISCOVER_REPLY, nic::active_index());
            println!("\n(replied to a discovery probe from {})", Mac(&source));
        }
        _ => {}
    }
}

/// The one currently loaded "paper tape": always `pmem::TAPE_BASE` (see its
/// doc comment) plus the tape's exact byte length. `None` until `tape load
/// disk`/`tape load net` succeeds; loading a new tape simply overwrites the
/// fixed region in place, so there's no per-tape frame bookkeeping to free.
/// The current supervisor identity map keeps this physical address directly
/// usable as a pointer -- the same convention `rtl8139.rs`'s own DMA buffers
/// rely on.
static mut TAPE: Option<(u32, u32)> = None;
static mut TAPE_ENTRY: u32 = 0;
static mut TAPE_ELF: bool = false;
static mut TAPE_HANDLE: u32 = 0;
static mut NEXT_TAPE_HANDLE: u32 = 1;
static mut TAPE_BOOT_INFO: *const BootInfo = core::ptr::null();

const TAPE_FS_OK: u32 = 0;
const TAPE_FS_NOT_FOUND: u32 = 1;
const TAPE_FS_INVALID_ARGUMENT: u32 = 2;
const TAPE_FS_BUFFER_TOO_SMALL: u32 = 3;
const TAPE_FS_DISK_ERROR: u32 = 4;
const TAPE_FS_DIRECTORY_FULL: u32 = 5;
const TAPE_FS_DATA_FULL: u32 = 6;

/// Reads/writes `TAPE` through raw pointers (`&raw const`/`&raw mut`)
/// rather than ever forming a `&`/`&mut` reference to the static, the same
/// convention `PEERS`/`PEER_COUNT` use just above -- this is a
/// single-threaded, non-reentrant shell, so the only real hazard is taking
/// a live reference across something that could alias it.
fn tape_get() -> Option<(u32, u32)> {
    unsafe { *(&raw const TAPE) }
}

fn tape_entry() -> u32 {
    unsafe { *(&raw const TAPE_ENTRY) }
}

fn install_tape(address: u32, length: u32) -> bool {
    let (entry, elf) = match load_elf32_image(address, length) {
        Ok(Some(entry)) => (entry, true),
        Ok(None) => (address, false),
        Err(message) => {
            println!("Tape rejected: {message}");
            return false;
        }
    };
    let handle = unsafe { *(&raw const NEXT_TAPE_HANDLE) };
    unsafe {
        *(&raw mut TAPE) = Some((address, length));
        *(&raw mut TAPE_ENTRY) = entry;
        *(&raw mut TAPE_ELF) = elf;
        *(&raw mut TAPE_HANDLE) = handle;
        *(&raw mut NEXT_TAPE_HANDLE) = handle.wrapping_add(1).max(1);
    }
    true
}

/// `tape [status|run [ARGS...]|load ...|save ...|send ...]`. `status`/no
/// arguments report what (if anything) is loaded; `run` executes it,
/// optionally passing everything after `run` to the tape verbatim as its
/// own `args_ptr`/`args_len` (see `tape_run`'s doc comment) -- a tape that
/// ignores those two arguments (every existing tape in this repository
/// does) is unaffected either way; `load`/`save`/`send` are documented on
/// their own handlers below.
#[inline(never)]
fn tape_command(info: &BootInfo, arguments: &[u8]) {
    let (subcommand, remainder) = take_word(arguments);
    match subcommand {
        b"" | b"status" if remainder.is_empty() => tape_status(),
        b"run" => tape_run(info, remainder),
        b"load" => tape_load_command(info, remainder),
        b"save" => tape_save_command(info, remainder),
        b"send" => tape_send_command(info, remainder),
        _ => {
            println!("Usage: tape [status|run [ARGS...]|load disk NAME|load net|");
            println!("save [diskN] NAME|send [diskN] NAME PEER_MAC|PEER_NUMBER]");
        }
    }
}

fn tape_status() {
    match tape_get() {
        Some((_address, length)) => {
            let format = if unsafe { *(&raw const TAPE_ELF) } {
                "ELF32"
            } else {
                "legacy flat"
            };
            println!(
                "Tape loaded: #{}; {length} byte(s), {format},",
                unsafe { *(&raw const TAPE_HANDLE) },
            );
            println!("entry 0x{:08x}. 'tape run' to execute it.", tape_entry())
        }
        None => println!("No tape loaded. 'tape load disk NAME' or 'tape load net' to load one."),
    }
}

/// Validates `length` fits the fixed tape region and reports the fixed
/// address, or explains why it doesn't. Every tape -- whatever loads it --
/// always lands at the same `pmem::TAPE_BASE` address (see its doc
/// comment): unlike a general allocation, this is deliberate, so a tape's
/// own compiled code (assembly, C, or Rust) can address itself directly
/// instead of needing to be position-independent and locate its own
/// runtime base first. Shared by both `tape load disk` and `tape load
/// net`, since either source ends up needing the exact same buffer.
fn allocate_tape_buffer(length: u32) -> Option<u32> {
    if length > pmem::TAPE_LEN {
        println!(
            "Tape too large: {length} byte(s) exceeds the {}-byte fixed tape",
            pmem::TAPE_LEN,
        );
        println!("region at 0x{:05x}.", pmem::TAPE_BASE);
        return None;
    }
    Some(pmem::TAPE_BASE)
}

#[derive(Clone, Copy)]
struct Elf32Load {
    offset: u32,
    virtual_address: u32,
    file_size: u32,
    memory_size: u32,
}

/// Validates and materializes one ELF32 tape in its linked load region.
///
/// The first migration step intentionally accepts `ET_EXEC` images linked at
/// the historical `pmem::TAPE_BASE` and retains a raw-flat fallback. The
/// program-header-only loader keeps section metadata out of the runtime ABI;
/// `ET_DYN`/relocation support can be added later without changing callers.
fn load_elf32_image(address: u32, length: u32) -> Result<Option<u32>, &'static str> {
    if length < 52 {
        return Ok(None);
    }
    let image = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    if &image[..4] != b"\x7fELF" {
        return Ok(None);
    }
    if image[4] != 1 || image[5] != 1 || image[6] != 1 {
        return Err("expected little-endian ELF32");
    }
    let e_type = elf_u16(image, 16)?;
    if e_type != 2 {
        return Err("only fixed-address ELF32 ET_EXEC tapes are supported");
    }
    if elf_u16(image, 18)? != 3 {
        return Err("ELF image is not for i386");
    }
    let entry = elf_u32(image, 24)?;
    let program_offset = elf_u32(image, 28)?;
    let entry_size = elf_u16(image, 42)? as u32;
    let program_count = elf_u16(image, 44)? as u32;
    if entry_size < 32 || program_count == 0 || program_count > 32 {
        return Err("invalid ELF32 program-header table");
    }
    let table_end = program_offset
        .checked_add(entry_size.saturating_mul(program_count))
        .ok_or("ELF32 program-header table overflows")?;
    if table_end > length {
        return Err("ELF32 program-header table is outside the file");
    }

    let mut loads = [Elf32Load {
        offset: 0,
        virtual_address: 0,
        file_size: 0,
        memory_size: 0,
    }; 32];
    let mut load_count = 0usize;
    let tape_end = pmem::TAPE_BASE
        .checked_add(pmem::TAPE_LEN)
        .ok_or("tape address range overflows")?;
    for index in 0..program_count as usize {
        let header = program_offset as usize + index * entry_size as usize;
        let kind = elf_u32(image, header)?;
        if kind != 1 {
            continue;
        }
        let file_offset = elf_u32(image, header + 4)?;
        let virtual_address = elf_u32(image, header + 8)?;
        let file_size = elf_u32(image, header + 16)?;
        let memory_size = elf_u32(image, header + 20)?;
        if file_offset
            .checked_add(file_size)
            .ok_or("ELF32 segment overflows")?
            > length
            || file_size > memory_size
            || virtual_address < pmem::TAPE_BASE
            || virtual_address
                .checked_add(memory_size)
                .ok_or("ELF32 segment address overflows")?
                > tape_end
        {
            return Err("ELF32 load segment is outside the tape region");
        }
        if load_count == loads.len() {
            return Err("too many ELF32 load segments");
        }
        loads[load_count] = Elf32Load {
            offset: file_offset,
            virtual_address,
            file_size,
            memory_size,
        };
        load_count += 1;
    }
    if load_count == 0
        || entry < pmem::TAPE_BASE
        || entry >= tape_end
        || !loads[..load_count].iter().any(|segment| {
            entry >= segment.virtual_address
                && entry < segment.virtual_address + segment.memory_size
        })
    {
        return Err("ELF32 entry point is not inside a loadable segment");
    }

    for segment in &loads[..load_count] {
        unsafe {
            core::ptr::copy(
                (address + segment.offset) as *const u8,
                segment.virtual_address as *mut u8,
                segment.file_size as usize,
            );
            core::ptr::write_bytes(
                (segment.virtual_address + segment.file_size) as *mut u8,
                0,
                (segment.memory_size - segment.file_size) as usize,
            );
        }
    }
    Ok(Some(entry))
}

fn elf_u16(image: &[u8], offset: usize) -> Result<u16, &'static str> {
    let bytes = image
        .get(offset..offset + 2)
        .ok_or("truncated ELF32 header")?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn elf_u32(image: &[u8], offset: usize) -> Result<u32, &'static str> {
    let bytes = image
        .get(offset..offset + 4)
        .ok_or("truncated ELF32 header")?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Executes a loaded tape's validated ELF entry point (or the image base for
/// the legacy raw-flat fallback) in ring 0 on the kernel's own stack. The
/// current paging setup is supervisor-only identity mapping, not isolation,
/// so a tape remains trusted exactly as much as any other kernel code. If it
/// ends with a near `ret`, control returns cleanly to this shell; if it hangs,
/// faults, or corrupts kernel state, there is no recovery -- a reset or
/// power-cycle is required, exactly as with any other unrecoverable fault.
///
/// Called as `extern "C" fn(putc: extern "C" fn(u32), should_abort: extern
/// "C" fn() -> u32, alloc: extern "C" fn(u32) -> u32, free: extern "C"
/// fn(u32, u32), read_key: extern "C" fn() -> u32, args_ptr: u32, args_len:
/// u32, nic_mac: extern "C" fn(u32) -> u32, nic_send: extern "C" fn(u32,
/// u32) -> u32, nic_receive: extern "C" fn(u32, u32) -> u32,
/// nic_max_frame_len: extern "C" fn() -> u32, ticks: extern "C" fn() -> u64,
/// nic_count: extern "C" fn() -> u32, nic_mac_at: extern "C" fn(u32, u32)
/// -> u32, nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
/// nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
/// nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
/// terminal_position: extern "C" fn() -> u32, fs_file_length: extern "C"
/// fn(u32, u32, u32) -> u32, fs_read: extern "C" fn(u32, u32, u32, u32, u32)
/// -> u32, fs_write: extern "C" fn(u32, u32, u32, u32) -> u32, fs_remove:
/// extern "C" fn(u32, u32) -> u32, audio_sample_rate: extern "C" fn() ->
/// u32, audio_channels_and_bits: extern "C" fn() -> u32, audio_submit:
/// extern "C" fn(u32, u32) -> u32, audio_capture: extern "C" fn(u32, u32)
/// -> u32, audio_count: extern "C" fn() -> u32, audio_sample_rate_at:
/// extern "C" fn(u32) -> u32, audio_channels_and_bits_at: extern "C"
/// fn(u32) -> u32, audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
/// audio_capture_at: extern "C" fn(u32, u32, u32) -> u32)` (cdecl, thirty
/// arguments), in that
/// fixed order. A tape is free
/// to declare only a leading prefix of these parameters (as
/// `primes.S`/`hello-rs` declare only `putc`, and `vga-colors` declares
/// only `putc`/`should_abort`) and ignore the rest; under cdecl this
/// remains binary-compatible, since the caller (this function) both
/// pushes and cleans up every argument regardless of how many the callee
/// actually reads. A tape needing a *later* argument (like `alloc`/`free`
/// below) must still declare every parameter before it, purely to keep
/// each argument's stack position lined up correctly -- see
/// `docs/rust-tapes-for-dummies.md` for a worked example.
/// - `console::print_byte` is passed as the first argument, giving a tape
///   a narrow, explicit way to call back into the kernel's own console
///   code to print, instead of touching VGA/COM1 hardware directly (see
///   `tapes/primes.S`'s header comment for why this one callback was
///   worth the exception). This is what makes a tape's own output show
///   up over `term net` like any other command's, and is why `tape_run`
///   no longer needs to re-sync the console's cursor state afterward: a
///   tape that only ever prints through this callback never touches the
///   hardware cursor itself, so the kernel's own cached position is
///   never stale to begin with.
/// - `combined_tape_should_abort` is passed as the second argument, giving
///   a long-running or looping tape (like `tapes/vga-colors`) a way
///   to notice a "give up" request without touching the keyboard
///   controller or network card itself. It checks two independent
///   sources, either one enough to stop the tape: a local "give up"
///   keypress (F12/F4/F6/F7/F8/F10, via `keyboard::tape_should_abort` --
///   see that function's own doc comment for why this is safe even when
///   a tape was started remotely over an active `term_net` session), and,
///   if a `term_net` session is what started this tape, a remote one too
///   (via `poll_remote_tape_abort`, since that session's own loop -- and
///   so its own `nic.receive` -- is paused for as long as this tape runs
///   and cannot notice incoming frames on its own).
/// - `tape_alloc`/`tape_free` are passed as the third and fourth
///   arguments, letting a tape reach far beyond its own fixed 4 MiB
///   static region (`pmem::TAPE_BASE`/`TAPE_LEN`) for a large working set
///   (a big sieve, a large buffer, ...) -- up to almost all of this
///   machine's RAM (see `pmem::TRACKED_BYTES`), by borrowing physically
///   contiguous frames straight from the same general pool `palloc`/
///   `pfree` operate on. See `tape_alloc`'s own doc comment for the full
///   contract (no auto-zeroing, no auto-free on return).
/// - `combined_tape_read_key` is passed as the fifth argument, giving a
///   tape a way to read actual input -- not just detect "give up" --
///   from whichever source started it, local keyboard or an active
///   `term_net` session, without knowing or caring which. See its own
///   doc comment for the full return-value encoding and, importantly,
///   why a tape should use *either* this *or* `should_abort`, never both
///   in the same loop.
/// - `args_ptr`/`args_len` are passed as the sixth and seventh arguments:
///   the address and length of whatever trailing text followed `tape run`
///   on the command line (e.g. `tape run 2 + 2` gives a tape `"2 + 2"`,
///   0 if none was given), letting a tape take simple, one-shot startup
///   parameters the way a Unix program reads `argv` -- as opposed to
///   `read_key`'s repeated, interactive input during a tape's own run.
///   This slice points directly into the command line this shell already
///   parsed (see `execute`/`LineEditor::feed`), which remains valid for
///   as long as this synchronous call lasts, so no copy is made. A tape
///   ignoring these two (every existing tape in this repository does)
///   simply never reads them, exactly like any other trailing argument
///   under cdecl.
/// - `tape_nic_mac`/`tape_nic_send`/`tape_nic_receive` are the first three
///   callbacks. They expose the active kernel-owned NIC as raw Ethernet
///   frames, so a tape can experiment with network protocols without
///   duplicating PCI discovery, DMA allocation, ring management, or
///   overflow recovery. `nic_mac` copies this machine's six-byte MAC to
///   its caller-provided buffer; `nic_send` transmits a caller-provided
///   complete frame; `nic_receive` copies one complete received frame to
///   its caller-provided buffer and returns its length, or zero when no
///   frame is waiting. Each returns zero on failure. A tape should call
///   `nic_mac` once at startup and report its failure explicitly before
///   entering a receive loop; the callbacks then reuse the same active
///   interface as the shell's `nic` and `chat` commands.
/// - `tape_nic_max_frame_len` and `tape_ticks` are the next two callbacks.
///   The first reports the largest frame `nic_receive` can return. The
///   second returns the monotonic PIT tick count used for protocol deadlines.
///   They are deliberately separate from packet operations so a shared
///   library can remain deterministic and testable with fake callbacks.
/// - `tape_nic_count`, `tape_nic_mac_at`, `tape_nic_send_at`, and
///   `tape_nic_receive_at` are appended after `ticks`. They let a newer tape
///   enumerate interfaces and keep independent raw-frame state per index.
/// - `tape_nic_set_promiscuous_at` enables or disables receive-all mode on
///   one selected interface, returning zero if that index is invalid. A
///   layer-2 bridge needs this because frames addressed to another host are
///   not normally delivered to a NIC's receive ring. `terminal_position`
///   and the `fs_*` file callbacks (documented individually near their own
///   definitions, e.g. [`tape_fs_remove`]) follow it.
/// - `tape_audio_sample_rate`/`tape_audio_channels_and_bits`/
///   `tape_audio_submit`/`tape_audio_capture` give a tape
///   access to the kernel-owned audio playback facade (`audio.rs`) the
///   same way the NIC callbacks above expose the kernel-owned network
///   card: `audio_sample_rate` and `audio_channels_and_bits` report
///   device 0's fixed format (zero if no device is present -- a
///   streaming tape must query both before its first `audio_submit` and
///   produce exactly that format, since this kernel does no resampling or
///   requantization of its own), `audio_submit` pushes raw PCM bytes
///   into that device's ISR-drained playback ring buffer, returning how
///   many bytes were actually accepted (a short count, including zero,
///   means the ring is temporarily full and the caller should retry the
///   remainder on a later poll, mirroring `tape_nic_send`'s own
///   short-write convention), and `audio_capture` copies out ISR-filled
///   recorded PCM bytes from the device's own capture ring, returning how
///   many bytes were actually copied (fewer than requested, including
///   zero, simply means fewer samples have been captured so far --
///   mirroring `tape_nic_receive`'s "however much is ready" convention
///   rather than `audio_submit`'s "however much fits" one).
/// - `tape_audio_count`, `tape_audio_sample_rate_at`,
///   `tape_audio_channels_and_bits_at`, `tape_audio_submit_at`, and
///   `tape_audio_capture_at` are the final five callbacks, added
///   alongside `es1370.rs`'s conversion to a multi-instance driver
///   (mirroring the NIC family's own `nic_count`/`nic_mac_at`/etc.
///   addition above): `audio_count` reports how many independent audio
///   devices (e.g. several physical Sound Blaster PCI cards) were
///   discovered, and the four `_at`-suffixed callbacks are the
///   index-based (0-based, matching `nic_mac_at`'s own convention)
///   counterparts to the four non-indexed callbacks just above, letting
///   a tape drive every discovered card as an independent stereo pair --
///   e.g. to combine four 2-channel cards into one 8-channel logical
///   stream -- instead of only ever the first one. The non-indexed
///   callbacks keep meaning "device 0" unchanged, so a tape written
///   before this addition (or one that only ever expects a single card)
///   needs no change at all. Every
///   existing tape in this repository ignores all nine of these audio
///   callbacks, exactly like any other trailing argument under cdecl.
/// - `tape_audio_capture_sample_rate`/`tape_audio_capture_channels_and_bits`
///   (device 0) and their `_at`-suffixed counterparts are the final four
///   callbacks, added because USB Audio Class capture (`uhci.rs`) broke
///   the assumption every earlier callback relied on: that one shared
///   format describes both `audio_submit`'s playback bytes and
///   `audio_capture`'s recorded ones. True for ES1370/HDA's real
///   hardware, but not for a real USB device whose capture interface can
///   have (and, on the physically tested TerraTec Aureon Dual, does have)
///   a genuinely different channel count from its own playback interface.
///   These four report the *capture* direction's own format instead --
///   identical to `audio_sample_rate`/`audio_channels_and_bits` for
///   ES1370/HDA, where both directions already share one format, but a
///   real, independently discovered answer for USB. A tape recording from
///   any device must consult these, not `audio_sample_rate`/
///   `audio_channels_and_bits`, to interpret `audio_capture`'s bytes
///   correctly.
///
/// (A tape is still ring-0 code and technically free to ignore any of
/// these callbacks and touch hardware -- or `pmem` -- directly anyway --
/// nothing enforces this convention -- so this remains just that: a
/// convention, not a sandboxed boundary.)
extern "C" fn combined_tape_should_abort() -> u32 {
    relay_pending_print_output();
    (keyboard::tape_should_abort() != 0 || poll_remote_tape_abort()) as u32
}

/// A small FIFO of pending remote tape-input bytes, exactly analogous to
/// `console`'s own `RELAY_QUEUE` (output) or `interrupts`'s scancode queue
/// (input): a single `TERM_TAG_DATA` frame can carry many bytes at once
/// (a whole pasted or fast-typed line), but `combined_tape_read_key`
/// hands a tape back only one decoded event per call, so any bytes beyond
/// the first one a frame yields are queued here instead of discarded --
/// unlike the older `poll_remote_tape_abort`, which only ever needed to
/// detect one specific byte and could safely ignore everything else in a
/// frame's payload.
const TAPE_INPUT_QUEUE_LEN: usize = 256;
static mut TAPE_INPUT_QUEUE: [u8; TAPE_INPUT_QUEUE_LEN] = [0; TAPE_INPUT_QUEUE_LEN];
static mut TAPE_INPUT_HEAD: usize = 0;
static mut TAPE_INPUT_TAIL: usize = 0;

/// A `read_key` call and a tape's raw `nic_receive` call both ultimately
/// poll the same RTL8139 ring. When `read_key` happens to collect a chat
/// frame before the tape asks for it, retain it here instead of silently
/// discarding it. A four-frame FIFO handles short bursts and lets a protocol
/// stack drain packets in order without requiring the kernel to parse IP or
/// TCP itself.
const TAPE_NETWORK_QUEUE_LEN: usize = 4;
static mut TAPE_NETWORK_FRAMES: [[u8; nic::MAX_FRAME_LEN]; TAPE_NETWORK_QUEUE_LEN] =
    [[0; nic::MAX_FRAME_LEN]; TAPE_NETWORK_QUEUE_LEN];
static mut TAPE_NETWORK_FRAME_LENS: [usize; TAPE_NETWORK_QUEUE_LEN] = [0; TAPE_NETWORK_QUEUE_LEN];
static mut TAPE_NETWORK_HEAD: usize = 0;
static mut TAPE_NETWORK_TAIL: usize = 0;
static mut TAPE_NETWORK_COUNT: usize = 0;
/// A terminal abort can be discovered by `nic_receive` before `read_key`
/// runs. Preserve it until `read_key` returns the standard abort event.
static mut TAPE_REMOTE_ABORT_PENDING: bool = false;
/// Set by a tape that has actually asked for keyboard input. Until then,
/// ordinary terminal data seen by its raw NIC callback belongs to the
/// suspended shell editor, not to a callback the tape never consumes.
static mut TAPE_READ_KEY_USED: bool = false;

/// Queues `bytes` for later `tape_input_pop` calls, silently dropping
/// whatever doesn't fit -- the same "never corrupt accounting, just stop
/// accepting more" choice `pmem::free_big` and this file's own
/// `remember_peer` already make elsewhere, on the theory that a tape
/// falling behind this far during interactive use has bigger problems
/// than one dropped keystroke.
fn tape_input_push(bytes: &[u8]) {
    unsafe {
        for &byte in bytes {
            let next = (TAPE_INPUT_TAIL + 1) % TAPE_INPUT_QUEUE_LEN;
            if next == TAPE_INPUT_HEAD {
                break;
            }
            TAPE_INPUT_QUEUE[TAPE_INPUT_TAIL] = byte;
            TAPE_INPUT_TAIL = next;
        }
    }
}

/// Prepends the first byte of a terminal frame after `classify_remote_tape_frame`
/// has already queued its remainder. This preserves the wire's original
/// order when `nic_receive`, rather than `read_key`, consumed that frame.
fn tape_input_push_front(byte: u8) {
    unsafe {
        let previous = (TAPE_INPUT_HEAD + TAPE_INPUT_QUEUE_LEN - 1) % TAPE_INPUT_QUEUE_LEN;
        if previous != TAPE_INPUT_TAIL {
            TAPE_INPUT_HEAD = previous;
            TAPE_INPUT_QUEUE[TAPE_INPUT_HEAD] = byte;
        }
    }
}

fn tape_input_pop() -> Option<u8> {
    unsafe {
        if TAPE_INPUT_HEAD == TAPE_INPUT_TAIL {
            return None;
        }
        let byte = TAPE_INPUT_QUEUE[TAPE_INPUT_HEAD];
        TAPE_INPUT_HEAD = (TAPE_INPUT_HEAD + 1) % TAPE_INPUT_QUEUE_LEN;
        Some(byte)
    }
}

fn tape_network_frame_push(frame: &[u8]) {
    unsafe {
        if frame.len() <= nic::MAX_FRAME_LEN && TAPE_NETWORK_COUNT < TAPE_NETWORK_QUEUE_LEN {
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                (&raw mut TAPE_NETWORK_FRAMES[TAPE_NETWORK_TAIL]).cast::<u8>(),
                frame.len(),
            );
            TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_TAIL] = frame.len();
            TAPE_NETWORK_TAIL = (TAPE_NETWORK_TAIL + 1) % TAPE_NETWORK_QUEUE_LEN;
            TAPE_NETWORK_COUNT += 1;
        }
    }
}

fn tape_network_frame_pop(out: &mut [u8]) -> Option<usize> {
    unsafe {
        if TAPE_NETWORK_COUNT == 0 {
            return None;
        }
        let length = TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_HEAD];
        if out.len() < length {
            return None;
        }
        core::ptr::copy_nonoverlapping(
            (&raw const TAPE_NETWORK_FRAMES[TAPE_NETWORK_HEAD]).cast::<u8>(),
            out.as_mut_ptr(),
            length,
        );
        TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_HEAD] = 0;
        TAPE_NETWORK_HEAD = (TAPE_NETWORK_HEAD + 1) % TAPE_NETWORK_QUEUE_LEN;
        TAPE_NETWORK_COUNT -= 1;
        Some(length)
    }
}

enum RemoteTapeFrame {
    Input(u32),
    Consumed,
    PassThrough,
}

/// Separates a `term net` control/input frame from an ordinary raw Ethernet
/// frame while a tape is running. `read_key` turns `Input` into an event;
/// `nic_receive` queues it for that later `read_key` call. Everything else
/// remains available to a networking tape through `PassThrough`.
fn classify_remote_tape_frame(peer: &[u8; 6], frame: &[u8]) -> RemoteTapeFrame {
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        return RemoteTapeFrame::PassThrough;
    }
    let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
    // A networking tape owns chat/discovery framing itself. Leave discovery
    // probes in its raw receive stream so it can update its own peer table
    // and send exactly one protocol reply, rather than having this remote-
    // input bridge consume the frame before the tape ever sees it.
    if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
        return RemoteTapeFrame::PassThrough;
    }
    if &frame_source != peer {
        return RemoteTapeFrame::PassThrough;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            RemoteTapeFrame::Input(TAPE_EVENT_ABORT)
        }
        TERM_TAG_DATA => {
            let payload = &frame[15..];
            let end = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            let data = &payload[..end];
            if is_remote_abort(data) {
                return RemoteTapeFrame::Input(TAPE_EVENT_ABORT);
            }
            let Some((&first, rest)) = data.split_first() else {
                return RemoteTapeFrame::Consumed;
            };
            tape_input_push(rest);
            RemoteTapeFrame::Input(TAPE_EVENT_CHAR_BASE | u32::from(first))
        }
        _ => RemoteTapeFrame::PassThrough,
    }
}

/// The one non-zero "nothing to report" event `combined_tape_read_key`
/// never returns -- reserved so `0` unambiguously means "no input right
/// now", matching `should_abort`'s own "0 means keep going" convention.
const TAPE_EVENT_NONE: u32 = 0;
/// Returned by `combined_tape_read_key` for a "give up" signal -- a local
/// escape keypress or a remote `term_net` peer's abort byte/disconnect --
/// exactly the same trigger `combined_tape_should_abort` already reports
/// as `true`, just encoded as a distinct value here so one event can mean
/// either "stop" or "here is a character" without a tape needing a second
/// callback to tell them apart.
const TAPE_EVENT_ABORT: u32 = 1;
/// Everything from `TAPE_EVENT_CHAR_BASE` up encodes an ordinary input
/// byte: subtract this base back off to recover it. Starts above `0`/`1`
/// so every valid encoded byte (`0..=255`) stays distinguishable from
/// both reserved values without needing a second return channel.
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

/// Polls the network card once, on behalf of `combined_tape_read_key`,
/// for a byte typed by the `term_net` peer that started this tape --
/// mirrors `poll_remote_tape_abort`'s framing/discovery-probe/output-
/// relaying handling closely (see that function's doc comment for why
/// polling here is safe even mid-tape), but where that function only
/// ever needed to notice one specific byte and could discard the rest of
/// a payload, this one keeps every byte: the first is returned
/// immediately, any more are queued in `TAPE_INPUT_QUEUE` for the next
/// call. A payload that is just the abort byte (`REMOTE_ABORT_BYTE`, see
/// `is_remote_abort`; a longer payload starting with it is a cursor-key
/// sequence and is delivered as input) is still treated exactly like `poll_remote_tape_abort`
/// treats it -- the whole frame is dropped as a "give up" signal, never
/// fed to a tape as ordinary characters, so a peer's escape keypress can
/// never accidentally end up echoed into a tape's own input.
///
/// `poll_background_work` (print-relay draining, term-net output
/// relaying, and NIC watchdog recovery) runs first, unconditionally,
/// even when a queued `TAPE_INPUT_QUEUE` byte lets this call return
/// immediately afterward -- otherwise a fast-typing peer could keep this
/// function returning from the queue for several calls in a row without
/// ever reaching the network-polling code below, leaving whatever the
/// tape had already printed (e.g. a prompt) stuck in the relay queue for
/// longer than necessary, and a latched NIC fault unrecovered for as
/// long as the tape kept the peer's attention.
fn poll_remote_tape_input() -> Option<u32> {
    // See `poll_remote_tape_abort`'s identical reasoning: the session's own
    // interface, not necessarily the active one.
    let session_nic = active_term_nic();
    poll_background_work(session_nic.or_else(|| nic::get().ok()));
    if unsafe { TAPE_REMOTE_ABORT_PENDING } {
        unsafe { TAPE_REMOTE_ABORT_PENDING = false };
        return Some(TAPE_EVENT_ABORT);
    }
    if let Some(byte) = tape_input_pop() {
        return Some(TAPE_EVENT_CHAR_BASE | u32::from(byte));
    }
    if !console::relay_enabled() {
        return None;
    }
    let Some(peer) = (unsafe { ACTIVE_TERM_PEER }) else {
        return None;
    };
    let Some(nic) = session_nic else { return None };
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let Some(received) = nic.receive(&mut buffer) else {
        return None;
    };
    let frame = &buffer[..received];
    match classify_remote_tape_frame(&peer, frame) {
        RemoteTapeFrame::Input(event) => Some(event),
        RemoteTapeFrame::Consumed => None,
        RemoteTapeFrame::PassThrough => {
            tape_network_frame_push(frame);
            None
        }
    }
}

/// Copies the active NIC's MAC into a six-byte tape buffer. A tape calls this
/// once before networking so a missing or failed controller has one explicit,
/// checkable error path instead of appearing as an endless empty receive loop.
extern "C" fn tape_nic_mac(out: u32) -> u32 {
    if out == 0 {
        return 0;
    }
    let Ok(nic) = nic::get() else { return 0 };
    unsafe {
        core::slice::from_raw_parts_mut(out as *mut u8, 6).copy_from_slice(&nic.mac_address());
    }
    1
}

/// Copies the selected NIC's MAC into a six-byte tape buffer.
extern "C" fn tape_nic_mac_at(index: u32, out: u32) -> u32 {
    if out == 0 {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    unsafe {
        core::slice::from_raw_parts_mut(out as *mut u8, 6).copy_from_slice(&nic.mac_address());
    }
    1
}

/// Sends one complete raw Ethernet frame through the active NIC.
/// Invalid lengths and controller/transmit failures are reported as zero;
/// callers own their input buffer and should print a useful error themselves.
extern "C" fn tape_nic_send(frame: u32, length: u32) -> u32 {
    if frame == 0 || !(14..=nic::MAX_FRAME_LEN as u32).contains(&length) {
        return 0;
    }
    let Ok(nic) = nic::get() else { return 0 };
    let frame = unsafe { core::slice::from_raw_parts(frame as *const u8, length as usize) };
    nic.send(frame) as u32
}

/// Sends one complete raw Ethernet frame through a selected NIC.
extern "C" fn tape_nic_send_at(index: u32, frame: u32, length: u32) -> u32 {
    if frame == 0 || !(14..=nic::MAX_FRAME_LEN as u32).contains(&length) {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    let frame = unsafe { core::slice::from_raw_parts(frame as *const u8, length as usize) };
    nic.send(frame) as u32
}

/// Returns the number of initialized Ethernet interfaces.
extern "C" fn tape_nic_count() -> u32 {
    nic::count() as u32
}

/// Reports the maximum Ethernet frame length accepted by the kernel-owned
/// driver. A shared tape networking library uses this instead of duplicating
/// the RTL8139 limit, so its receive buffers stay synchronized with the
/// kernel's hardware boundary.
extern "C" fn tape_nic_max_frame_len() -> u32 {
    nic::MAX_FRAME_LEN as u32
}

/// Returns the monotonic PIT tick counter used by the kernel. Protocol
/// retransmission and connection deadlines should use this callback rather
/// than programming a timer or reading PIT hardware from a tape.
/// Tape ABI callback `wait_for_event`: sleeps until the next interrupt (a
/// received frame, a keypress, or at worst the 100 Hz PIT tick, so at most
/// about 10 ms). A tape calls it after a polling pass that found nothing to do,
/// instead of spinning at 100% CPU. It may return early or for no visible
/// reason, so the tape must simply poll again; it never reports a result. If
/// any NIC lacks a receive interrupt it returns at once (see `idle_wait`).
/// Tape ABI callback `set_input_mode` (slot 37): bit 0 set switches `read_key`
/// to raw keys, bit 0 clear (the default for every tape start) restores the
/// original behaviour. In the default mode `read_key` erases the previous
/// character on screen itself whenever it returns Backspace (8), which keeps a
/// simple line reader to "decrement my length". A tape that moves the cursor
/// (see `tape_rt::KeyDecoder`) must place the cursor and redraw on its own, so
/// it asks for raw keys; Backspace is then just the byte 8.
extern "C" fn tape_set_input_mode(mode: u32) {
    unsafe { TAPE_RAW_KEYS = mode & 1 != 0 };
}

/// Set by `tape_set_input_mode`; cleared at the start of every `tape_run`.
static mut TAPE_RAW_KEYS: bool = false;

extern "C" fn tape_wait_for_event() {
    crate::debug::watchdog_pet();
    idle_wait();
}

/// Tape ABI slot 38: microseconds since boot (`interrupts::micros`), for
/// rate pacing and latency measurement finer than the 10 ms tick.
extern "C" fn tape_micros() -> u64 {
    crate::debug::watchdog_pet();
    interrupts::micros()
}

extern "C" fn tape_ticks() -> u64 {
    crate::debug::watchdog_pet(); // a tape polling the clock is alive
    interrupts::ticks()
}

/// Reports the active audio device's fixed sample rate in Hz, or zero if no
/// device is present. A streaming tape must call this (and
/// `tape_audio_channels_and_bits`) before `tape_audio_submit`, and must
/// produce exactly this format -- the kernel does no resampling or
/// requantization of its own (see `audio.rs`'s own module doc comment).
extern "C" fn tape_audio_sample_rate() -> u32 {
    match audio::get() {
        Ok(device) => device.format().sample_rate,
        Err(_) => 0,
    }
}

/// Packs the active audio device's channel count and bit depth into one
/// `u32` (channels in the low byte, bits-per-sample in the next byte),
/// avoiding a sixth trailing ABI parameter for what is otherwise always
/// consulted alongside `tape_audio_sample_rate`. Returns zero if no device
/// is present, which never collides with a real format (every real device
/// has at least one channel and a nonzero bit depth).
extern "C" fn tape_audio_channels_and_bits() -> u32 {
    match audio::get() {
        Ok(device) => {
            let format = device.format();
            format.channels as u32 | ((format.bits_per_sample as u32) << 8)
        }
        Err(_) => 0,
    }
}

/// Submits raw PCM samples, in the active device's own format, for
/// playback. Returns how many bytes were actually accepted; a short count
/// (including zero) means the device's ring buffer is temporarily full and
/// the caller should retry the remainder on a later poll, the same
/// short-write convention `tape_nic_send`'s own ABI already uses elsewhere.
extern "C" fn tape_audio_submit(data: u32, length: u32) -> u32 {
    if data == 0 || length == 0 {
        return 0;
    }
    let Ok(device) = audio::get() else {
        return 0;
    };
    let bytes = unsafe { core::slice::from_raw_parts(data as *const u8, length as usize) };
    device.submit(bytes) as u32
}

/// Copies out captured (recorded) PCM samples, in the active device's own
/// format, up to `capacity` bytes into `out` -- the ADC counterpart to
/// `tape_audio_submit`, mirroring `tape_nic_receive`'s own "copy however
/// much is ready" shape rather than `tape_audio_submit`'s "accept however
/// much fits" one. Returns how many bytes were actually copied; fewer than
/// `capacity` (including zero) simply means fewer samples have been
/// captured so far, not an error -- the caller should keep polling.
extern "C" fn tape_audio_capture(out: u32, capacity: u32) -> u32 {
    if out == 0 || capacity == 0 {
        return 0;
    }
    let Ok(device) = audio::get() else {
        return 0;
    };
    let buffer =
        unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    device.capture(buffer) as u32
}

/// Returns the number of initialized audio devices -- the audio-side
/// counterpart to `tape_nic_count`, added so a multichannel-streaming tape
/// can drive every discovered Sound Blaster PCI (ES1370-family) card as an
/// independent stereo pair, rather than only ever the first one
/// (`tape_audio_sample_rate`/`tape_audio_submit`/... keep meaning "device
/// 0" unchanged, for a tape that only ever expects one card).
extern "C" fn tape_audio_count() -> u32 {
    audio::count() as u32
}

/// The `_at`-suffixed counterpart to `tape_audio_sample_rate`, reporting
/// one selected device's fixed sample rate instead of always device 0.
/// Returns zero for an out-of-range index, exactly like `tape_nic_mac_at`
/// treats one.
extern "C" fn tape_audio_sample_rate_at(index: u32) -> u32 {
    match audio::get_at(index as usize) {
        Some(device) => device.format().sample_rate,
        None => 0,
    }
}

/// The `_at`-suffixed counterpart to `tape_audio_channels_and_bits`.
extern "C" fn tape_audio_channels_and_bits_at(index: u32) -> u32 {
    match audio::get_at(index as usize) {
        Some(device) => {
            let format = device.format();
            format.channels as u32 | ((format.bits_per_sample as u32) << 8)
        }
        None => 0,
    }
}

/// The `_at`-suffixed counterpart to `tape_audio_submit`, feeding one
/// selected device's own network-fed ring instead of always device 0 --
/// this is what actually lets a multichannel-streaming tape drive several
/// cards' DAC1 channels simultaneously, each with its own independent
/// jitter buffer and playback timing.
extern "C" fn tape_audio_submit_at(index: u32, data: u32, length: u32) -> u32 {
    if data == 0 || length == 0 {
        return 0;
    }
    let Some(device) = audio::get_at(index as usize) else {
        return 0;
    };
    let bytes = unsafe { core::slice::from_raw_parts(data as *const u8, length as usize) };
    device.submit(bytes) as u32
}

/// The `_at`-suffixed counterpart to `tape_audio_capture`.
extern "C" fn tape_audio_capture_at(index: u32, out: u32, capacity: u32) -> u32 {
    if out == 0 || capacity == 0 {
        return 0;
    }
    let Some(device) = audio::get_at(index as usize) else {
        return 0;
    };
    let buffer =
        unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    device.capture(buffer) as u32
}

/// The *capture*-direction counterpart to `tape_audio_sample_rate`. For
/// ES1370/HDA this reports the same number `tape_audio_sample_rate`
/// already does -- their real hardware genuinely shares one format
/// between playback and capture -- but a USB Audio Class device is not
/// guaranteed to (the real TerraTec Aureon Dual's capture interface is
/// mono against a stereo playback format), so a tape recording from any
/// device must consult this rather than assuming `tape_audio_sample_rate`
/// describes captured bytes too. Returns zero if no device is present.
extern "C" fn tape_audio_capture_sample_rate() -> u32 {
    match audio::get() {
        Ok(device) => device.capture_format().sample_rate,
        Err(_) => 0,
    }
}

/// The capture-direction counterpart to `tape_audio_channels_and_bits` --
/// see that callback's own doc comment for the packing convention.
extern "C" fn tape_audio_capture_channels_and_bits() -> u32 {
    match audio::get() {
        Ok(device) => {
            let format = device.capture_format();
            format.channels as u32 | ((format.bits_per_sample as u32) << 8)
        }
        Err(_) => 0,
    }
}

/// The `_at`-suffixed counterpart to `tape_audio_capture_sample_rate`.
extern "C" fn tape_audio_capture_sample_rate_at(index: u32) -> u32 {
    match audio::get_at(index as usize) {
        Some(device) => device.capture_format().sample_rate,
        None => 0,
    }
}

/// The `_at`-suffixed counterpart to `tape_audio_capture_channels_and_bits`.
extern "C" fn tape_audio_capture_channels_and_bits_at(index: u32) -> u32 {
    match audio::get_at(index as usize) {
        Some(device) => {
            let format = device.capture_format();
            format.channels as u32 | ((format.bits_per_sample as u32) << 8)
        }
        None => 0,
    }
}

/// Copies one raw Ethernet frame into `out`, returning its full length or
/// zero when none is waiting, the caller buffer is too small, or the NIC
/// cannot be used. A terminal frame consumed here is queued for `read_key`
/// rather than exposed as chat traffic, preserving remote input while a
/// networking tape runs inside `term net`. A too-small-buffer poll leaves
/// the frame in the deferred FIFO for a later retry.
extern "C" fn tape_nic_receive(out: u32, capacity: u32) -> u32 {
    crate::debug::watchdog_pet();
    if out == 0 || capacity as usize > nic::MAX_FRAME_LEN {
        return 0;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    if let Some(length) = tape_network_frame_pop(out) {
        return length as u32;
    }
    let Ok(nic) = nic::get() else { return 0 };
    let mut frame = [0u8; nic::MAX_FRAME_LEN];
    let Some(length) = nic.receive(&mut frame) else {
        return 0;
    };

    if console::relay_enabled() {
        if let Some(peer) = unsafe { ACTIVE_TERM_PEER } {
            if !unsafe { TAPE_READ_KEY_USED }
                && frame[..length].len() >= 15
                && frame[12..14] == CHAT_ETHERTYPE
                && frame[6..12] == peer
                && frame[14] == TERM_TAG_DATA
                && !is_remote_abort(&frame[15..length])
            {
                defer_active_term_input(&frame[..length]);
                return 0;
            }
            match classify_remote_tape_frame(&peer, &frame[..length]) {
                RemoteTapeFrame::Input(TAPE_EVENT_ABORT) => {
                    unsafe { TAPE_REMOTE_ABORT_PENDING = true };
                    return 0;
                }
                RemoteTapeFrame::Input(event) => {
                    tape_input_push_front((event & 0xff) as u8);
                    return 0;
                }
                RemoteTapeFrame::Consumed => return 0,
                RemoteTapeFrame::PassThrough => {}
            }
        }
    }
    if out.len() < length {
        // Keep the frame available for a retry with the advertised maximum
        // size instead of consuming it because this poll used a small
        // caller-provided buffer.
        tape_network_frame_push(&frame[..length]);
        return 0;
    }
    out[..length].copy_from_slice(&frame[..length]);
    length as u32
}

/// Receives one frame from a selected NIC. The active interface uses the
/// existing terminal-aware path; other interfaces are exposed directly to
/// the tape without being mistaken for the active `term net` input channel.
extern "C" fn tape_nic_receive_at(index: u32, out: u32, capacity: u32) -> u32 {
    crate::debug::watchdog_pet();
    if out == 0 || capacity as usize > nic::MAX_FRAME_LEN {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    if index as usize == nic::active_index() {
        return tape_nic_receive(out, capacity);
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    let mut frame = [0u8; nic::MAX_FRAME_LEN];
    let Some(length) = nic.receive(&mut frame) else {
        return 0;
    };
    if out.len() < length {
        return 0;
    }
    out[..length].copy_from_slice(&frame[..length]);
    length as u32
}

/// Enables or disables receive-all mode for one selected NIC. This is
/// intentionally per-interface: a multi-port layer-2 tape must see frames
/// addressed to hosts behind its other ports, while ordinary protocol tapes
/// should keep the backend's normal address filtering.
extern "C" fn tape_nic_set_promiscuous_at(index: u32, enabled: u32) -> u32 {
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    nic.set_promiscuous(enabled != 0);
    1
}

/// `extern "C" fn() -> u32` callback giving a tape actual interactive
/// input, not just "give up" detection -- passed as the tape ABI's fifth
/// argument (see `tape_run`'s doc comment). Non-blocking: called once per
/// polling iteration by a tape's own loop, exactly like `should_abort`,
/// returning immediately either way.
///
/// Return value, checked in this order:
/// - `0` (`TAPE_EVENT_NONE`): nothing waiting right now; keep polling.
/// - `1` (`TAPE_EVENT_ABORT`): the same "give up" signal
///   `combined_tape_should_abort` reports as `true` -- a local escape
///   keypress, or (if a `term_net` session started this tape) the remote
///   peer's abort byte or a disconnect.
/// - `0x100` (`TAPE_EVENT_CHAR_BASE`) or above: an ordinary input byte,
///   recovered with `value - 0x100` (or `value & 0xff`, equivalently) --
///   from whichever source produced it, local keyboard or an active
///   `term_net` session's `TERM_TAG_DATA` payload, with no way for a tape
///   to tell which after the fact (nor any reason to want to: both are
///   just "the user typed this").
///
/// A decoded backspace (ASCII `8`) also triggers `console::
/// erase_last_character()` here, before returning the event -- the one
/// piece of this a tape cannot safely do itself even with the `putc`
/// callback, since plain `console::write_byte` does not special-case
/// `0x08` the way `erase_last_character` does (rewinding the VGA cursor
/// and emitting the `\x08 \x08` sequence a real serial terminal expects).
/// Doing it here, unconditionally, keeps a tape's own backspace handling
/// as simple as "decrement my buffer length" regardless of whether the
/// keystroke was typed locally or arrived over the network -- exactly
/// mirroring how `LineEditor::feed` already handles backspace for the
/// ordinary shell prompt.
///
/// Shares its two input sources with `should_abort`
/// (`keyboard::poll_term_event`'s scancode queue, and a `term_net`
/// session's own network frames) but is a genuinely separate consumer of
/// each: calling both callbacks from the same tape in the same loop will
/// race them for input and intermittently lose keystrokes to whichever
/// one happens to poll first. A tape wanting real input should call only
/// this one -- checking for `TAPE_EVENT_ABORT` covers everything
/// `should_abort` would have told it anyway -- not both.
extern "C" fn combined_tape_read_key() -> u32 {
    unsafe { TAPE_READ_KEY_USED = true };
    if let Some(event) = keyboard::poll_term_event() {
        let code = match event {
            keyboard::KeyEvent::Escape => TAPE_EVENT_ABORT,
            keyboard::KeyEvent::Char(byte) => TAPE_EVENT_CHAR_BASE | u32::from(byte),
            // Meaningless outside `term_printer`'s own typing loop; no
            // tape ABI event corresponds to it, so just poll again.
            keyboard::KeyEvent::KillLine => return combined_tape_read_key(),
        };
        if code == TAPE_EVENT_CHAR_BASE | 8 && !unsafe { TAPE_RAW_KEYS } {
            console::erase_last_character();
        }
        return code;
    }
    match poll_remote_tape_input() {
        Some(code) => {
            if code == TAPE_EVENT_CHAR_BASE | 8 && !unsafe { TAPE_RAW_KEYS } {
                console::erase_last_character();
            }
            code
        }
        None => TAPE_EVENT_NONE,
    }
}

/// Hands a tape a fresh, physically contiguous block of at least `bytes`
/// bytes (rounded up to whole 1 MiB chunks), from `pmem`'s separate,
/// coarse-grained "big pool" (`pmem::allocate_big_contiguous`/`free_big`)
/// -- everything from `pmem::BIG_POOL_BASE` up to `pmem::BIG_POOL_END`
/// (2 GiB by default), entirely apart from the fine 4 KiB frame pool
/// `palloc`/`pfree`/the fixed 4 MiB tape region draw from. Passed as the
/// tape ABI's third argument (see `tape_run`'s doc comment). See
/// `pmem::TRACKED_BYTES`'s doc comment for why this second, coarser pool
/// exists at all instead of simply growing the fine bitmap: doing that
/// would have grown `kernel.bin` itself (a flat binary's `.bss` is baked
/// into the file) beyond the old single-transfer kernel-load ceiling.
///
/// Returns physical address `0` on failure (not enough contiguous free
/// memory) -- never a valid chunk address, since nothing below
/// `pmem::BIG_POOL_BASE` is ever handed out by this callback. Unlike a
/// `static mut` array (whose bytes a flat binary's own loading always
/// zeroes, see `entry.S`'s `.bss` zeroing stub in every Rust tape), these
/// chunks are **not** zeroed -- they may hold whatever a previous
/// allocation left behind -- so a tape relying on its buffer starting at
/// zero must zero it itself (see `tapes/bigsieve/src/main.rs`).
///
/// A tape that calls this must later call `tape_free` with the exact same
/// `bytes` value (or leak those chunks forever) -- there is no automatic
/// cleanup on return, the same "trusted exactly as much as any other
/// kernel code" reasoning `tape_run`'s own doc comment already gives for
/// tapes in general, and the same manual-only contract `palloc`/`pfree`
/// already have from the shell.
extern "C" fn tape_alloc(bytes: u32) -> u32 {
    let chunks = bytes.div_ceil(pmem::BIG_CHUNK_SIZE) as usize;
    pmem::allocate_big_contiguous(chunks).unwrap_or(0)
}

/// `tape_alloc`'s exact opposite: releases the same `bytes` (rounded up
/// to whole chunks the same way `tape_alloc` rounded when allocating)
/// back to the big pool, starting at `address`. Passed as the tape ABI's
/// fourth argument (see `tape_run`'s doc comment). `pmem::free_big`
/// already silently ignores an out-of-range or already-free chunk rather
/// than corrupting accounting, so a slightly-wrong `bytes` value here is
/// safe, if still a bug worth fixing in the tape itself.
extern "C" fn tape_free(address: u32, bytes: u32) {
    let chunks = bytes.div_ceil(pmem::BIG_CHUNK_SIZE) as usize;
    pmem::free_big(address, chunks);
}

fn tape_boot_info() -> Option<&'static BootInfo> {
    let pointer = unsafe { TAPE_BOOT_INFO };
    (!pointer.is_null()).then(|| unsafe { &*pointer })
}

/// A FOSFS name is at most 16 bytes; a `diskN/` selector prefix (matching
/// `tape run`'s `@diskN/NAME` and the shell's `diskN` command selectors)
/// adds a handful more, so this is a generous round bound, not a precise
/// one -- the real length/charset checks happen on the two halves
/// separately once split, in [`tape_fs_reference`].
const TAPE_FS_REFERENCE_CAPACITY: usize = 32;

/// Resolves a tape-supplied file reference into a disk device and a plain
/// FOSFS name, exactly mirroring `tape run`'s `@diskN/NAME` convention: a
/// bare name (no `/`) always means the boot disk, matching every existing
/// tape's behavior before this existed; a `diskN/NAME` reference selects an
/// attached secondary disk instead. A literal `/` is unambiguous as the
/// separator since FOSFS names may only contain letters, digits, `.`, `_`,
/// and `-`, so a real file name can never collide with it.
fn tape_fs_reference(pointer: u32, length: u32) -> Option<(disk::Device, &'static [u8])> {
    let length = usize::try_from(length).ok()?;
    if pointer == 0 || length == 0 || length > TAPE_FS_REFERENCE_CAPACITY {
        return None;
    }
    let reference = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    let (device, name) = match reference.iter().position(|&byte| byte == b'/') {
        Some(slash) => {
            let device = strip_disk_selector(&reference[..slash])?;
            (device, &reference[slash + 1..])
        }
        None => (disk::Device::Boot, reference),
    };
    if name.is_empty()
        || name.len() > 16
        || !name
            .iter()
            .all(|byte| matches!(*byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some((device, name))
}

fn tape_fs_buffer(pointer: u32, length: u32) -> Option<&'static mut [u8]> {
    let length = usize::try_from(length).ok()?;
    if length == 0 {
        return Some(&mut []);
    }
    (pointer != 0).then(|| unsafe {
        core::slice::from_raw_parts_mut(pointer as *mut u8, length)
    })
}

/// Returns a FOSFS file's exact byte length through the append-only tape ABI.
/// The output length is written unaligned because tape buffers are not
/// required to have a particular alignment. `name` may be a bare FOSFS name
/// (boot disk) or a `diskN/NAME` reference (an attached secondary disk); see
/// [`tape_fs_reference`].
extern "C" fn tape_fs_file_length(name: u32, name_length: u32, out_length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    if out_length == 0 {
        return TAPE_FS_INVALID_ARGUMENT;
    }
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => {
            unsafe { core::ptr::write_unaligned(out_length as *mut u64, length) };
            TAPE_FS_OK
        }
        Ok(None) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Reads a complete FOSFS file into a tape buffer. The output length is
/// written only on success; callers should query `tape_fs_file_length` first
/// when they need to allocate a precisely sized buffer. `name` may be a bare
/// FOSFS name (boot disk) or a `diskN/NAME` reference (an attached secondary
/// disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_read(
    name: u32,
    name_length: u32,
    buffer: u32,
    capacity: u32,
    out_length: u32,
) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    if out_length == 0 {
        return TAPE_FS_INVALID_ARGUMENT;
    }
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => return TAPE_FS_NOT_FOUND,
        Err(_) => return TAPE_FS_DISK_ERROR,
    };
    if length > u64::from(capacity) {
        return TAPE_FS_BUFFER_TOO_SMALL;
    }
    let Some(buffer) = tape_fs_buffer(buffer, capacity) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(read_length)) => {
            unsafe { core::ptr::write_unaligned(out_length as *mut u32, read_length as u32) };
            TAPE_FS_OK
        }
        Ok(None) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Replaces or creates one complete FOSFS file from a tape buffer. `name` may
/// be a bare FOSFS name (boot disk) or a `diskN/NAME` reference (an attached
/// secondary disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_write(name: u32, name_length: u32, buffer: u32, length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(buffer) = tape_fs_buffer(buffer, length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::write(info, device, name, buffer) {
        Ok(fosfs::WriteOutcome::Written) => TAPE_FS_OK,
        Ok(fosfs::WriteOutcome::InvalidName) => TAPE_FS_INVALID_ARGUMENT,
        Ok(fosfs::WriteOutcome::DirectoryFull) => TAPE_FS_DIRECTORY_FULL,
        Ok(fosfs::WriteOutcome::DataFull) => TAPE_FS_DATA_FULL,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Removes one FOSFS file, returning `TAPE_FS_NOT_FOUND` when absent. `name`
/// may be a bare FOSFS name (boot disk) or a `diskN/NAME` reference (an
/// attached secondary disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_remove(name: u32, name_length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::remove(info, device, name) {
        Ok(true) => TAPE_FS_OK,
        Ok(false) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

enum TapeArgumentSource<'a> {
    Direct(&'a [u8]),
    File(disk::Device, &'a [u8]),
    RemoteFile([u8; 6], u8, &'a [u8]),
    Invalid,
}

/// `@NAME` reads the config file from the boot disk, matching the historical
/// syntax; `@diskN/NAME` reads it from an attached secondary disk instead;
/// `@HOST[:diskN]/NAME` fetches it over the network from a discovered
/// peer's own disk instead (see `parse_remote_host_token`, NETFS). A
/// literal `/` is unambiguous as the disk-or-host/name separator here --
/// FOSFS names may only contain letters, digits, `.`, `_`, and `-`, so `/`
/// can never appear in a real file name to collide with this.
fn tape_argument_source(arguments: &[u8]) -> TapeArgumentSource<'_> {
    let (first, remainder) = take_word(arguments);
    // The tape's own name is an optional leading word, as `tape run router @FILE`
    // always allowed; `nettest` reads its settings file the same way.
    let (candidate, trailing) = if first == b"router" || first == b"nettest" {
        take_word(remainder)
    } else {
        (first, remainder)
    };
    if !candidate.starts_with(b"@") {
        return TapeArgumentSource::Direct(arguments);
    }
    if candidate.len() == 1 || !trailing.is_empty() {
        return TapeArgumentSource::Invalid;
    }
    let reference = &candidate[1..];
    match reference.iter().position(|&byte| byte == b'/') {
        Some(slash) => {
            let head = &reference[..slash];
            let name = &reference[slash + 1..];
            if name.is_empty() {
                return TapeArgumentSource::Invalid;
            }
            if let Some(device) = strip_disk_selector(head) {
                return TapeArgumentSource::File(device, name);
            }
            match parse_remote_host_token(head) {
                Some((peer, remote_disk_number)) => {
                    TapeArgumentSource::RemoteFile(peer, remote_disk_number, name)
                }
                None => TapeArgumentSource::Invalid,
            }
        }
        None => {
            if reference.is_empty() {
                return TapeArgumentSource::Invalid;
            }
            TapeArgumentSource::File(disk::Device::Boot, reference)
        }
    }
}

#[inline(never)]
fn tape_run(info: &BootInfo, arguments: &[u8]) {
    let Some((address, length)) = tape_get() else {
        println!("No tape loaded; use 'tape load disk NAME' or 'tape load net' first.");
        return;
    };
    let mut file_buffer = [0u8; TAPE_ARGUMENT_FILE_CAPACITY];
    let arguments = match tape_argument_source(arguments) {
        TapeArgumentSource::Direct(arguments) => arguments,
        TapeArgumentSource::Invalid => {
            println!("Usage: tape run [ARGS...] or tape run router @CONFIG_FILE");
            println!("(or @diskN/CONFIG_FILE or @HOST[:diskN]/CONFIG_FILE)");
            return;
        }
        TapeArgumentSource::File(device, name) => {
            let length = match fosfs::file_length(info, device, name) {
                Ok(Some(length)) => length,
                Ok(None) => {
                    println!(
                        "Router configuration file '{}' was not found on disk{}.",
                        core::str::from_utf8(name).unwrap_or("?"),
                        device_number(device)
                    );
                    return;
                }
                Err(error) => {
                    println!("FOSFS disk error while reading router configuration: {error:?}");
                    return;
                }
            };
            if length > TAPE_ARGUMENT_FILE_CAPACITY as u64 {
                println!(
                    "Router configuration file is too large: {length} bytes (maximum {TAPE_ARGUMENT_FILE_CAPACITY})."
                );
                return;
            }
            let length = length as usize;
            match fosfs::read(info, device, name, &mut file_buffer[..length]) {
                Ok(Some(_)) => &file_buffer[..length],
                Ok(None) => {
                    println!(
                        "Router configuration file '{}' was not found on disk{}.",
                        core::str::from_utf8(name).unwrap_or("?"),
                        device_number(device)
                    );
                    return;
                }
                Err(error) => {
                    println!("FOSFS disk error while reading router configuration: {error:?}");
                    return;
                }
            }
        }
        TapeArgumentSource::RemoteFile(peer, remote_disk_number, name) => {
            let Some(nic) = nic_or_report() else {
                return;
            };
            let own_mac = nic.mac_address();
            match netfs_get(nic, peer, own_mac, remote_disk_number, name, &mut file_buffer) {
                Ok(received) => &file_buffer[..received],
                Err(error) => {
                    println!("NETFS fetch of router configuration failed: {error}");
                    return;
                }
            }
        }
    };
    println!("Running {length}-byte tape at 0x{address:05x} in ring 0, no");
    println!("memory protection or fault recovery.");
    // No tape's pending remote keystrokes, abort, or raw frame belongs to
    // the next one. Clear this cross-callback state before every entry,
    // including tapes launched locally after a prior `term net` session.
    unsafe {
        TAPE_INPUT_HEAD = 0;
        TAPE_INPUT_TAIL = 0;
        TAPE_NETWORK_HEAD = 0;
        TAPE_NETWORK_TAIL = 0;
        TAPE_NETWORK_COUNT = 0;
        TAPE_REMOTE_ABORT_PENDING = false;
        TAPE_READ_KEY_USED = false;
        TAPE_RAW_KEYS = false;
    }
    type TapeEntry = extern "C" fn(
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
        audio_sample_rate: extern "C" fn() -> u32,
        audio_channels_and_bits: extern "C" fn() -> u32,
        audio_submit: extern "C" fn(u32, u32) -> u32,
        audio_capture: extern "C" fn(u32, u32) -> u32,
        audio_count: extern "C" fn() -> u32,
        audio_sample_rate_at: extern "C" fn(u32) -> u32,
        audio_channels_and_bits_at: extern "C" fn(u32) -> u32,
        audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
        audio_capture_at: extern "C" fn(u32, u32, u32) -> u32,
        audio_capture_sample_rate: extern "C" fn() -> u32,
        audio_capture_channels_and_bits: extern "C" fn() -> u32,
        audio_capture_sample_rate_at: extern "C" fn(u32) -> u32,
        audio_capture_channels_and_bits_at: extern "C" fn(u32) -> u32,
        wait_for_event: extern "C" fn(),
        set_input_mode: extern "C" fn(u32),
        micros: extern "C" fn() -> u64,
    );
    let entry: TapeEntry =
        unsafe { core::mem::transmute::<usize, TapeEntry>(tape_entry() as usize) };
    unsafe {
        TAPE_BOOT_INFO = info as *const BootInfo;
    }
    // A tape may compute for a long time without calling back into the
    // kernel; the watchdog allows it a much longer silence (see `debug.rs`).
    crate::debug::watchdog_tape_running(true);
    entry(
        console::print_byte,
        combined_tape_should_abort,
        tape_alloc,
        tape_free,
        combined_tape_read_key,
        arguments.as_ptr() as u32,
        arguments.len() as u32,
        tape_nic_mac,
        tape_nic_send,
        tape_nic_receive,
        tape_nic_max_frame_len,
        tape_ticks,
        tape_nic_count,
        tape_nic_mac_at,
        tape_nic_send_at,
        tape_nic_receive_at,
        tape_nic_set_promiscuous_at,
        console::terminal_position,
        tape_fs_file_length,
        tape_fs_read,
        tape_fs_write,
        tape_fs_remove,
        tape_audio_sample_rate,
        tape_audio_channels_and_bits,
        tape_audio_submit,
        tape_audio_capture,
        tape_audio_count,
        tape_audio_sample_rate_at,
        tape_audio_channels_and_bits_at,
        tape_audio_submit_at,
        tape_audio_capture_at,
        tape_audio_capture_sample_rate,
        tape_audio_capture_channels_and_bits,
        tape_audio_capture_sample_rate_at,
        tape_audio_capture_channels_and_bits_at,
        tape_wait_for_event,
        tape_set_input_mode,
        tape_micros,
    );
    crate::debug::watchdog_tape_running(false);
    unsafe {
        TAPE_BOOT_INFO = core::ptr::null();
    }
    console::reset_terminal_output();
    println!("Tape returned; back at the fos shell.");
}

#[inline(never)]
fn tape_load_command(info: &BootInfo, arguments: &[u8]) {
    let (kind, remainder) = take_word_or_slash(arguments);
    match kind {
        b"net" if remainder.is_empty() => tape_load_net(),
        b"disk" => {
            let (name, extra) = take_word(remainder);
            if name.is_empty() || !extra.is_empty() {
                println!("Usage: tape load disk NAME");
                return;
            }
            tape_load_disk(info, disk::Device::Boot, name, true);
        }
        // `disk` above is really just `disk1` spelled the historical way;
        // any other `diskN` word (`disk2`, `disk3`, ...) names an attached
        // secondary disk exactly like every other disk command's selector.
        _ if strip_disk_selector(kind).is_some() => {
            let device = strip_disk_selector(kind).unwrap();
            let (name, extra) = take_word(remainder);
            if name.is_empty() || !extra.is_empty() {
                println!("Usage: tape load diskN NAME");
                return;
            }
            tape_load_disk(info, device, name, true);
        }
        // A bare hostname or `nic peers` number (optionally with `:diskN`)
        // fetches the file over the network from that peer instead, via
        // NETFS -- see `parse_remote_host_token`/`netfs_get`.
        _ if parse_remote_host_token(kind).is_some() => {
            let (peer, remote_disk_number) = parse_remote_host_token(kind).unwrap();
            let (name, extra) = take_word(remainder);
            if name.is_empty() || !extra.is_empty() {
                println!("Usage: tape load HOST[:diskN] NAME");
                return;
            }
            tape_load_remote(peer, remote_disk_number, name);
        }
        _ => println!("Usage: tape load [net|disk NAME|diskN NAME|HOST[:diskN] NAME]"),
    }
}

/// Fetches a file from a discovered peer's own disk over NETFS and loads
/// it into the fixed tape buffer, the remote counterpart to
/// `tape_load_disk`. Reuses the same `pmem::TAPE_BASE`/`TAPE_LEN` region
/// as every other tape-load path, so `netfs_get` can write directly into
/// it rather than staging through a second buffer.
#[inline(never)]
fn tape_load_remote(peer: [u8; 6], remote_disk_number: u8, name: &[u8]) {
    let Some(nic) = nic_or_report() else {
        return;
    };
    let own_mac = nic.mac_address();
    let Some(address) = allocate_tape_buffer(pmem::TAPE_LEN) else {
        return;
    };
    let destination =
        unsafe { core::slice::from_raw_parts_mut(address as *mut u8, pmem::TAPE_LEN as usize) };
    match netfs_get(nic, peer, own_mac, remote_disk_number, name, destination) {
        Ok(length) => {
            if length == 0 {
                println!(
                    "'{}' is empty; nothing to load.",
                    core::str::from_utf8(name).unwrap_or("?")
                );
                return;
            }
            if !install_tape(address, length as u32) {
                return;
            }
            println!(
                "Loaded {length} byte(s) from '{}' over NETFS at 0x{address:05x}.",
                core::str::from_utf8(name).unwrap_or("?")
            );
            println!("'tape run' to execute it.");
        }
        Err(error) => println!("NETFS load failed: {error}"),
    }
}

/// Loads a complete flat FOSFS file straight into the fixed tape buffer.
/// Multi-sector files are read one BIOS sector at a time, and the file must
/// fit the same fixed region used by `tape load net`.
#[inline(never)]
fn tape_load_disk(info: &BootInfo, device: disk::Device, name: &[u8], hint: bool) -> bool {
    match fosfs::file_length(info, device, name) {
        Ok(Some(length)) if length == 0 => {
            println!(
                "'{}' is empty; nothing to load.",
                core::str::from_utf8(name).unwrap_or("?")
            );
        }
        Ok(Some(length)) if length > u32::MAX as u64 => {
            println!("Tape is too large for this 32-bit tape loader.");
        }
        Ok(Some(length)) => {
            let length = length as usize;
            let Some(address) = allocate_tape_buffer(length as u32) else {
                return false;
            };
            let destination =
                unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
            match fosfs::read(info, device, name, destination) {
                Ok(Some(_)) => {
                    if !install_tape(address, length as u32) {
                        return false;
                    }
                    println!(
                        "Loaded {length} byte(s) from '{}' at 0x{address:05x}.",
                        core::str::from_utf8(name).unwrap_or("?")
                    );
                    // A bare command name (`run_external_command`) runs the
                    // tape straight away, so the hint would be misleading.
                    if hint {
                        println!("'tape run' to execute it.");
                    }
                    return true;
                }
                Ok(None) => println!("File not found."),
                Err(error) => println!("FOSFS disk error: {error:?}"),
            }
        }
        Ok(None) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
    false
}

/// Writes the currently loaded tape's exact bytes out to FOSFS, the
/// missing counterpart to `tape load disk`/`tape load net`: a tape
/// received over the network (`tape load net`) previously had no way to
/// persist it, so a fresh `tape load net` transfer -- with its real,
/// per-run pacing/reliability cost -- was the only way to run it again
/// after a reboot. Works identically regardless of how the tape got into
/// the fixed buffer in the first place (disk load, network load, or even
/// right after an earlier `tape save`), since it just re-reads whatever
/// `tape_get()` currently reports, exactly the same "whatever's resident"
/// contract `tape run` itself already relies on. Unlike `copy_file`, this
/// never reuses the buffer as scratch space for something else, so the
/// loaded tape's own bookkeeping (`TAPE`/`TAPE_ENTRY`/`TAPE_HANDLE`) is
/// left completely untouched -- `tape run` still works immediately
/// afterward with no need to `tape load` it back first.
#[inline(never)]
fn tape_save_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word_or_slash(arguments);
    let (destination, name, remainder) = match parse_save_destination(name) {
        Some(destination) => {
            let (name, remainder) = take_word(remainder);
            (destination, name, remainder)
        }
        None => (SaveDestination::Disk(disk::Device::Boot), name, remainder),
    };
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: tape save [diskN|HOST[:diskN]] NAME");
        return;
    }
    let Some((address, length)) = tape_get() else {
        println!("No tape loaded; use 'tape load disk NAME' or 'tape load net' first.");
        return;
    };
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    match destination {
        SaveDestination::Disk(device) => match fosfs::write(info, device, name, bytes) {
            Ok(fosfs::WriteOutcome::Written) => println!(
                "Saved {length} byte(s) of the loaded tape to disk{} '{}'.",
                device_number(device),
                core::str::from_utf8(name).unwrap_or("?")
            ),
            Ok(fosfs::WriteOutcome::InvalidName) => {
                println!(
                    "Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'."
                );
            }
            Ok(fosfs::WriteOutcome::DirectoryFull) => {
                println!("Directory full: all 64 FOSFS entries are in use.")
            }
            Ok(fosfs::WriteOutcome::DataFull) => {
                println!("Data region full: no free sector remains in the FOSFS partition.")
            }
            Err(error) => println!("FOSFS disk error: {error:?}"),
        },
        SaveDestination::Remote(peer, remote_disk_number) => {
            let Some(nic) = nic_or_report() else {
                return;
            };
            let own_mac = nic.mac_address();
            match netfs_put(nic, peer, own_mac, remote_disk_number, name, bytes) {
                Ok(()) => println!(
                    "Saved {length} byte(s) of the loaded tape over NETFS to '{}'.",
                    core::str::from_utf8(name).unwrap_or("?")
                ),
                Err(error) => println!("NETFS save failed: {error}"),
            }
        }
    }
}

enum SaveDestination {
    Disk(disk::Device),
    Remote([u8; 6], u8),
}

/// Recognizes an optional leading destination word for `tape save`: a
/// `diskN` selector (an attached secondary disk), or a `HOST[:diskN]` peer
/// reference (NETFS). Returns `None` when the word is neither, so the
/// caller treats it as the file name instead (matching every other
/// `[diskN] NAME`-style command's own "no selector present" fallback).
fn parse_save_destination(word: &[u8]) -> Option<SaveDestination> {
    if let Some(device) = strip_disk_selector(word) {
        return Some(SaveDestination::Disk(device));
    }
    parse_remote_host_token(word)
        .map(|(peer, remote_disk_number)| SaveDestination::Remote(peer, remote_disk_number))
}

/// One tape transfer in progress during `tape load net`: which sender it's
/// locked onto (so unrelated chat/discover/tape traffic from anyone else
/// can't corrupt it mid-transfer), where its bytes are landing, how long
/// it's declared to be, and how much of it (by sequence number and byte
/// count) has arrived so far.
struct TapeTransfer {
    source: [u8; 6],
    address: u32,
    length: u32,
    expected_seq: u32,
    received: u32,
}

/// Waits for a tape to arrive over the network: a `TAPE_TAG_START` frame
/// declares the total length, then `TAPE_TAG_DATA` frames each carry an
/// explicit sequence number and up to `TAPE_CHUNK_LEN` bytes, until a
/// `TAPE_TAG_STOP` frame closes the transfer -- see the `TAPE_TAG_*`
/// constants. The offset and expected byte count for each chunk come from
/// its sequence number and the transfer's declared total length, never
/// from the received Ethernet frame's own (possibly hardware- or
/// software-padded) length, so a genuine binary tape's own trailing zero
/// bytes are never mistaken for padding -- unlike `chat`'s text frames,
/// which can get away with trimming trailing zeros only because printable
/// text never legitimately ends in one.
///
/// An out-of-order or skipped sequence number aborts that attempt
/// (freeing any frames already allocated for it) rather than silently
/// loading a corrupt tape, since this deliberately simple protocol has no
/// retransmission; waiting for a fresh start after an abort, or after any
/// unrelated sender's frame, costs nothing and needs no separate state.
#[inline(never)]
fn tape_load_net() {
    let Some(nic) = nic_or_report() else { return };
    let own_mac = nic.mac_address();
    println!("Waiting for a tape over the network; press F12 (or F4/F6/F7/F8/F10) to give up.");
    println!(
        "Have the sender target this card's MAC address ({}).",
        Mac(&own_mac)
    );

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut transfer: Option<TapeTransfer> = None;
    // Periodic (~2s) progress line while a transfer is in flight, so a
    // stalled load is visibly distinguishable from one still making
    // progress -- and, since `Rtl8139::overflow_count` only climbs when
    // the receive ring genuinely overflowed (see `Rtl8139::recover`
    // in rtl8139.rs), a nonzero and growing count here is a strong sign
    // the sender is outrunning this driver's polling loop, distinct from
    // a sender that has simply stopped, gone quiet, or was never reached
    // at all.
    let mut next_status = interrupts::ticks() + TAPE_STATUS_TICKS;

    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            println!("\nGave up waiting; no tape loaded.");
            return;
        }
        if remote_stop_via_session_nic(nic::active_index()) {
            if unsafe { PENDING_REMOTE_BYE } {
                println!("\nRemote terminal disconnected; no tape loaded.");
            } else {
                println!("\nGave up waiting (remote escape); no tape loaded.");
            }
            return;
        }
        if transfer.is_some() && interrupts::ticks() >= next_status {
            let active = transfer.as_ref().unwrap();
            println!(
                "\n... {} of {} byte(s) received so far ({} Rx overflow recover{}).",
                active.received,
                active.length,
                nic.overflow_count(),
                if nic.overflow_count() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            next_status = interrupts::ticks() + TAPE_STATUS_TICKS;
        }
        let Some(received) = nic.receive(&mut buffer) else {
            idle_wait();
            continue;
        };
        let frame = &buffer[..received];
        if remote_stop_frame(frame) {
            if unsafe { PENDING_REMOTE_BYE } {
                println!("\nRemote terminal disconnected; no tape loaded.");
            } else {
                println!("\nGave up waiting (remote escape); no tape loaded.");
            }
            return;
        }
        // While this nested loop owns NIC polling, do not let a command
        // from the suspended remote terminal editor vanish into the tape
        // protocol's catch-all arm. A remote hangup still ends the session
        // once `term_net` resumes; normal data is replayed by that editor.
        if defer_active_term_input(frame) {
            if unsafe { PENDING_REMOTE_BYE } {
                println!("\nRemote terminal disconnected; no tape loaded.");
                return;
            }
            continue;
        }
        if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        let payload = &frame[15..];

        match frame[14] {
            TAPE_TAG_START if transfer.is_none() => {
                let Some(length_bytes) = payload.get(0..4) else {
                    continue;
                };
                let length = u32::from_le_bytes(length_bytes.try_into().unwrap());
                if length == 0 {
                    println!("\nIgnoring a zero-length tape start from {}.", Mac(&source));
                    continue;
                }
                let Some(address) = allocate_tape_buffer(length) else {
                    continue;
                };
                println!("\nReceiving a {length}-byte tape from {}...", Mac(&source));
                // Restart the progress timer: it has usually long expired
                // while waiting for this START, and an immediate progress line
                // would block on console output right as data frames begin
                // arriving -- enough to overflow a small receive ring.
                next_status = interrupts::ticks() + TAPE_STATUS_TICKS;
                transfer = Some(TapeTransfer {
                    source,
                    address,
                    length,
                    expected_seq: 0,
                    received: 0,
                });
            }
            TAPE_TAG_DATA
                if transfer
                    .as_ref()
                    .is_some_and(|transfer| transfer.source == source) =>
            {
                let active = transfer.as_mut().unwrap();
                let Some(seq_bytes) = payload.get(0..4) else {
                    continue;
                };
                let seq = u32::from_le_bytes(seq_bytes.try_into().unwrap());
                if seq != active.expected_seq {
                    println!(
                        "\nGap in tape data from {}: expected chunk #{}, got #{seq} -- aborting this load.",
                        Mac(&source),
                        active.expected_seq
                    );
                    println!("(still listening for a new tape; resend it, or F12 to give up)");
                    transfer = None;
                    continue;
                }
                let offset = seq as usize * TAPE_CHUNK_LEN;
                let chunk_len = (active.length as usize - offset).min(TAPE_CHUNK_LEN);
                let data = &payload[4..];
                if data.len() < chunk_len {
                    println!(
                        "\nShort tape data chunk #{seq} from {}: expected {chunk_len} byte(s), frame only carried {} -- aborting this load.",
                        Mac(&source),
                        data.len()
                    );
                    println!("(still listening for a new tape; resend it, or F12 to give up)");
                    transfer = None;
                    continue;
                }
                let destination = unsafe {
                    core::slice::from_raw_parts_mut(
                        (active.address as usize + offset) as *mut u8,
                        chunk_len,
                    )
                };
                destination.copy_from_slice(&data[..chunk_len]);
                active.received += chunk_len as u32;
                active.expected_seq += 1;
            }
            TAPE_TAG_STOP
                if transfer
                    .as_ref()
                    .is_some_and(|transfer| transfer.source == source) =>
            {
                let active = transfer.take().unwrap();
                if active.received == active.length {
                    if !install_tape(active.address, active.length) {
                        return;
                    }
                    println!(
                        "\nTape loaded: {} byte(s) at 0x{:05x} from {}. 'tape run' to execute it.",
                        active.length,
                        active.address,
                        Mac(&source)
                    );
                    return;
                }
                println!(
                    "\nTape from {} stopped early: only {} of {} declared byte(s) arrived -- aborting.",
                    Mac(&source),
                    active.received,
                    active.length
                );
                println!("(still listening for a new tape; resend it, or F12 to give up)");
            }
            CHAT_TAG_DISCOVER_REQUEST => {
                // A `send_tape.py`/`nic discover`/`chat` peer probing for
                // us: reply the same way `handle_incoming_chat_frame` and
                // `nic_discover` do, regardless of whether a transfer is
                // in progress, so this host stays discoverable the whole
                // time it sits in `tape load net`, not only before/after
                // a transfer.
                remember_peer(source, trim_trailing_zero(payload));
                send_chat_control(
                    nic,
                    &source,
                    &own_mac,
                    CHAT_TAG_DISCOVER_REPLY,
                    nic::active_index(),
                );
                println!("\n(replied to a discovery probe from {})", Mac(&source));
            }
            _ => {} // A stray/unrelated tag, or traffic from a different sender mid-transfer: ignored.
        }
    }
}

fn tape_send_command(info: &BootInfo, arguments: &[u8]) {
    // An optional leading `diskN` selector, matching every other
    // FOSFS-reading command (`tape load`/`tape save`/`cat`/`rm`/`write`/
    // `copy`) -- this used to be the one command that always read from
    // disk1/Boot regardless, a real inconsistency found while building
    // AGENTS.md's cross-tape/command syntax reference.
    let (first_word, remainder) = take_word_or_slash(arguments);
    let (device, name, remainder) = match strip_disk_selector(first_word) {
        Some(device) => {
            let (name, remainder) = take_word(remainder);
            (device, name, remainder)
        }
        None => (disk::Device::Boot, first_word, remainder),
    };
    let (peer_word, extra) = take_word(remainder);
    if name.is_empty() || peer_word.is_empty() || !extra.is_empty() {
        println!("Usage: tape send [diskN] NAME PEER_MAC|PEER_NUMBER");
        return;
    }
    let destination = if let Some(mac) = parse_mac(peer_word) {
        mac
    } else if let Some(index) = parse_number(peer_word) {
        match peer_by_index(index as usize) {
            Some(mac) => mac,
            None => {
                println!(
                    "No peer #{index} known; run 'nic discover' first, or give a full MAC address."
                );
                return;
            }
        }
    } else {
        println!(
            "'{}' is neither a MAC address (aa:bb:cc:dd:ee:ff) nor a known peer",
            core::str::from_utf8(peer_word).unwrap_or("?")
        );
        println!("number; see 'nic peers'.");
        return;
    };

    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length == 0 {
        println!(
            "'{}' is empty; nothing to send.",
            core::str::from_utf8(name).unwrap_or("?")
        );
        return;
    }
    if length > u32::MAX as u64 {
        println!("File is too large for this 32-bit tape transfer.");
        return;
    }

    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let data = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, device, name, data) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    if !install_tape(address, length as u32) {
        return;
    }
    let Some(nic) = nic_or_report() else { return };
    let source = nic.mac_address();
    tape_send(nic, &destination, &source, data);
}

/// Sends one tape (`TAPE_TAG_START`, then `TAPE_TAG_DATA` chunks, then
/// `TAPE_TAG_STOP`, see the `TAPE_TAG_*` constants) unicast to
/// `destination`, mirroring `tools/send_tape.py`'s host-side sender so
/// either end can talk to the other.
fn tape_send(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], data: &[u8]) {
    let mut start_frame = [0u8; 19];
    start_frame[0..6].copy_from_slice(destination);
    start_frame[6..12].copy_from_slice(source);
    start_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    start_frame[14] = TAPE_TAG_START;
    start_frame[15..19].copy_from_slice(&(data.len() as u32).to_le_bytes());
    if !nic.send(&start_frame) {
        println!("Transmit failed (card reported an error, or timed out);");
        println!("tape not sent.");
        return;
    }
    pace_tape_send();

    let mut data_frame = [0u8; 15 + 4 + TAPE_CHUNK_LEN];
    data_frame[0..6].copy_from_slice(destination);
    data_frame[6..12].copy_from_slice(source);
    data_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    data_frame[14] = TAPE_TAG_DATA;
    for (seq, chunk) in data.chunks(TAPE_CHUNK_LEN).enumerate() {
        data_frame[15..19].copy_from_slice(&(seq as u32).to_le_bytes());
        data_frame[19..19 + chunk.len()].copy_from_slice(chunk);
        if !nic.send(&data_frame[..19 + chunk.len()]) {
            println!(
                "Transmit failed on chunk #{seq} (card reported an error, or"
            );
            println!("timed out); tape not fully sent.");
            return;
        }
        pace_tape_send();
    }

    let mut stop_frame = [0u8; 15];
    stop_frame[0..6].copy_from_slice(destination);
    stop_frame[6..12].copy_from_slice(source);
    stop_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    stop_frame[14] = TAPE_TAG_STOP;
    if !nic.send(&stop_frame) {
        println!("Transmit failed sending the closing frame; the receiver may");
        println!("report an incomplete tape.");
        return;
    }

    println!("Sent {} byte(s) to {}.", data.len(), Mac(destination));
}

fn pace_tape_send() {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < TAPE_SEND_PACE_TICKS {
        core::hint::spin_loop();
    }
}

/// One incoming NETFS PUT in progress, mirroring `TapeTransfer`'s "locked
/// onto exactly one sender, sequence-numbered, no retransmission" shape,
/// but living across many separate `ambient_discovery_poll` calls (one
/// frame at a time) rather than one dedicated blocking loop, since serving
/// is deliberately folded into the idle prompt loop instead of a loop of
/// its own (see "NETFS" above `NETFS_TAG_GET_REQUEST`). Lives in the
/// shared `pmem::TAPE_BASE` staging buffer -- the same one `copy`/`tape
/// load disk`/`tape load net` already reuse for "receive an unknown-sized
/// file, then act on it" -- so a locally typed `copy`/`tape load` command
/// that also touches that buffer while a remote PUT is mid-flight (which,
/// spread across many idle-loop iterations, can span real wall-clock
/// seconds) would corrupt both. This is a known, accepted limitation of
/// fos's single-threaded, no-locking design, not something this feature
/// attempts to guard against.
struct NetfsPutTransfer {
    source: [u8; 6],
    device: disk::Device,
    remote_disk_number: u8,
    name: [u8; NETFS_NAME_CAPACITY],
    name_len: u8,
    length: u32,
    checksum: u16,
    expected_seq: u32,
    received: u32,
}

static mut NETFS_INCOMING_PUT: Option<NetfsPutTransfer> = None;

fn send_netfs_get_request(
    nic: &nic::Nic,
    peer: &[u8; 6],
    own_mac: &[u8; 6],
    remote_disk_number: u8,
    name: &[u8],
) -> bool {
    let mut frame = [0u8; 16 + NETFS_NAME_CAPACITY];
    let total = 16 + name.len();
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = NETFS_TAG_GET_REQUEST;
    frame[15] = remote_disk_number;
    frame[16..16 + name.len()].copy_from_slice(name);
    nic.send(&frame[..total])
}

fn send_netfs_put_request(
    nic: &nic::Nic,
    peer: &[u8; 6],
    own_mac: &[u8; 6],
    remote_disk_number: u8,
    length: u32,
    checksum: u16,
    name: &[u8],
) -> bool {
    let mut frame = [0u8; 22 + NETFS_NAME_CAPACITY];
    let total = 22 + name.len();
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = NETFS_TAG_PUT_REQUEST;
    frame[15] = remote_disk_number;
    frame[16..20].copy_from_slice(&length.to_le_bytes());
    frame[20..22].copy_from_slice(&checksum.to_le_bytes());
    frame[22..22 + name.len()].copy_from_slice(name);
    nic.send(&frame[..total])
}

fn send_netfs_get_start(
    nic: &nic::Nic,
    peer: &[u8; 6],
    own_mac: &[u8; 6],
    length: u32,
    checksum: u16,
) -> bool {
    let mut frame = [0u8; 21];
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = NETFS_TAG_GET_START;
    frame[15..19].copy_from_slice(&length.to_le_bytes());
    frame[19..21].copy_from_slice(&checksum.to_le_bytes());
    nic.send(&frame)
}

/// Sends `data` as a sequence-numbered burst of `tag`-tagged chunks (up to
/// `TAPE_CHUNK_LEN` bytes each), the exact same shape and pacing
/// `tape_send`'s own `TAPE_TAG_DATA` loop uses -- reused here rather than
/// duplicated, since NETFS's GET/PUT data frames are byte-for-byte the
/// same shape, just under different tags.
fn send_netfs_data_frames(
    nic: &nic::Nic,
    peer: &[u8; 6],
    own_mac: &[u8; 6],
    tag: u8,
    data: &[u8],
) -> bool {
    let mut frame = [0u8; 15 + 4 + TAPE_CHUNK_LEN];
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    for (seq, chunk) in data.chunks(TAPE_CHUNK_LEN).enumerate() {
        frame[15..19].copy_from_slice(&(seq as u32).to_le_bytes());
        frame[19..19 + chunk.len()].copy_from_slice(chunk);
        if !nic.send(&frame[..19 + chunk.len()]) {
            return false;
        }
        pace_tape_send();
    }
    true
}

fn send_netfs_control(nic: &nic::Nic, peer: &[u8; 6], own_mac: &[u8; 6], tag: u8) -> bool {
    let mut frame = [0u8; 15];
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    nic.send(&frame)
}

fn send_netfs_error(nic: &nic::Nic, peer: &[u8; 6], own_mac: &[u8; 6], tag: u8, code: u8) {
    let mut frame = [0u8; 16];
    frame[0..6].copy_from_slice(peer);
    frame[6..12].copy_from_slice(own_mac);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    frame[15] = code;
    nic.send(&frame);
}

/// Fetches a file from `peer`'s disk `remote_disk_number` into
/// `destination`, returning the number of bytes actually received. This is
/// the shared client-side half of NETFS's GET, used identically by `copy`,
/// `tape load HOST[:diskN] NAME`, and `tape run`'s `@HOST[:diskN]/NAME`
/// argument source -- each just provides a different destination buffer
/// (the shared `pmem::TAPE_BASE` scratch region for the first two, a small
/// caller-owned stack buffer for the third, so a config-file fetch can
/// never disturb an already-loaded tape sitting in that shared region).
/// Gives up, returning `NetfsError::NoReply`, after `NETFS_TIMEOUT_TICKS`
/// with no further progress, or on a local escape keypress -- the same
/// "give up" convention `tape_load_net` already uses.
fn netfs_get(
    nic: &nic::Nic,
    peer: [u8; 6],
    own_mac: [u8; 6],
    remote_disk_number: u8,
    name: &[u8],
    destination: &mut [u8],
) -> Result<usize, NetfsError> {
    if !send_netfs_get_request(nic, &peer, &own_mac, remote_disk_number, name) {
        return Err(NetfsError::TransmitFailed);
    }
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut deadline = interrupts::ticks() + NETFS_TIMEOUT_TICKS;
    let mut declared_length: Option<u32> = None;
    let mut declared_checksum = 0u16;
    let mut expected_seq = 0u32;
    let mut received = 0u32;
    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            return Err(NetfsError::NoReply);
        }
        if interrupts::ticks() >= deadline {
            return Err(NetfsError::NoReply);
        }
        let Some(length) = nic.receive(&mut buffer) else {
            idle_wait();
            continue;
        };
        let frame = &buffer[..length];
        if defer_active_term_input(frame) {
            if unsafe { PENDING_REMOTE_BYE } {
                return Err(NetfsError::NoReply);
            }
            continue;
        }
        if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        let payload = &frame[15..];
        if source != peer {
            if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
                remember_peer(source, trim_trailing_zero(payload));
                send_chat_control(
                    nic,
                    &source,
                    &own_mac,
                    CHAT_TAG_DISCOVER_REPLY,
                    nic::active_index(),
                );
            }
            continue;
        }
        match frame[14] {
            NETFS_TAG_GET_START if declared_length.is_none() => {
                let Some(length_bytes) = payload.get(0..4) else {
                    continue;
                };
                let Some(checksum_bytes) = payload.get(4..6) else {
                    continue;
                };
                let total = u32::from_le_bytes(length_bytes.try_into().unwrap());
                if total as usize > destination.len() {
                    return Err(NetfsError::TooLargeLocally);
                }
                declared_length = Some(total);
                declared_checksum = u16::from_le_bytes(checksum_bytes.try_into().unwrap());
                deadline = interrupts::ticks() + NETFS_TIMEOUT_TICKS;
            }
            NETFS_TAG_GET_DATA if declared_length.is_some() => {
                let total = declared_length.unwrap();
                let Some(seq_bytes) = payload.get(0..4) else {
                    continue;
                };
                let seq = u32::from_le_bytes(seq_bytes.try_into().unwrap());
                if seq != expected_seq {
                    return Err(NetfsError::Gap);
                }
                let offset = seq as usize * TAPE_CHUNK_LEN;
                let chunk_len = (total as usize - offset).min(TAPE_CHUNK_LEN);
                let data = &payload[4..];
                if data.len() < chunk_len {
                    return Err(NetfsError::Gap);
                }
                destination[offset..offset + chunk_len].copy_from_slice(&data[..chunk_len]);
                received += chunk_len as u32;
                expected_seq += 1;
                deadline = interrupts::ticks() + NETFS_TIMEOUT_TICKS;
            }
            NETFS_TAG_GET_STOP if declared_length.is_some() => {
                let total = declared_length.unwrap();
                if received != total {
                    return Err(NetfsError::Gap);
                }
                let total = total as usize;
                if internet_checksum(&destination[..total]) != declared_checksum {
                    return Err(NetfsError::ChecksumMismatch);
                }
                return Ok(total);
            }
            NETFS_TAG_GET_ERROR => {
                return Err(NetfsError::Remote(payload.first().copied().unwrap_or(0)));
            }
            _ => {}
        }
    }
}

/// Stores `data` under `name` on `peer`'s disk `remote_disk_number`, the
/// shared client-side half of NETFS's PUT, used identically by `copy` and
/// `tape save HOST[:diskN] NAME`. Unlike `netfs_get`, the length and
/// checksum are already known up front (the caller already has the whole
/// file in memory), so they travel in the initial request frame itself
/// rather than a separate `_START`; the transfer still ends with an
/// explicit `_ACK`/`_ERROR` so the caller learns whether the remote FOSFS
/// write actually succeeded, not just whether every byte arrived.
fn netfs_put(
    nic: &nic::Nic,
    peer: [u8; 6],
    own_mac: [u8; 6],
    remote_disk_number: u8,
    name: &[u8],
    data: &[u8],
) -> Result<(), NetfsError> {
    if data.len() > u32::MAX as usize {
        return Err(NetfsError::TooLargeLocally);
    }
    let checksum = internet_checksum(data);
    if !send_netfs_put_request(
        nic,
        &peer,
        &own_mac,
        remote_disk_number,
        data.len() as u32,
        checksum,
        name,
    ) {
        return Err(NetfsError::TransmitFailed);
    }
    pace_tape_send();
    if !send_netfs_data_frames(nic, &peer, &own_mac, NETFS_TAG_PUT_DATA, data) {
        return Err(NetfsError::TransmitFailed);
    }
    if !send_netfs_control(nic, &peer, &own_mac, NETFS_TAG_PUT_STOP) {
        return Err(NetfsError::TransmitFailed);
    }
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let deadline = interrupts::ticks() + NETFS_TIMEOUT_TICKS;
    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            return Err(NetfsError::NoReply);
        }
        if interrupts::ticks() >= deadline {
            return Err(NetfsError::NoReply);
        }
        let Some(length) = nic.receive(&mut buffer) else {
            idle_wait();
            continue;
        };
        let frame = &buffer[..length];
        if defer_active_term_input(frame) {
            if unsafe { PENDING_REMOTE_BYE } {
                return Err(NetfsError::NoReply);
            }
            continue;
        }
        if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        if source != peer {
            if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
                remember_peer(source, trim_trailing_zero(&frame[15..]));
                send_chat_control(
                    nic,
                    &source,
                    &own_mac,
                    CHAT_TAG_DISCOVER_REPLY,
                    nic::active_index(),
                );
            }
            continue;
        }
        match frame[14] {
            NETFS_TAG_PUT_ACK => return Ok(()),
            NETFS_TAG_PUT_ERROR => {
                return Err(NetfsError::Remote(frame.get(15).copied().unwrap_or(0)));
            }
            _ => {}
        }
    }
}

/// Answers one `NETFS_TAG_GET_REQUEST`, called from `ambient_discovery_
/// poll`. This is synchronous and blocking, exactly like `tape_send`'s
/// existing paced transmit loop: it reads the whole file, computes its
/// checksum, and sends the entire `_START`+`_DATA*`+`_STOP` burst before
/// returning, pausing the idle prompt loop for the duration -- an accepted
/// trade-off consistent with this kernel having no real concurrency at all.
fn serve_netfs_get_request(
    info: &BootInfo,
    nic: &nic::Nic,
    source: [u8; 6],
    own_mac: [u8; 6],
    remote_disk_number: u8,
    name: &[u8],
) {
    let name = trim_trailing_zero(name);
    if name.is_empty() {
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_GET_ERROR,
            NETFS_ERROR_INVALID_NAME,
        );
        return;
    }
    let Some(device) = device_from_number(remote_disk_number as u32) else {
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_GET_ERROR,
            NETFS_ERROR_DISK_UNAVAILABLE,
        );
        return;
    };
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            send_netfs_error(nic, &source, &own_mac, NETFS_TAG_GET_ERROR, NETFS_ERROR_NOT_FOUND);
            return;
        }
        Err(_) => {
            send_netfs_error(nic, &source, &own_mac, NETFS_TAG_GET_ERROR, NETFS_ERROR_DISK_ERROR);
            return;
        }
    };
    if length == 0 || length > pmem::TAPE_LEN as u64 {
        send_netfs_error(nic, &source, &own_mac, NETFS_TAG_GET_ERROR, NETFS_ERROR_TOO_LARGE);
        return;
    }
    let length = length as usize;
    let buffer = unsafe { core::slice::from_raw_parts_mut(pmem::TAPE_BASE as *mut u8, length) };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            send_netfs_error(nic, &source, &own_mac, NETFS_TAG_GET_ERROR, NETFS_ERROR_NOT_FOUND);
            return;
        }
        Err(_) => {
            send_netfs_error(nic, &source, &own_mac, NETFS_TAG_GET_ERROR, NETFS_ERROR_DISK_ERROR);
            return;
        }
    }
    let checksum = internet_checksum(buffer);
    println!(
        "\nServing a {length}-byte network GET of disk{remote_disk_number} '{}' to {}...",
        core::str::from_utf8(name).unwrap_or("?"),
        Mac(&source)
    );
    if send_netfs_get_start(nic, &source, &own_mac, length as u32, checksum) {
        pace_tape_send();
        if send_netfs_data_frames(nic, &source, &own_mac, NETFS_TAG_GET_DATA, buffer) {
            send_netfs_control(nic, &source, &own_mac, NETFS_TAG_GET_STOP);
        }
    }
    invalidate_tape_after_scratch_use();
}

/// Begins serving one `NETFS_TAG_PUT_REQUEST`, called from
/// `ambient_discovery_poll`. Unlike GET (answered synchronously in one
/// call), a PUT's data necessarily spans many later `ambient_discovery_
/// poll` calls, so this only validates the request and records
/// `NetfsPutTransfer` state; `serve_netfs_put_data`/`serve_netfs_put_stop`
/// (each called from later frames) do the rest.
fn serve_netfs_put_request(nic: &nic::Nic, source: [u8; 6], own_mac: [u8; 6], payload: &[u8]) {
    if unsafe { (*(&raw const NETFS_INCOMING_PUT)).is_some() } {
        send_netfs_error(nic, &source, &own_mac, NETFS_TAG_PUT_ERROR, NETFS_ERROR_BUSY);
        return;
    }
    let Some(&remote_disk_number) = payload.first() else {
        return;
    };
    let Some(length_bytes) = payload.get(1..5) else {
        return;
    };
    let Some(checksum_bytes) = payload.get(5..7) else {
        return;
    };
    let name = trim_trailing_zero(payload.get(7..).unwrap_or(&[]));
    let length = u32::from_le_bytes(length_bytes.try_into().unwrap());
    let checksum = u16::from_le_bytes(checksum_bytes.try_into().unwrap());
    if name.is_empty() || name.len() > NETFS_NAME_CAPACITY {
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_INVALID_NAME,
        );
        return;
    }
    let Some(device) = device_from_number(remote_disk_number as u32) else {
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_DISK_UNAVAILABLE,
        );
        return;
    };
    if length == 0 || length > pmem::TAPE_LEN {
        send_netfs_error(nic, &source, &own_mac, NETFS_TAG_PUT_ERROR, NETFS_ERROR_TOO_LARGE);
        return;
    }
    println!(
        "\nAccepting a {length}-byte network PUT of '{}' onto disk{remote_disk_number} from {}...",
        core::str::from_utf8(name).unwrap_or("?"),
        Mac(&source)
    );
    let mut transfer = NetfsPutTransfer {
        source,
        device,
        remote_disk_number,
        name: [0; NETFS_NAME_CAPACITY],
        name_len: name.len() as u8,
        length,
        checksum,
        expected_seq: 0,
        received: 0,
    };
    transfer.name[..name.len()].copy_from_slice(name);
    unsafe {
        *(&raw mut NETFS_INCOMING_PUT) = Some(transfer);
    }
}

fn serve_netfs_put_data(source: [u8; 6], payload: &[u8]) {
    let transfer_ptr = &raw mut NETFS_INCOMING_PUT;
    let Some(transfer) = (unsafe { &mut *transfer_ptr }).as_mut() else {
        return;
    };
    if transfer.source != source {
        return;
    }
    let Some(seq_bytes) = payload.get(0..4) else {
        return;
    };
    let seq = u32::from_le_bytes(seq_bytes.try_into().unwrap());
    if seq != transfer.expected_seq {
        println!(
            "\nGap in incoming network PUT data from {}; aborting.",
            Mac(&source)
        );
        unsafe {
            *transfer_ptr = None;
        }
        return;
    }
    let offset = seq as usize * TAPE_CHUNK_LEN;
    let chunk_len = (transfer.length as usize - offset).min(TAPE_CHUNK_LEN);
    let data = &payload[4..];
    if data.len() < chunk_len {
        println!(
            "\nShort incoming network PUT data from {}; aborting.",
            Mac(&source)
        );
        unsafe {
            *transfer_ptr = None;
        }
        return;
    }
    let destination = unsafe {
        core::slice::from_raw_parts_mut((pmem::TAPE_BASE as usize + offset) as *mut u8, chunk_len)
    };
    destination.copy_from_slice(&data[..chunk_len]);
    transfer.received += chunk_len as u32;
    transfer.expected_seq += 1;
}

fn serve_netfs_put_stop(info: &BootInfo, nic: &nic::Nic, source: [u8; 6], own_mac: [u8; 6]) {
    let transfer_ptr = &raw mut NETFS_INCOMING_PUT;
    let matches = unsafe { &*transfer_ptr }
        .as_ref()
        .is_some_and(|transfer| transfer.source == source);
    if !matches {
        return;
    }
    let transfer = unsafe { (&mut *transfer_ptr).take().unwrap() };
    if transfer.received != transfer.length {
        println!(
            "\nIncoming network PUT from {} stopped early ({} of {} byte(s)); aborting.",
            Mac(&source),
            transfer.received,
            transfer.length
        );
        send_netfs_error(nic, &source, &own_mac, NETFS_TAG_PUT_ERROR, NETFS_ERROR_DISK_ERROR);
        return;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(pmem::TAPE_BASE as *const u8, transfer.length as usize)
    };
    if internet_checksum(bytes) != transfer.checksum {
        println!(
            "\nChecksum mismatch on incoming network PUT from {}; discarding.",
            Mac(&source)
        );
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_CHECKSUM_MISMATCH,
        );
        return;
    }
    let name = &transfer.name[..transfer.name_len as usize];
    let outcome = fosfs::write(info, transfer.device, name, bytes);
    invalidate_tape_after_scratch_use();
    match outcome {
        Ok(fosfs::WriteOutcome::Written) => {
            println!(
                "\nWrote {} byte(s) from {} to disk{} '{}'.",
                transfer.length,
                Mac(&source),
                transfer.remote_disk_number,
                core::str::from_utf8(name).unwrap_or("?")
            );
            send_netfs_control(nic, &source, &own_mac, NETFS_TAG_PUT_ACK);
        }
        Ok(fosfs::WriteOutcome::InvalidName) => send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_INVALID_NAME,
        ),
        Ok(fosfs::WriteOutcome::DirectoryFull) => send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_DIRECTORY_FULL,
        ),
        Ok(fosfs::WriteOutcome::DataFull) => send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_DATA_FULL,
        ),
        Err(_) => send_netfs_error(
            nic,
            &source,
            &own_mac,
            NETFS_TAG_PUT_ERROR,
            NETFS_ERROR_DISK_ERROR,
        ),
    }
}

/// One incoming KUP (network kernel update) PUT in progress -- structurally
/// identical to [`NetfsPutTransfer`] above (see its own doc comment for the
/// shared "sequence-numbered, no retransmission, spread across many idle-
/// loop calls" design), but tracking a raw kernel image destined for an A/B
/// slot instead of a named FOSFS file, so there is no `device`/`name` field
/// here at all. Shares the same accepted shared-buffer risk
/// `NetfsPutTransfer` documents (both stage incoming bytes in
/// `pmem::TAPE_BASE`) -- more consequential here, since it could corrupt an
/// in-progress kernel image, but still bounded by the same safety net:
/// `select_kernel_slot` (`boot/stage2.S`) only ever trusts a pending slot
/// once it is both `attempted` and `confirmed`, so even a corrupted image
/// installed this way cannot brick the machine outright -- it simply fails
/// to reach `debug::confirm_boot`'s milestone and is rolled back on the
/// following boot exactly like any other bad kernel push.
struct KernelUpdateTransfer {
    source: [u8; 6],
    length: u32,
    checksum: u16,
    expected_seq: u32,
    received: u32,
}

static mut KERNEL_UPDATE_INCOMING: Option<KernelUpdateTransfer> = None;

/// Begins serving one `KUP_TAG_PUT_REQUEST`, called from
/// `ambient_discovery_poll` only when `debug::kernel_update_enabled()`.
/// Payload: length (u32 LE) then checksum (u16 LE) -- no name, since there
/// is only ever one kernel image per update, unlike NETFS's arbitrarily
/// named files. `serve_kup_put_data`/`serve_kup_put_stop` (each called from
/// later frames) do the rest, mirroring `serve_netfs_put_request`'s own
/// split for the same reason: a PUT's data necessarily spans many later
/// `ambient_discovery_poll` calls.
fn serve_kup_put_request(nic: &nic::Nic, source: [u8; 6], own_mac: [u8; 6], payload: &[u8]) {
    if unsafe { (*(&raw const KERNEL_UPDATE_INCOMING)).is_some() } {
        send_netfs_error(nic, &source, &own_mac, KUP_TAG_PUT_ERROR, KUP_ERROR_BUSY);
        return;
    }
    let Some(length_bytes) = payload.get(0..4) else {
        return;
    };
    let Some(checksum_bytes) = payload.get(4..6) else {
        return;
    };
    let length = u32::from_le_bytes(length_bytes.try_into().unwrap());
    let checksum = u16::from_le_bytes(checksum_bytes.try_into().unwrap());
    let max_len = (crate::debug::SLOT_SECTORS * 512) as u32;
    if length == 0 || length > max_len || length > pmem::TAPE_LEN {
        send_netfs_error(nic, &source, &own_mac, KUP_TAG_PUT_ERROR, KUP_ERROR_TOO_LARGE);
        return;
    }
    println!(
        "\nAccepting a {length}-byte network kernel update from {}...",
        Mac(&source)
    );
    unsafe {
        *(&raw mut KERNEL_UPDATE_INCOMING) = Some(KernelUpdateTransfer {
            source,
            length,
            checksum,
            expected_seq: 0,
            received: 0,
        });
    }
}

fn serve_kup_put_data(source: [u8; 6], payload: &[u8]) {
    let transfer_ptr = &raw mut KERNEL_UPDATE_INCOMING;
    let Some(transfer) = (unsafe { &mut *transfer_ptr }).as_mut() else {
        return;
    };
    if transfer.source != source {
        return;
    }
    let Some(seq_bytes) = payload.get(0..4) else {
        return;
    };
    let seq = u32::from_le_bytes(seq_bytes.try_into().unwrap());
    if seq != transfer.expected_seq {
        println!(
            "\nGap in incoming network kernel update data from {}; aborting.",
            Mac(&source)
        );
        unsafe {
            *transfer_ptr = None;
        }
        return;
    }
    let offset = seq as usize * TAPE_CHUNK_LEN;
    let chunk_len = (transfer.length as usize - offset).min(TAPE_CHUNK_LEN);
    let data = &payload[4..];
    if data.len() < chunk_len {
        println!(
            "\nShort incoming network kernel update data from {}; aborting.",
            Mac(&source)
        );
        unsafe {
            *transfer_ptr = None;
        }
        return;
    }
    let destination = unsafe {
        core::slice::from_raw_parts_mut((pmem::TAPE_BASE as usize + offset) as *mut u8, chunk_len)
    };
    destination.copy_from_slice(&data[..chunk_len]);
    transfer.received += chunk_len as u32;
    transfer.expected_seq += 1;
}

/// Finishes one KUP transfer: verifies the checksum, writes the image into
/// whichever slot is not `active_slot` ([`debug::inactive_slot`]), records
/// it as a fresh `pending_slot` ([`debug::install_kernel_update`]), and --
/// on success only -- acknowledges the peer and resets this machine a short
/// delay later so `select_kernel_slot`'s existing fallback state machine
/// gives the new kernel its first, bounded try. A failure at any step
/// leaves the currently running kernel and its `active_slot` completely
/// untouched; this machine keeps running unmodified and the operator can
/// simply try again.
fn serve_kup_put_stop(info: &BootInfo, nic: &nic::Nic, source: [u8; 6], own_mac: [u8; 6]) {
    let transfer_ptr = &raw mut KERNEL_UPDATE_INCOMING;
    let matches = unsafe { &*transfer_ptr }
        .as_ref()
        .is_some_and(|transfer| transfer.source == source);
    if !matches {
        return;
    }
    let transfer = unsafe { (&mut *transfer_ptr).take().unwrap() };
    if transfer.received != transfer.length {
        println!(
            "\nIncoming network kernel update from {} stopped early ({} of {} byte(s)); aborting.",
            Mac(&source),
            transfer.received,
            transfer.length
        );
        send_netfs_error(nic, &source, &own_mac, KUP_TAG_PUT_ERROR, KUP_ERROR_DISK_ERROR);
        return;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(pmem::TAPE_BASE as *const u8, transfer.length as usize)
    };
    if internet_checksum(bytes) != transfer.checksum {
        println!(
            "\nChecksum mismatch on incoming network kernel update from {}; discarding.",
            Mac(&source)
        );
        send_netfs_error(
            nic,
            &source,
            &own_mac,
            KUP_TAG_PUT_ERROR,
            KUP_ERROR_CHECKSUM_MISMATCH,
        );
        return;
    }
    let Some(metadata) = crate::debug::SlotMetadata::read(info) else {
        println!("\nCould not read A/B slot metadata; discarding network kernel update.");
        send_netfs_error(nic, &source, &own_mac, KUP_TAG_PUT_ERROR, KUP_ERROR_DISK_ERROR);
        return;
    };
    let slot = crate::debug::inactive_slot(&metadata);
    if let Err(error) = crate::debug::install_kernel_update(info, slot, bytes) {
        println!("\nFailed to write network kernel update to slot {slot}: {error:?}");
        send_netfs_error(nic, &source, &own_mac, KUP_TAG_PUT_ERROR, KUP_ERROR_DISK_ERROR);
        return;
    }
    invalidate_tape_after_scratch_use();
    println!(
        "\nInstalled a {}-byte kernel from {} into slot {slot}; rebooting to try it ...",
        transfer.length,
        Mac(&source)
    );
    send_netfs_control(nic, &source, &own_mac, KUP_TAG_PUT_ACK);
    let deadline = interrupts::ticks().wrapping_add(100);
    while interrupts::ticks() < deadline {
        core::hint::spin_loop();
    }
    // A soft reboot (see `crate::reboot`) picks up the freshly installed
    // pending slot exactly like a hardware reset would, just without
    // waiting through a full BIOS POST first -- KUP's whole point is fast
    // network-driven iteration, so skipping POST here matters in exactly
    // the way it doesn't for `reboot-on-crash` (see that module's own
    // doc comment for why *that* path stays hardware-only).
    crate::debug::note_clean_shutdown(info);
    crate::reboot::soft_reboot(info);
}

/// Displays a MAC address as `aa:bb:cc:dd:ee:ff` wherever `println!`/`print!`
/// need one, without repeating the six-field hex format at every call site.
struct Mac<'a>(&'a [u8; 6]);

impl core::fmt::Display for Mac<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Parses `aa:bb:cc:dd:ee:ff`-style text into a MAC address, mirroring
/// `tools/send_frame.py`'s `parse_mac` for consistency across the
/// host/kernel halves of the chat/sniff tooling.
fn parse_mac(text: &[u8]) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut fields = text.split(|&byte| byte == b':');
    for slot in mac.iter_mut() {
        let field = fields.next()?;
        if field.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(core::str::from_utf8(field).ok()?, 16).ok()?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(mac)
}

fn no_arguments(arguments: &[u8]) -> bool {
    if arguments.is_empty() {
        true
    } else {
        println!("This command does not accept arguments.");
        false
    }
}

/// Maps a 1-based disk number (`1` = boot disk, `2` = the first secondary
/// disk, ...) to a [`disk::Device`], but only when it names a disk that is
/// actually attached right now -- the numeric half of `strip_disk_selector`
/// below, factored out for callers (like NETFS's wire protocol, which
/// sends a disk number as a single byte rather than parsing `diskN` text)
/// that already have the number itself.
fn device_from_number(number: u32) -> Option<disk::Device> {
    match number {
        0 => None,
        1 => Some(disk::Device::Boot),
        n => {
            let index = (n - 2) as usize;
            (index < disk::secondary_count()).then_some(disk::Device::Secondary(index))
        }
    }
}

/// Parses a strict `diskN` token (`disk` followed by one or more decimal
/// digits) into a [`disk::Device`], but only when `N` names a disk that is
/// actually attached right now: `disk1` is always the boot disk; `disk2`,
/// `disk3`, ... map to [`disk::discover_secondary_disks`]'s results in
/// discovery order. Never guesses -- an out-of-range or malformed token
/// simply isn't recognized as a selector at all, so a FOSFS file literally
/// named e.g. `disk2` stays reachable unless a second disk really is
/// attached (see AGENTS.md's "A second (non-boot) disk" section).
fn strip_disk_selector(word: &[u8]) -> Option<disk::Device> {
    let suffix = word.strip_prefix(b"disk")?;
    if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number: u32 = core::str::from_utf8(suffix).ok()?.parse().ok()?;
    device_from_number(number)
}

/// Parses an optional `diskN` selector for a command that itself takes no
/// further arguments (`ls`, `format`, `diskinfo`): a lone leading `diskN`
/// word is unambiguous here, since none of these commands mean anything
/// else by their argument. Accepts either `diskN` alone or, via
/// `take_word_or_slash`, a `diskN/` prefix (unused here since there is
/// nothing to attach after it, but kept for symmetry with the other two
/// helpers below).
fn parse_disk_only(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word_or_slash(arguments);
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// Parses an optional `diskN` selector for a command that takes exactly one
/// further argument (`cat NAME`, `rm NAME`, `readsector LBA`). A `diskN`-
/// shaped first word is only treated as a selector when something follows
/// it; with nothing left over, it must be the command's own single
/// argument instead -- e.g. `cat disk2` still reads a file literally named
/// `disk2` when there is nothing else on the line to read. The selector and
/// its argument may be written `diskN NAME` or `diskN/NAME` (see
/// `take_word_or_slash`); `cat disk2` (no name at all) is unaffected by
/// this, since a bare `diskN` with nothing after it -- space or slash --
/// still falls through to the "no selector" case above.
fn parse_disk_and_one_argument(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word_or_slash(arguments);
    if remainder.is_empty() {
        return (disk::Device::Boot, arguments);
    }
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// Parses an optional `diskN` selector for `write NAME TEXT`, which takes
/// two further arguments. The same disambiguation rule applies one level
/// deeper: `diskN` is only a selector when a non-empty NAME *and* non-empty
/// TEXT both remain after it, so `write disk2 hello` still means "write a
/// file named disk2 containing 'hello'" rather than an invalid selector
/// with a missing TEXT. As with the two helpers above, `diskN/NAME TEXT` is
/// accepted alongside `diskN NAME TEXT`.
fn parse_disk_and_two_arguments(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word_or_slash(arguments);
    let (name, text) = take_word(remainder);
    if name.is_empty() || text.is_empty() {
        return (disk::Device::Boot, arguments);
    }
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// The human-facing `diskN` number for a [`disk::Device`], for status
/// messages (`disk1` is always the boot disk).
fn device_number(device: disk::Device) -> u32 {
    match device {
        disk::Device::Boot => 1,
        disk::Device::Secondary(index) => index as u32 + 2,
    }
}

fn take_word(input: &[u8]) -> (&[u8], &[u8]) {
    let input = trim_start(input);
    let end = input
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(input.len());
    (&input[..end], trim_start(&input[end..]))
}

/// Like [`take_word`], but also accepts a `/` as an alternate word
/// separator: `diskN/NAME` splits the same way `diskN NAME` does. FOSFS
/// names and hostnames never contain `/` (see `fosfs::valid_name`), so this
/// is unambiguous, and it lets every `[diskN] NAME`-style command accept the
/// same compact single-token spelling `@diskN/NAME` and `HOST[:diskN]/NAME`
/// already use elsewhere, instead of requiring the space form only. A `/`
/// found before any space wins, since that is the only way a disk selector
/// can be joined to what follows it.
fn take_word_or_slash(input: &[u8]) -> (&[u8], &[u8]) {
    let input = trim_start(input);
    let end = input
        .iter()
        .position(|byte| *byte == b' ' || *byte == b'/')
        .unwrap_or(input.len());
    if input.get(end) == Some(&b'/') {
        (&input[..end], &input[end + 1..])
    } else {
        (&input[..end], trim_start(&input[end..]))
    }
}

fn trim_start(mut input: &[u8]) -> &[u8] {
    while input.first() == Some(&b' ') {
        input = &input[1..];
    }
    input
}

/// Trims trailing ASCII whitespace (space, tab, `\r`, `\n`) from `bytes`,
/// used to clean up a hand-edited config file's own trailing newline (see
/// `load_hostname`) -- distinct from `trim_trailing_zero` below, which
/// trims the zero *padding* Ethernet's 60-byte minimum frame length adds to
/// a short frame, not genuine whitespace a human typed.
fn trim_trailing_whitespace(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

/// Trims trailing zero bytes from a received frame's payload -- the padding
/// `Rtl8139::send`/`tools/send_frame.py` add to reach Ethernet's 60-byte
/// minimum frame length, safe to strip here because genuine text/hostname
/// payloads never legitimately end in a real zero byte.
fn trim_trailing_zero(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |index| index + 1);
    &bytes[..end]
}

/// Writes `value` as ASCII decimal digits into `out` (which must be large
/// enough for the full representation), returning the number of bytes
/// written. Used to append a 1-based interface number to a configured
/// hostname (see `interface_hostname`) -- this kernel has no `core::fmt`
/// integration for writing into a plain byte buffer, so this is a small,
/// direct replacement for that one purpose.
fn write_decimal(out: &mut [u8], value: usize) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 20]; // usize::MAX in decimal fits comfortably.
    let mut count = 0;
    let mut remaining = value;
    while remaining > 0 {
        digits[count] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        count += 1;
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    count
}

fn filesystem_result(result: Result<(), disk::Error>, success: &str) {
    match result {
        Ok(()) if !success.is_empty() => println!("{success}"),
        Ok(()) => {}
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn cat_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: cat [diskN] NAME");
        return;
    }
    match fosfs::cat(info, device, name) {
        Ok(true) => {}
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn write_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_two_arguments(arguments);
    let (name, text) = take_word(arguments);
    if name.is_empty() || text.is_empty() {
        println!("Usage: write [diskN] NAME TEXT");
        return;
    }
    match fosfs::write(info, device, name, text) {
        Ok(fosfs::WriteOutcome::Written) => println!("File written."),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn remove_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: rm [diskN] NAME");
        return;
    }
    match fosfs::remove(info, device, name) {
        Ok(true) => println!("File removed."),
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// `copy SRC_DISK NAME DST_DISK [NEW_NAME]` or, equivalently, `copy
/// diskN/NAME DST_DISK[/NEW_NAME]` (the `diskN/NAME` spelling every other
/// disk command takes). Unlike every other FOSFS
/// command, both disks here must be spelled out explicitly (even `disk1`)
/// rather than one side defaulting to the boot disk: with two disks
/// genuinely in play, a silent default would be ambiguous about which side
/// it applies to. Either side may instead name a discovered NETFS peer:
/// `copy HOST[:diskN]/NAME DST_DISK [NEW_NAME]` (remote source) or
/// `copy SRC_DISK NAME HOST[:diskN] [NEW_NAME]` (remote destination). A
/// remote *source* is written as one `HOST[:diskN]/NAME` token (mirroring
/// `tape run`'s `@HOST[:diskN]/NAME`), disambiguated from a local
/// `SRC_DISK` token by the presence of `/` -- FOSFS names never contain
/// one. A remote *destination* has no file name of its own to combine
/// with, so it's just `HOST[:diskN]`, same shape as a local `DST_DISK`.
/// Copying directly between two remote hosts is deliberately rejected;
/// copy through a local disk in two steps instead. Reuses the shared
/// `pmem::TAPE_BASE` staging buffer that `tape load disk` already uses for
/// "read a whole file into memory, then do something with it," rather
/// than adding a second large static buffer; see `copy_file`'s doc comment
/// for why that is safe.
#[inline(never)]
fn copy_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: [&str; 3] = [
        "Usage: copy SRC DST [NEW_NAME]   SRC: diskN/NAME or HOST[:diskN]/NAME",
        "       DST: diskN[/NAME] or HOST[:diskN][/NAME]",
        "       (older form: copy SRC_DISK NAME DST [NEW_NAME])",
    ];
    let usage = || USAGE.iter().for_each(|line| println!("{line}"));
    let (first, remainder) = take_word(arguments);
    // The source is either one `diskN/NAME` / `HOST[:diskN]/NAME` word or,
    // in the older spelling, `SRC_DISK NAME` as two words.
    let (source, name, remainder) = if let Some(slash) = first.iter().position(|&b| b == b'/') {
        let (head, name) = (&first[..slash], &first[slash + 1..]);
        if name.is_empty() {
            usage();
            return;
        }
        if let Some(device) = strip_disk_selector(head) {
            (CopyDestination::Local(device), name, remainder)
        } else if let Some((peer, remote_disk_number)) = parse_remote_host_token(head) {
            (CopyDestination::Remote(peer, remote_disk_number), name, remainder)
        } else {
            println!("Unknown source: SRC must be an attached disk (e.g. disk1/NAME)");
            println!("or a discovered HOST[:diskN]/NAME.");
            return;
        }
    } else {
        let (name, remainder) = take_word(remainder);
        let Some(device) = strip_disk_selector(first) else {
            usage();
            println!("SRC_DISK must name an attached disk (e.g. disk1).");
            return;
        };
        (CopyDestination::Local(device), name, remainder)
    };
    let (destination_word, remainder) = take_word(remainder);
    let (new_name, remainder) = take_word(remainder);
    if name.is_empty() || destination_word.is_empty() || !remainder.is_empty() {
        usage();
        return;
    }
    // `DST/NAME` carries the new name itself; a separate NEW_NAME word too
    // would be two answers to one question.
    let (destination_word, inline_name) =
        match destination_word.iter().position(|&b| b == b'/') {
            Some(slash) => (&destination_word[..slash], &destination_word[slash + 1..]),
            None => (destination_word, &[][..]),
        };
    if !inline_name.is_empty() && !new_name.is_empty() {
        println!("Give the new name once: either DST/NAME or a NEW_NAME word.");
        return;
    }
    let destination_name = if !inline_name.is_empty() {
        inline_name
    } else if !new_name.is_empty() {
        new_name
    } else {
        name
    };
    let Some(destination) = parse_copy_destination(destination_word) else {
        usage();
        println!("DST must name an attached disk or a discovered HOST[:diskN].");
        return;
    };
    match (source, destination) {
        (CopyDestination::Local(source), CopyDestination::Local(destination)) => {
            copy_file(info, source, name, destination, destination_name);
        }
        (CopyDestination::Local(source), CopyDestination::Remote(peer, remote_disk_number)) => {
            copy_to_remote(info, source, name, peer, remote_disk_number, destination_name);
        }
        (CopyDestination::Remote(peer, remote_disk_number), CopyDestination::Local(destination)) => {
            copy_from_remote(info, peer, remote_disk_number, name, destination, destination_name);
        }
        (CopyDestination::Remote(..), CopyDestination::Remote(..)) => {
            println!("copy can't move a file directly between two remote hosts;");
            println!("copy through a local disk in two steps instead.");
        }
    }
}

enum CopyDestination {
    Local(disk::Device),
    Remote([u8; 6], u8),
}

/// Parses `copy`'s destination word as either a local `diskN` selector or a
/// discovered `HOST[:diskN]` NETFS peer reference. Returns `None` when the
/// word is neither, so the caller can report a single combined usage
/// message rather than guessing which one the operator meant.
fn parse_copy_destination(word: &[u8]) -> Option<CopyDestination> {
    if let Some(device) = strip_disk_selector(word) {
        return Some(CopyDestination::Local(device));
    }
    parse_remote_host_token(word)
        .map(|(peer, remote_disk_number)| CopyDestination::Remote(peer, remote_disk_number))
}

/// Fetches a file from a discovered peer's own disk over NETFS and writes
/// it to a local disk, the remote-source counterpart to `copy_file`.
/// Shares the same staging buffer and invalidation logic.
fn copy_from_remote(
    info: &BootInfo,
    peer: [u8; 6],
    remote_disk_number: u8,
    name: &[u8],
    destination: disk::Device,
    destination_name: &[u8],
) {
    let Some(nic) = nic_or_report() else {
        return;
    };
    let own_mac = nic.mac_address();
    let Some(address) = allocate_tape_buffer(pmem::TAPE_LEN) else {
        return;
    };
    let buffer =
        unsafe { core::slice::from_raw_parts_mut(address as *mut u8, pmem::TAPE_LEN as usize) };
    let length = match netfs_get(nic, peer, own_mac, remote_disk_number, name, buffer) {
        Ok(length) => length,
        Err(error) => {
            println!("NETFS fetch failed: {error}");
            return;
        }
    };
    let outcome = fosfs::write(info, destination, destination_name, &buffer[..length]);
    invalidate_tape_after_scratch_use();
    match outcome {
        Ok(fosfs::WriteOutcome::Written) => println!(
            "Copied {length} byte(s) over NETFS to disk{} '{}'.",
            device_number(destination),
            core::str::from_utf8(destination_name).unwrap_or("?")
        ),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// Reads a file from a local disk and sends it to a discovered peer's own
/// disk over NETFS, the remote-destination counterpart to `copy_file`.
/// Shares the same staging buffer and invalidation logic.
fn copy_to_remote(
    info: &BootInfo,
    source: disk::Device,
    name: &[u8],
    peer: [u8; 6],
    remote_disk_number: u8,
    destination_name: &[u8],
) {
    let length = match fosfs::file_length(info, source, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length > u32::MAX as u64 {
        println!("File is too large to copy through the shared tape-buffer scratch area.");
        return;
    }
    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, source, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    let Some(nic) = nic_or_report() else {
        invalidate_tape_after_scratch_use();
        return;
    };
    let own_mac = nic.mac_address();
    let outcome = netfs_put(nic, peer, own_mac, remote_disk_number, destination_name, buffer);
    invalidate_tape_after_scratch_use();
    match outcome {
        Ok(()) => println!(
            "Copied {length} byte(s) from disk{} '{}' over NETFS to '{}'.",
            device_number(source),
            core::str::from_utf8(name).unwrap_or("?"),
            core::str::from_utf8(destination_name).unwrap_or("?")
        ),
        Err(error) => println!("NETFS send failed: {error}"),
    }
}

/// Reads a whole file from `source` into the shared tape staging buffer
/// (`pmem::TAPE_BASE`/`TAPE_LEN`, the same one `tape load disk`/`tape load
/// net` use) and writes it back out to `destination` under
/// `destination_name`. This is safe to share with the tape subsystem
/// because nothing here calls `install_tape`: a copy never becomes a
/// loaded tape, and any tape that *was* loaded there is invalidated
/// afterward (its bytes are now whatever file was just copied through),
/// exactly like loading a genuinely new tape already overwrites the
/// previous one in place.
fn copy_file(
    info: &BootInfo,
    source: disk::Device,
    name: &[u8],
    destination: disk::Device,
    destination_name: &[u8],
) {
    let length = match fosfs::file_length(info, source, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length > u32::MAX as u64 {
        println!("File is too large to copy through the shared tape-buffer scratch area.");
        return;
    }
    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, source, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    let outcome = fosfs::write(info, destination, destination_name, buffer);
    invalidate_tape_after_scratch_use();
    match outcome {
        Ok(fosfs::WriteOutcome::Written) => println!(
            "Copied {length} byte(s) from disk{} '{}' to disk{} '{}'.",
            device_number(source),
            core::str::from_utf8(name).unwrap_or("?"),
            device_number(destination),
            core::str::from_utf8(destination_name).unwrap_or("?")
        ),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// Clears any previously loaded tape's bookkeeping after `copy_file` reuses
/// the tape staging buffer as scratch space, and says so -- silently
/// leaving stale `TAPE`/`TAPE_ENTRY` state around would let a later `tape
/// run` misinterpret the just-copied file's bytes as tape code instead of
/// honestly reporting "nothing is loaded."
fn invalidate_tape_after_scratch_use() {
    let had_tape = tape_get().is_some();
    unsafe {
        *(&raw mut TAPE) = None;
    }
    if had_tape {
        println!("Note: this reused the shared tape buffer, so the previously");
        println!("loaded tape is gone; 'tape load' again before 'tape run'.");
    }
}

fn print_disk_info(info: &BootInfo, device: disk::Device) {
    println!(
        "disk{}: {} sectors; FOSFS partition: LBA {} + {} sectors",
        device_number(device),
        disk::device_sector_count(info, device),
        disk::device_data_partition_lba(info, device),
        disk::device_data_partition_sectors(info, device)
    );
    println!("Only FOSFS commands may write; raw sector inspection is read-only.");
    let secondary = disk::secondary_count();
    if device == disk::Device::Boot && secondary > 0 {
        println!(
            "{secondary} additional disk(s) attached: disk2..disk{}. Give diskinfo diskN for details.",
            secondary + 1
        );
    }
}

#[inline(never)]
fn read_sector_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (value, remainder) = take_word(arguments);
    let Some(lba) = parse_number(value) else {
        println!("Usage: readsector [diskN] LBA (decimal or 0x hexadecimal)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: readsector [diskN] LBA");
        return;
    }
    match disk::device_read_sector(info, device, lba as u64, |sector| {
        dump_bytes(lba * 512, sector)
    }) {
        Ok(()) => {}
        Err(error) => println!("Disk read failed: {error:?}"),
    }
}

#[inline(never)]
fn hexdump_command(info: &BootInfo, arguments: &[u8]) {
    let (address, remainder) = take_word(arguments);
    let (length, extra) = take_word(remainder);
    let (Some(address), Some(length)) = (parse_number(address), parse_number(length)) else {
        println!("Usage: hexdump ADDRESS LENGTH (kernel memory only; maximum 128 bytes)");
        return;
    };
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    if !extra.is_empty()
        || length == 0
        || length > 128
        || address < kernel_start
        || address
            .checked_add(length)
            .is_none_or(|end| end > kernel_end)
    {
        println!("Address must be within the reserved kernel range; maximum is 128 bytes.");
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    dump_bytes(address, bytes);
}

fn dump_bytes(base: u32, bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        print!("0x{:08x}: ", base + (row * 16) as u32);
        for byte in chunk {
            print!("{byte:02x} ");
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        print!("|");
        for byte in chunk {
            print!(
                "{}",
                if (0x20..=0x7e).contains(byte) {
                    *byte as char
                } else {
                    '.'
                }
            );
        }
        println!("|");
    }
}

fn parse_number(value: &[u8]) -> Option<u32> {
    let (radix, digits) = if value.starts_with(b"0x") {
        (16, &value[2..])
    } else {
        (10, value)
    };
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u32, |number, digit| {
        let value = match *digit {
            b'0'..=b'9' => u32::from(*digit - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(*digit - b'a' + 10),
            b'A'..=b'F' if radix == 16 => u32::from(*digit - b'A' + 10),
            _ => return None,
        };
        if value >= radix {
            return None;
        }
        number.checked_mul(radix)?.checked_add(value)
    })
}

fn print_boot_info(info: &BootInfo) {
    let boot_drive = info.boot_drive;
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    let serial_available = info.serial_available != 0;
    println!(
        "Boot drive: 0x{boot_drive:02x}; serial: {}; kernel: 0x{kernel_start:08x}-0x{kernel_end:08x}",
        if serial_available {
            "available"
        } else {
            "unavailable"
        }
    );
    if disk::boot_is_usb() {
        println!("disk1 is promoted: reads/writes go through the native");
        println!("USB driver, not BIOS (see 'usb storage promote').");
    }
}

#[inline(never)]
fn print_memory_map(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    println!("Firmware memory map ({count} entries reported by BIOS INT 15h EAX=E820h):");
    for index in 0..count {
        let region =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        print_region(index, region);
    }
}

fn e820_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "usable RAM",
        2 => "reserved (not usable)",
        3 => "ACPI reclaimable",
        4 => "ACPI NVS",
        5 => "bad memory",
        _ => "unknown type",
    }
}

fn print_memory_summary(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    let mut usable = 0u64;
    let mut reserved = 0u64;
    for index in 0..count {
        let region =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        let length = read_u64(core::ptr::addr_of!(region.length));
        if read_u32(core::ptr::addr_of!(region.kind)) == 1 {
            usable = usable.saturating_add(length);
        } else {
            reserved = reserved.saturating_add(length);
        }
    }
    println!("E820 summary: {usable} usable bytes; {reserved} non-usable bytes");
    println!("(reserved/ACPI/bad).");
}

fn print_region(index: usize, region: MemoryRegion) {
    let base = read_u64(core::ptr::addr_of!(region.base));
    let length = read_u64(core::ptr::addr_of!(region.length));
    let kind = read_u32(core::ptr::addr_of!(region.kind));
    println!(
        "  {index:02}: 0x{base:016x} + 0x{length:016x} type {kind} ({})",
        e820_type_name(kind)
    );
}

fn read_u16(pointer: *const u16) -> u16 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u32(pointer: *const u32) -> u32 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u64(pointer: *const u64) -> u64 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
