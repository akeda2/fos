//! USB Mass Storage Class, Bulk-Only Transport (BOT) driver, layered on
//! top of `kernel/src/uhci.rs`'s UHCI controller driver and its
//! `bulk_transfer` primitive -- see AGENTS.md's USB section for why this
//! exists (a real Pentium 4 USB-boot machine's BIOS legacy `INT13h`
//! emulation breaks once `uhci::initialize_all()` resets any UHCI
//! controller, so a native driver is the only way for fos to keep using
//! that disk once running).
//!
//! Deliberately its own file rather than folded into `uhci.rs` alongside
//! `UsbAudioDevice`: Bulk-Only Transport's CBW/CSW framing and the SCSI
//! command subset below are a real, separate protocol layer above raw
//! bulk transfers, the same way `es1370.rs`/`hda.rs` are their own files
//! despite depending on `pci.rs`.
//!
//! Scope is deliberately narrow, matching this project's established
//! "reject rather than emulate" convention: only interface class 0x08
//! (Mass Storage) / subclass 0x06 (SCSI transparent) / protocol 0x50
//! (Bulk-Only Transport) is recognized -- the universal choice for real
//! USB flash drives -- and only a native 512-byte block size is accepted
//! (`read_capacity_10` rejects anything else outright rather than
//! attempting 4Kn-to-512-byte emulation).
//!
//! This module has no knowledge of FOSFS or `disk.rs`; it is exercised
//! standalone by the `usb storage` shell command for now (enumerate,
//! print `INQUIRY`/`READ CAPACITY`, hex-dump one sector) -- integrating
//! it as an ordinary secondary disk is later, separate work.

use crate::uhci::{self, Uhci};

const MSC_CLASS: u8 = 0x08;
const MSC_SUBCLASS_SCSI: u8 = 0x06;
const MSC_PROTOCOL_BULK_ONLY: u8 = 0x50;

const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC", little-endian on the wire
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS", little-endian on the wire
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;

/// This driver's own block-size contract: every `read_10`/`write_10`
/// buffer must be a whole number of 512-byte blocks, matching FOSFS/
/// `disk.rs`'s own fixed sector size. `read_capacity_10` enforces that the
/// *device* actually reports 512-byte native blocks before this driver
/// will use it at all.
pub const BLOCK_SIZE: usize = 512;

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum Error {
    /// A bulk transfer itself failed (STALL/CRC/timeout/babble) or timed
    /// out; BOT's own reset-recovery procedure has already been driven
    /// before this is returned, so the device should be ready for a
    /// fresh command on the next call. Carries the underlying
    /// `uhci::Error` (added to diagnose a real-hardware-only failure
    /// QEMU's UHCI emulation never reproduced) so a caller can report
    /// exactly which stage/error fired instead of just "it failed."
    TransportFailed(crate::uhci::Error),
    /// The CSW's signature or tag didn't match this command, or its
    /// status byte was the reserved "Phase Error" value -- BOT's own
    /// reset-recovery procedure has already been driven before this is
    /// returned, same as `TransportFailed`.
    PhaseError,
    /// The CSW reported ordinary command failure (status `1`); the
    /// device is still in a normal, usable state -- a caller wanting the
    /// reason should follow up with `request_sense`.
    CommandFailed,
    /// A request this driver deliberately does not support (a native
    /// block size other than 512 bytes, a transfer not a whole number of
    /// blocks, or one exceeding SCSI READ(10)/WRITE(10)'s own 16-bit
    /// block-count field).
    Unsupported,
}

fn wait_ticks(count: u64) {
    let start = crate::interrupts::ticks();
    while crate::interrupts::ticks().wrapping_sub(start) < count {}
}

/// The two bulk endpoints (and their owning interface) of a discovered
/// Bulk-Only Transport Mass Storage interface.
#[derive(Clone, Copy)]
struct MscEndpoints {
    interface_number: u8,
    in_endpoint: u8,
    in_max_packet: u8,
    out_endpoint: u8,
    out_max_packet: u8,
}

