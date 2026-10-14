//! CPU temperature and package power, for the `power` command (Intel only; AMD and virtual machines report nothing).
//!
//! An MSR the CPU does not implement raises #GP, which this kernel treats as fatal, so every read is guarded: the thermal
//! readouts by CPUID leaf 6 (digital sensor, bit 0; package sensor, bit 6), RAPL (the energy counter) by a list of the
//! family 6 models known to have it, and everything by "not under a hypervisor" (QEMU does not model these MSRs).
//!
//! * Temperature: `IA32_THERM_STATUS` (0x19c) and `IA32_PACKAGE_THERM_STATUS` (0x1b1) report the distance *below* the
//!   throttle point (bits 22:16, valid in bit 31), not degrees. `MSR_TEMPERATURE_TARGET` (0x1a2, bits 23:16) holds that
//!   throttle point (TjMax), so temperature = TjMax - readout.
//! * Power: `MSR_RAPL_POWER_UNIT` (0x606, bits 12:8) says the energy counter ticks in 1/2^N joule; `MSR_PKG_ENERGY_STATUS`
//!   (0x611) is that counter, 32 bits wide. Watts = counter delta * unit / seconds. It covers the CPU package (cores,
//!   uncore, usually the graphics), not the NICs, disks or the screen.
//! * Speed: `IA32_APERF` (0xe8) counts at the speed the core really ran, `IA32_MPERF` (0xe7) at the fixed base rate
//!   while not halted (CPUID.6 ECX bit 0); their ratio is the average speed as a percentage of base (above 100 = turbo).
//! * Frequency scaling, opt-in: left alone, the firmware's setting stays and the CPU runs flat out whenever it is awake.
//!   Hardware P-states (HWP, CPUID.6 EAX bit 7) hand the choice back to the CPU: it sees how busy the core is and ramps
//!   up by itself, so a busy tape (tape-net, audio) gets full speed with no kernel policy. `IA32_PM_ENABLE` (0x770) bit 0
//!   turns it on (it cannot be turned off again before a reset); `IA32_HWP_REQUEST` (0x774) then takes the minimum
//!   (bits 7:0), maximum (15:8), desired (23:16, 0 = automatic) performance and, with CPUID.6 EAX bit 10, the energy
//!   preference (31:24; 0 = performance ... 255 = power saving). `IA32_HWP_CAPABILITIES` (0x771) gives the limits.

use crate::{apic, cpu};

const MSR_THERM_STATUS: u32 = 0x19c;
const MSR_TEMPERATURE_TARGET: u32 = 0x1a2;
const MSR_PACKAGE_THERM_STATUS: u32 = 0x1b1;
const MSR_MPERF: u32 = 0xe7;
const MSR_APERF: u32 = 0xe8;
const MSR_PM_ENABLE: u32 = 0x770;
const MSR_HWP_CAPABILITIES: u32 = 0x771;
const MSR_HWP_REQUEST: u32 = 0x774;
const MSR_RAPL_POWER_UNIT: u32 = 0x606;
const MSR_PKG_ENERGY_STATUS: u32 = 0x611;

/// Family 6 models with RAPL's package energy counter (Sandy Bridge up to the current hybrid parts).
const RAPL_MODELS: &[u32] = &[
    0x2a, 0x2d, 0x3a, 0x3e, 0x3c, 0x45, 0x46, 0x3f, 0x3d, 0x47, 0x4f, 0x56, 0x4e, 0x5e, 0x55, 0x8e, 0x9e, 0x66, 0x7d, 0x7e, 0x8c, 0x8d,
    0x97, 0x9a, 0xa5, 0xa6, 0xb7, 0xba, 0xbf, 0x9c,
];

struct State {
    thermal: bool,
    package_thermal: bool,
    tjmax: Option<u32>,
    rapl: bool,
    /// Joules per counter tick, times 2^32 (so the arithmetic stays in integers).
    energy_unit_q32: u64,
    /// The counter and the time at boot and at the previous `power`: (raw counter, microseconds).
    boot: (u32, u64),
    mark: (u32, u64),
    perf: bool,
    /// APERF and MPERF at boot and at the previous `power`.
    perf_boot: (u64, u64),
    perf_mark: (u64, u64),
}

static mut STATE: State = State { thermal: false, package_thermal: false, tjmax: None, rapl: false, energy_unit_q32: 0, boot: (0, 0), mark: (0, 0), perf: false, perf_boot: (0, 0), perf_mark: (0, 0) };

fn model() -> Option<u32> {
    let (signature, _, ecx, _) = cpu::cpuid(1, 0);
    let (_, ebx, ecx_vendor, edx_vendor) = cpu::cpuid(0, 0);
    let intel = ebx == u32::from_le_bytes(*b"Genu") && edx_vendor == u32::from_le_bytes(*b"ineI") && ecx_vendor == u32::from_le_bytes(*b"ntel");
    let family = (signature >> 8) & 0xf;
    if !intel || family != 6 || ecx & (1 << 31) != 0 {
        return None;
    }
    Some(((signature >> 4) & 0xf) | (((signature >> 16) & 0xf) << 4))
}

