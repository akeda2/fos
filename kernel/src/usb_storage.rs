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
    /// fresh command on the next call.
    TransportFailed,
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

/// Scans every discovered controller/port for a device offering a usable
/// Bulk-Only Transport Mass Storage interface, completing
/// `SET_ADDRESS`/`SET_CONFIGURATION` (no `set_interface` needed -- Mass
/// Storage's default alternate setting is already the operative one).
/// Mirrors `uhci::find_uac_device`'s exact "reset and redo everything from
/// scratch, one port reset per attempt" shape.
fn find_msc_device() -> Option<(&'static Uhci, u8, MscEndpoints)> {
    const DEVICE_ADDRESS: u8 = 1;

    for index in 0..uhci::count() {
        let Some(controller) = uhci::get(index) else { continue };
        for port in 0..uhci::UHCI_PORTS {
            let Ok(low_speed) = controller.reset_port(port) else { continue };
            if low_speed {
                continue;
            }
            let mut first8 = [0u8; 8];
            if controller.get_descriptor(0, 8, uhci::USB_DESCRIPTOR_DEVICE, &mut first8).is_err() {
                continue;
            }
            let max_packet0 = first8[7];
            if controller.set_address(DEVICE_ADDRESS).is_err() {
                continue;
            }
            let mut config_header = [0u8; 9];
            if controller
                .get_descriptor(DEVICE_ADDRESS, max_packet0, uhci::USB_DESCRIPTOR_CONFIGURATION, &mut config_header)
                .is_err()
            {
                continue;
            }
            let total_length = (u16::from_le_bytes([config_header[2], config_header[3]]) as usize)
                .min(uhci::MAX_DESCRIPTOR_BYTES);
            let mut config = [0u8; uhci::MAX_DESCRIPTOR_BYTES];
            let Ok(read) = controller.get_descriptor(
                DEVICE_ADDRESS,
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
            if controller.set_configuration(DEVICE_ADDRESS, max_packet0, config_value).is_err() {
                continue;
            }
            return Some((controller, max_packet0, endpoints));
        }
    }
    None
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
/// monotonically increasing CBW tag BOT's own spec expects.
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
/// found on any UHCI controller/port. Re-discovers from scratch every
/// call, the same "no persistent handle across calls" precedent
/// `uhci::find_uac_device` already set.
pub fn open() -> Option<MscDevice> {
    const DEVICE_ADDRESS: u8 = 1;
    let (controller, control_max_packet, endpoints) = find_msc_device()?;
    Some(MscDevice {
        controller,
        endpoints,
        device_address: DEVICE_ADDRESS,
        control_max_packet,
        // Freshly `SET_CONFIGURATION`'d: the device's own toggles start
        // at DATA0 per the USB spec's own "configuration selection resets
        // all non-control endpoint state" rule.
        in_toggle: false,
        out_toggle: false,
        next_tag: 0,
    })
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
    /// reported" convention.
    fn recover(&mut self) {
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

    /// Runs one full Bulk-Only Transport exchange: CBW out, an optional
    /// data stage (`buffer`, direction per `data_in`), CSW in. Returns
    /// the number of data-stage bytes actually transferred (only
    /// meaningful for `data_in`, mirroring `Uhci::control_transfer`'s
    /// identical convention). On any transport-level or phase error,
    /// drives `recover` before returning, so the *next* call starts from
    /// a clean, known device state rather than compounding a stuck one.
    fn command(&mut self, lun: u8, cdb: &[u8], buffer: &mut [u8], data_in: bool) -> Result<usize, Error> {
        let tag = self.next_tag();
        let mut cbw = build_cbw(tag, buffer.len() as u32, data_in, lun, cdb);
        if self
            .controller
            .bulk_transfer(
                self.device_address,
                self.endpoints.out_endpoint & 0x0f,
                self.endpoints.out_max_packet,
                &mut cbw,
                false,
                &mut self.out_toggle,
            )
            .is_err()
        {
            self.recover();
            return Err(Error::TransportFailed);
        }

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
                Err(_) => {
                    self.recover();
                    return Err(Error::TransportFailed);
                }
            }
        }

        let mut csw = [0u8; CSW_LEN];
        if self
            .controller
            .bulk_transfer(
                self.device_address,
                self.endpoints.in_endpoint & 0x0f,
                self.endpoints.in_max_packet,
                &mut csw,
                true,
                &mut self.in_toggle,
            )
            .is_err()
        {
            self.recover();
            return Err(Error::TransportFailed);
        }

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
    /// contract. Not yet called anywhere: the standalone `usb storage`
    /// shell command is deliberately read-only (issuing a real write
    /// against whichever USB device happens to enumerate first, with no
    /// disk-selection safeguard, is not a safe default action), so this
    /// is exercised by a dedicated test once `disk.rs` integration lands.
    #[allow(dead_code)]
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