/// Walks a configuration descriptor's raw bytes generically, one
/// sub-descriptor at a time (each one's own `bLength` at byte 0, the same
/// shape `uhci::find_uac_endpoint` already walks), looking for one
/// interface matching `MSC_CLASS`/`MSC_SUBCLASS_SCSI`/
/// `MSC_PROTOCOL_BULK_ONLY` with a bulk IN and a bulk OUT endpoint. Unlike
/// UAC1 (where alternate setting 0 is always a zero-bandwidth "idle"
/// setting that must be skipped), Mass Storage has no such convention --
/// the first matching interface's endpoints are used directly.
fn find_msc_endpoints(config: &[u8]) -> Option<MscEndpoints> {
    const DESC_INTERFACE: u8 = 0x04;
    const DESC_ENDPOINT: u8 = 0x05;
    const TRANSFER_TYPE_BULK: u8 = 0x02;

    let mut offset = 0usize;
    let mut interface_number = 0u8;
    let mut in_msc_interface = false;
    let mut in_endpoint: Option<(u8, u8)> = None;
    let mut out_endpoint: Option<(u8, u8)> = None;

    while offset + 1 < config.len() {
        let length = config[offset] as usize;
        if length < 2 || offset + length > config.len() {
            break;
        }
        let descriptor_type = config[offset + 1];
        match descriptor_type {
            DESC_INTERFACE if length >= 9 => {
                let candidate_interface = config[offset + 2];
                let class = config[offset + 5];
                let subclass = config[offset + 6];
                let protocol = config[offset + 7];
                in_msc_interface =
                    class == MSC_CLASS && subclass == MSC_SUBCLASS_SCSI && protocol == MSC_PROTOCOL_BULK_ONLY;
                if in_msc_interface {
                    interface_number = candidate_interface;
                    in_endpoint = None;
                    out_endpoint = None;
                }
            }
            DESC_ENDPOINT if in_msc_interface && length >= 7 => {
                let endpoint_address = config[offset + 2];
                let attributes = config[offset + 3];
                let max_packet_size = u16::from_le_bytes([config[offset + 4], config[offset + 5]]);
                if attributes & 0x03 == TRANSFER_TYPE_BULK {
                    // Full-speed bulk is capped at 64 bytes/packet; UHCI
                    // (this driver's only host-controller family) is
                    // full-speed-only, so a larger declared value can't
                    // legitimately occur here.
                    let max_packet = max_packet_size.min(64) as u8;
                    if endpoint_address & 0x80 != 0 {
                        in_endpoint = Some((endpoint_address, max_packet));
                    } else {
                        out_endpoint = Some((endpoint_address, max_packet));
                    }
                }
                if let (Some((in_addr, in_mp)), Some((out_addr, out_mp))) = (in_endpoint, out_endpoint) {
                    return Some(MscEndpoints {
                        interface_number,
                        in_endpoint: in_addr,
                        in_max_packet: in_mp,
                        out_endpoint: out_addr,
                        out_max_packet: out_mp,
                    });
                }
            }
            _ => {}
        }
        offset += length;
    }
    None
}