/// Looks at what the CPU offers and takes the first energy reading. Call once at boot.
pub fn init() {
    let Some(model) = model() else { return };
    let (max_leaf, ..) = cpu::cpuid(0, 0);
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    if max_leaf >= 6 {
        let (eax, ..) = cpu::cpuid(6, 0);
        state.thermal = eax & 1 != 0;
        state.package_thermal = eax & (1 << 6) != 0;
        state.perf = cpu::cpuid(6, 0).2 & 1 != 0;
    }
    if state.perf {
        state.perf_boot = (cpu::read_msr(MSR_APERF), cpu::read_msr(MSR_MPERF));
        state.perf_mark = state.perf_boot;
    }
    state.rapl = RAPL_MODELS.contains(&model);
    if state.rapl {
        state.tjmax = Some(((cpu::read_msr(MSR_TEMPERATURE_TARGET) >> 16) & 0xff) as u32).filter(|&t| t != 0);
        let unit = (cpu::read_msr(MSR_RAPL_POWER_UNIT) >> 8) & 0x1f;
        state.energy_unit_q32 = (1u64 << 32) >> unit;
        let reading = (cpu::read_msr(MSR_PKG_ENERGY_STATUS) as u32, apic::micros());
        state.boot = reading;
        state.mark = reading;
    }
}

/// The hottest valid thermal readout as (degrees below TjMax, TjMax if known): the package sensor when there is one.
pub fn temperature() -> Option<(u32, Option<u32>)> {
    let state = unsafe { &*core::ptr::addr_of!(STATE) };
    let msr = if state.package_thermal {
        MSR_PACKAGE_THERM_STATUS
    } else if state.thermal {
        MSR_THERM_STATUS
    } else {
        return None;
    };
    let value = cpu::read_msr(msr);
    (value & (1 << 31) != 0).then(|| (((value >> 16) & 0x7f) as u32, state.tjmax))
}

/// Package power in milliwatts, since boot and since the previous call, or None without RAPL.
pub fn power_milliwatts() -> Option<(u64, u64)> {
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    if !state.rapl {
        return None;
    }
    let (now_counter, now_time) = (cpu::read_msr(MSR_PKG_ENERGY_STATUS) as u32, apic::micros());
    let milliwatts = |from: (u32, u64)| {
        let ticks = u64::from(now_counter.wrapping_sub(from.0));
        let microseconds = now_time.saturating_sub(from.1).max(1);
        // millijoules = ticks * unit * 1000; milliwatts = millijoules / seconds
        let millijoules = (u128::from(ticks) * u128::from(state.energy_unit_q32) * 1000) >> 32;
        (millijoules * 1_000_000 / u128::from(microseconds)) as u64
    };
    let result = (milliwatts(state.boot), milliwatts(state.mark));
    state.mark = (now_counter, now_time);
    Some(result)
}

/// Average core speed as a percentage of the base rate (since boot, since the previous call), or None without APERF/MPERF.
pub fn speed_percent() -> Option<(u64, u64)> {
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    if !state.perf {
        return None;
    }
    let now = (cpu::read_msr(MSR_APERF), cpu::read_msr(MSR_MPERF));
    let percent = |from: (u64, u64)| now.0.wrapping_sub(from.0).saturating_mul(100) / now.1.wrapping_sub(from.1).max(1);
    let result = (percent(state.perf_boot), percent(state.perf_mark));
    state.perf_mark = now;
    Some(result)
}

/// Energy preference words for `power hwp` and the `settings` file: (name, EPP value).
pub const PREFERENCES: &[(&str, u64)] = &[("performance", 0), ("balanced", 128), ("power", 255)];

/// None without HWP, else whether the energy preference field exists.
fn hwp_supported() -> Option<bool> {
    model()?;
    let (max_leaf, ..) = cpu::cpuid(0, 0);
    if max_leaf < 6 {
        return None;
    }
    let (eax, ..) = cpu::cpuid(6, 0);
    (eax & (1 << 7) != 0).then_some(eax & (1 << 10) != 0)
}

/// Turns on hardware P-states with the given energy preference (0 performance .. 255 power saving). The request spans the
/// whole range the CPU reports (lowest to highest performance, automatic desired), so the hardware picks the speed.
/// Returns the (lowest, highest) performance levels, or an explanation.
pub fn enable_hwp(preference: u64) -> Result<(u64, u64), &'static str> {
    let Some(has_epp) = hwp_supported() else { return Err("needs an Intel CPU with hardware P-states, not a virtual machine") };
    cpu::write_msr(MSR_PM_ENABLE, 1);
    let capabilities = cpu::read_msr(MSR_HWP_CAPABILITIES);
    let (highest, lowest) = (capabilities & 0xff, (capabilities >> 24) & 0xff);
    if highest == 0 {
        return Err("the CPU reports no performance range");
    }
    let mut request = lowest | highest << 8;
    if has_epp {
        request |= preference.min(255) << 24;
    }
    cpu::write_msr(MSR_HWP_REQUEST, request);
    Ok((lowest, highest))
}

/// What HWP is doing: None when unsupported, else (enabled, minimum, maximum, preference if the CPU has one).
pub fn hwp_status() -> Option<(bool, u64, u64, Option<u64>)> {
    let has_epp = hwp_supported()?;
    if cpu::read_msr(MSR_PM_ENABLE) & 1 == 0 {
        return Some((false, 0, 0, None));
    }
    let request = cpu::read_msr(MSR_HWP_REQUEST);
    Some((true, request & 0xff, (request >> 8) & 0xff, has_epp.then_some((request >> 24) & 0xff)))
}
