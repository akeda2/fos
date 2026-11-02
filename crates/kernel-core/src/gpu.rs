//! `gpu`: a read-only look at what the firmware left running on an Intel display engine (Skylake and later, gen9
//! register layout). Experiment for a tape that drives two screens: the UEFI GOP shows one framebuffer, so which pipes,
//! planes and DDI ports are really enabled, and where each plane scans out from, tells whether a second screen is lit.
//! Registers are read only (BAR0, uncached); nothing is written. Offsets are from the i915 driver's gen9 headers.

use crate::{paging, pci};

const PIPE_NAMES: [&str; 3] = ["A", "B", "C"];
const DDI_NAMES: [&str; 5] = ["A", "B", "C", "D", "E"];

fn read(base: u64, offset: u64) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

pub fn command() {
    let Some(device) = pci::devices().find(|d| d.vendor == 0x8086 && d.class == 3) else {
        return crate::println!("gpu: no Intel display controller found.");
    };
    let handle = pci::Device { ..device };
    let Some((bar, _)) = handle.memory_bar(0) else { return crate::println!("gpu: BAR0 is not a memory BAR.") };
    // Display registers live in 0x60000-0x73000 and the PCH block at 0xC4000.
    paging::map_device(bar + 0x60000, 0x14000);
    paging::map_device(bar + 0xc4000, 0x1000);
    crate::println!("gpu: Intel {:04x}:{:04x} at {}:{}.{}, BAR0 {:#x}", device.vendor, device.id, device.bus, device.device, device.function, bar);
    for (pipe, name) in PIPE_NAMES.iter().enumerate() {
        let step = pipe as u64 * 0x1000;
        let conf = read(bar, 0x70008 + step);
        let src = read(bar, 0x6001c + step);
        let plane = read(bar, 0x70180 + step);
        let surf = read(bar, 0x7019c + step);
        let stride = read(bar, 0x70188 + step);
        crate::println!(
            "pipe {name}: conf {conf:#010x} {} src {}x{} | plane1 ctl {plane:#010x} {} surf {surf:#x} stride {stride}",
            if conf & 1 << 31 != 0 { "ON " } else { "off" },
            (src >> 16) + 1,
            (src & 0xffff) + 1,
            if plane & 1 << 31 != 0 { "ON " } else { "off" }
        );
    }
    for (index, offset) in [(0u64, 0x60400u64), (1, 0x61400), (2, 0x62400), (3, 0x6f400)].iter().copied() {
        let ctl = read(bar, offset);
        let name = if index == 3 { "EDP" } else { PIPE_NAMES[index as usize] };
        let mode = ["HDMI", "DVI", "DP", "DP-MST", "?", "?", "?", "?"][((ctl >> 24) & 7) as usize];
        crate::println!(
            "transcoder {name}: func {ctl:#010x} {} ddi {} mode {mode}",
            if ctl & 1 << 31 != 0 { "ON " } else { "off" },
            DDI_NAMES.get(((ctl >> 28) & 7) as usize).copied().unwrap_or("-")
        );
    }
    for (index, name) in DDI_NAMES.iter().enumerate() {
        let ctl = read(bar, 0x64000 + index as u64 * 0x100);
        crate::println!("ddi {name}: buf {ctl:#010x} {}", if ctl & 1 << 31 != 0 { "ON" } else { "off" });
    }
    crate::println!("pch hotplug status SDEISR {:#010x}", read(bar, 0xc4000));
}