/// Scans every discovered controller/port for devices offering a usable
/// Bulk-Only Transport Mass Storage interface, opening as many as `out`
/// has room for -- unlike a single `find_msc_device` call used to,
/// this does not stop at the first match, so more than one native USB
/// Mass Storage device can be opened in one pass (see `disk.rs`'s
/// `promote_all_and_attach`, which needs to cross-validate every
/// BIOS-backed disk against whatever is actually attached). Each device
/// is given its own address via `uhci::allocate_device_address` rather
/// than a single hardcoded value, so two devices opened in the same pass
/// don't collide. Completes `SET_ADDRESS`/`SET_CONFIGURATION` for each
/// (no `set_interface` needed -- Mass Storage's default alternate setting
/// is already the operative one), mirroring `uhci::find_uac_device`'s
/// "reset and redo everything from scratch, one port reset per attempt"
/// shape per port. Returns how many devices were opened.
pub fn open_all(out: &mut [Option<MscDevice>]) -> usize {
    let mut opened = 0usize;
    for index in 0..uhci::count() {
        let Some(controller) = uhci::get(index) else { continue };
        for port in 0..uhci::UHCI_PORTS {
            if opened >= out.len() {
                return opened;
            }
            let Ok(low_speed) = controller.reset_port(port) else { continue };
            if low_speed {
                continue;
            }
            let mut first8 = [0u8; 8];
            if controller.get_descriptor(0, 8, uhci::USB_DESCRIPTOR_DEVICE, &mut first8).is_err() {
                continue;
            }
            let max_packet0 = first8[7];
            let Some(device_address) = uhci::allocate_device_address() else { continue };
            if controller.set_address(device_address).is_err() {
                continue;
            }
            let mut config_header = [0u8; 9];
            if controller
                .get_descriptor(device_address, max_packet0, uhci::USB_DESCRIPTOR_CONFIGURATION, &mut config_header)
                .is_err()
            {
                continue;
            }
            let total_length = (u16::from_le_bytes([config_header[2], config_header[3]]) as usize)
                .min(uhci::MAX_DESCRIPTOR_BYTES);
            let mut config = [0u8; uhci::MAX_DESCRIPTOR_BYTES];
            let Ok(read) = controller.get_descriptor(
                device_address,
                max_packet0,
                uhci::USB_DESCRIPTOR_CONFIGURATION,
                &mut config[..total_length],
            ) else {
                continue;
            };
            let Some(endpoints) = find_msc_endpoints(&config[..read]) else {
                continue;
            };
            // bConfigurationValue is the configuration descriptor's 6th byte.
            let config_value = config.get(5).copied().unwrap_or(0);
            if controller.set_configuration(device_address, max_packet0, config_value).is_err() {
                continue;
            }
            out[opened] = Some(MscDevice {
                controller,
                endpoints,
                device_address,
                control_max_packet: max_packet0,
                // Freshly `SET_CONFIGURATION`'d: the device's own toggles
                // start at DATA0 per the USB spec's own "configuration
                // selection resets all non-control endpoint state" rule.
                in_toggle: false,
                out_toggle: false,
                next_tag: 0,
            });
            opened += 1;
        }
    }
    opened
}

fn build_cbw(tag: u32, data_transfer_length: u32, data_in: bool, lun: u8, cdb: &[u8]) -> [u8; CBW_LEN] {
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&data_transfer_length.to_le_bytes());
    cbw[12] = if data_in { 0x80 } else { 0x00 };
    cbw[13] = lun;
    cbw[14] = cdb.len() as u8;
    cbw[15..15 + cdb.len()].copy_from_slice(cdb);
    cbw
}

/// One discovered, already-`SET_CONFIGURATION`'d USB Mass Storage device.
/// Owns the persistent per-endpoint DATA0/DATA1 toggle state
/// `uhci::Uhci::bulk_transfer` requires (see that function's own doc
/// comment for why `Uhci` itself can't track this), plus the
/// monotonically increasing CBW tag BOT's own spec expects. `Copy` (every
/// field already is) so `disk.rs`'s multi-device matching
/// (`promote_all_and_attach`) can freely try a discovered candidate
/// against one disk's baseline without losing it if that attempt fails --
/// a genuine mutation (advancing the toggle/tag state) only ever needs to
/// persist once a device is actually kept, at which point it is moved
/// into its owning static by value, not shared.
#[derive(Clone, Copy)]
pub struct MscDevice {
    controller: &'static Uhci,
    endpoints: MscEndpoints,
    device_address: u8,
    control_max_packet: u8,
    in_toggle: bool,
    out_toggle: bool,
    next_tag: u32,
}

/// Discovers and opens the first Bulk-Only Transport Mass Storage device
/// found on any UHCI controller/port, via `open_all` with a one-element
/// buffer. Re-discovers from scratch every call, the same "no persistent
/// handle across calls" precedent `uhci::find_uac_device` already set.
pub fn open() -> Option<MscDevice> {
    let mut devices = [const { None }; 1];
    open_all(&mut devices);
    let [device] = devices;
    device
}

impl MscDevice {
    fn next_tag(&mut self) -> u32 {
        self.next_tag = self.next_tag.wrapping_add(1);
        self.next_tag
    }

    /// BOT's own mandated reset-recovery procedure: a class-specific
    /// Bulk-Only Mass Storage Reset, then `CLEAR_FEATURE(ENDPOINT_HALT)`
    /// on both bulk endpoints -- resetting the device's own toggle state
    /// back to DATA0, matched here by resetting this driver's own
    /// tracked toggles the same way. Each step's own failure is ignored
    /// (this is already the failure-recovery path; there is no deeper
    /// fallback than "the next command will find out whether it worked"),
    /// mirroring `usb_play_command`'s own "best-effort teardown, not
    /// reported" convention. `pub(crate)` (rather than private) so
    /// `disk.rs`'s `try_promote_with_candidate` can also call it directly
    /// after a failed cross-validation attempt, leaving a rejected
    /// candidate in a known-good state for a possible retry against a
    /// different disk.
    pub(crate) fn recover(&mut self) {
        let _ = self.controller.bulk_only_mass_storage_reset(
            self.device_address,
            self.control_max_packet,
            self.endpoints.interface_number,
        );
        let _ =
            self.controller
                .clear_endpoint_halt(self.device_address, self.control_max_packet, self.endpoints.in_endpoint);
        let _ = self.controller.clear_endpoint_halt(
            self.device_address,
            self.control_max_packet,
            self.endpoints.out_endpoint,
        );
        self.in_toggle = false;
        self.out_toggle = false;
    }

    /// Runs [`command_once`](Self::command_once) up to `MAX_COMMAND_ATTEMPTS`
    /// times, retrying (after `recover`'s reset-recovery, already run
    /// internally by `command_once`) on a transport-level or phase error --
    /// confirmed necessary on real hardware: a genuine Kingston flash
    /// drive occasionally times out a single bulk transfer within an
    /// otherwise-correct exchange (at an unpredictable stage -- sometimes
    /// the data stage, sometimes the CSW), something QEMU's synthetic
    /// usb-storage device never reproduces. A `CommandFailed` (an
    /// ordinary SCSI-level "not this time" answer, e.g. a transient
    /// not-ready condition) is a real, meaningful response, not a
    /// transport glitch, so it is returned immediately rather than
    /// retried here -- `wait_until_ready`'s own bounded retry loop
    /// already handles that case at a higher level.
    fn command(&mut self, lun: u8, cdb: &[u8], buffer: &mut [u8], data_in: bool) -> Result<usize, Error> {
        const MAX_COMMAND_ATTEMPTS: u32 = 3;
        let mut last_error = Error::TransportFailed(crate::uhci::Error::Timeout);
        for attempt in 0..MAX_COMMAND_ATTEMPTS {
            match self.command_once(lun, cdb, buffer, data_in) {
                Ok(len) => return Ok(len),
                Err(Error::CommandFailed) => return Err(Error::CommandFailed),
                Err(error) => {
                    last_error = error;
                    if attempt + 1 < MAX_COMMAND_ATTEMPTS {
                        wait_ticks(1);
                    }
                }
            }
        }
        Err(last_error)
    }

    /// One single attempt at a full Bulk-Only Transport exchange: CBW
    /// out, an optional data stage (`buffer`, direction per `data_in`),
    /// CSW in. Returns the number of data-stage bytes actually
    /// transferred (only meaningful for `data_in`, mirroring
    /// `Uhci::control_transfer`'s identical convention). On any
    /// transport-level or phase error, drives `recover` before
    /// returning, so a following attempt (whether `command`'s own retry
    /// above, or an entirely separate command) starts from a clean,
    /// known device state rather than compounding a stuck one.
    fn command_once(&mut self, lun: u8, cdb: &[u8], buffer: &mut [u8], data_in: bool) -> Result<usize, Error> {
        let tag = self.next_tag();
        let mut cbw = build_cbw(tag, buffer.len() as u32, data_in, lun, cdb);
        if let Err(error) = self.controller.bulk_transfer(
            self.device_address,
            self.endpoints.out_endpoint & 0x0f,
            self.endpoints.out_max_packet,
            &mut cbw,
            false,
            &mut self.out_toggle,
        ) {
            self.recover();
            return Err(Error::TransportFailed(error));
        }
        // Every bulk_transfer call stops the queue on its own completion
        // and the *next* one restarts it fresh from scratch -- unlike
        // `control_transfer`, which keeps one QH continuously armed across
        // its entire SETUP+DATA+STATUS chain without ever stopping it
        // mid-exchange. A brief settle delay here, before the very next
        // stage restarts that queue, is the same real-hardware-only
        // turnaround-time accommodation already confirmed necessary
        // between the data stage and the CSW below.
        wait_ticks(1);

        let mut transferred = 0usize;
        if !buffer.is_empty() {
            let endpoint = if data_in { self.endpoints.in_endpoint } else { self.endpoints.out_endpoint };
            let max_packet = if data_in { self.endpoints.in_max_packet } else { self.endpoints.out_max_packet };
            let toggle = if data_in { &mut self.in_toggle } else { &mut self.out_toggle };
            match self.controller.bulk_transfer(
                self.device_address,
                endpoint & 0x0f,
                max_packet,
                buffer,
                data_in,
                toggle,
            ) {
                Ok(len) => transferred = len,
                Err(error) => {
                    self.recover();
                    return Err(Error::TransportFailed(error));
                }
            }
            // A real device needs a brief moment after finishing its own
            // data stage before it's ready to send the CSW -- confirmed
            // necessary on real hardware (not reproduced by QEMU's
            // synthetic usb-storage device at all): without this, the
            // CSW-in bulk transfer immediately below reliably times out
            // (Active never clears) on a genuine Kingston flash drive,
            // even though the exact same IN endpoint just successfully
            // completed the data stage moments earlier.
            wait_ticks(1);
        }

        let mut csw = [0u8; CSW_LEN];
        if let Err(error) = self.controller.bulk_transfer(
            self.device_address,
            self.endpoints.in_endpoint & 0x0f,
            self.endpoints.in_max_packet,
            &mut csw,
            true,
            &mut self.in_toggle,
        ) {
            self.recover();
            return Err(Error::TransportFailed(error));
        }
        // Settle time after this command's own final stage too, not just
        // between this command's internal stages -- confirmed necessary
        // for back-to-back *separate* commands on the same device (e.g.
        // FOSFS's own multi-sector directory read, several independent
        // `read_10` calls in a tight loop with no caller-side pacing of
        // its own), the same real-hardware-only turnaround-time
        // requirement as the intra-command delays above.
        wait_ticks(1);

        let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
        let csw_tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
        let status = csw[12];
        if signature != CSW_SIGNATURE || csw_tag != tag {
            self.recover();
            return Err(Error::PhaseError);
        }
        match status {
            0 => Ok(transferred),
            1 => Err(Error::CommandFailed),
            _ => {
                self.recover();
                Err(Error::PhaseError)
            }
        }
    }

    /// `INQUIRY`: reads back up to `out.len()` bytes of the standard
    /// inquiry data (vendor/product identification, peripheral device
    /// type in the low 5 bits of byte 0 -- `0x00` is "direct access
    /// block device", the only type this driver expects).
    pub fn inquiry(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let len = out.len().min(0xff);
        let cdb = [0x12, 0x00, 0x00, 0x00, len as u8, 0x00];
        self.command(0, &cdb, &mut out[..len], true)
    }

    /// `TEST UNIT READY`, retried up to `attempts` times with a short
    /// delay between tries -- real removable/flash media can report "not
    /// ready" for a moment right after enumeration while it finishes its
    /// own internal spin-up/mount, the same bounded-retry idea
    /// `Uhci::reset_port`'s own polling loop already uses for a different
    /// wait.
    pub fn wait_until_ready(&mut self, attempts: u32) -> bool {
        for attempt in 0..attempts {
            let cdb = [0x00u8; 6];
            if self.command(0, &cdb, &mut [], true).is_ok() {
                return true;
            }
            if attempt + 1 < attempts {
                wait_ticks(10); // 100 ms at 100 Hz
            }
        }
        false
    }

    /// `REQUEST SENSE`: decodes why the most recent command reported
    /// `Error::CommandFailed` (sense key at byte 2, ASC/ASCQ at bytes
    /// 12/13 of the standard fixed sense format) into raw bytes for the
    /// caller to print -- this driver does not maintain its own
    /// sense-code-to-string table.
    pub fn request_sense(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let len = out.len().min(0xff);
        let cdb = [0x03, 0x00, 0x00, 0x00, len as u8, 0x00];
        self.command(0, &cdb, &mut out[..len], true)
    }

    /// `READ CAPACITY (10)`: returns `(block_count, block_size_bytes)`.
    /// Rejects (`Err(Error::Unsupported)`) any device whose native block
    /// size isn't `BLOCK_SIZE` (512) bytes -- see this module's own doc
    /// comment for why 4Kn emulation is deliberately out of scope.
    pub fn read_capacity_10(&mut self) -> Result<(u32, u32), Error> {
        let cdb = [0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut data = [0u8; 8];
        self.command(0, &cdb, &mut data, true)?;
        let last_lba = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if block_size as usize != BLOCK_SIZE {
            return Err(Error::Unsupported);
        }
        Ok((last_lba.wrapping_add(1), block_size))
    }

    /// `READ(10)`: `buffer.len()` must be a nonzero, whole multiple of
    /// `BLOCK_SIZE` no larger than SCSI READ(10)'s own 16-bit
    /// transfer-length field allows (65535 blocks).
    pub fn read_10(&mut self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        let blocks = block_count(buffer.len())?;
        let cdb = read_write_cdb(0x28, lba, blocks);
        self.command(0, &cdb, buffer, true).map(|_| ())
    }

    /// `WRITE(10)`, the mirror image of `read_10` -- same block-count
    /// contract. Called from `kernel/src/disk.rs`'s `usb_transfer`, never
    /// from the standalone `usb storage` shell command (deliberately
    /// read-only -- see its own doc comment in `shell.rs`).
    pub fn write_10(&mut self, lba: u32, buffer: &mut [u8]) -> Result<(), Error> {
        let blocks = block_count(buffer.len())?;
        let cdb = read_write_cdb(0x2a, lba, blocks);
        self.command(0, &cdb, buffer, false).map(|_| ())
    }
}

fn block_count(byte_len: usize) -> Result<u16, Error> {
    if byte_len == 0 || byte_len % BLOCK_SIZE != 0 {
        return Err(Error::Unsupported);
    }
    let blocks = byte_len / BLOCK_SIZE;
    if blocks > 0xffff {
        return Err(Error::Unsupported);
    }
    Ok(blocks as u16)
}

fn read_write_cdb(opcode: u8, lba: u32, blocks: u16) -> [u8; 10] {
    [
        opcode,
        0x00,
        (lba >> 24) as u8,
        (lba >> 16) as u8,
        (lba >> 8) as u8,
        lba as u8,
        0x00,
        (blocks >> 8) as u8,
        blocks as u8,
        0x00,
    ]
}
