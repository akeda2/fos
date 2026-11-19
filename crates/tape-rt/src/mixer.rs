//! The active audio device's hardware mixer (tape ABI 41, the `mixer_*` calls in `abi/src/tape.rs`): a matrix of linear
//! crosspoint gains (sources = inputs, then playback channels) with a master per output, level meters and named controls.

use fosfor_abi::tape::{MixerControl, MixerInfo, MixerMeter, TapeApi, MIXER_MASTER, MIXER_UNKNOWN};

/// A handle on the mixer `mixer_info` described. Every call goes to the kernel: the device (or another tape, or its own
/// front panel) may change a gain at any time; `serial` says when to read again.
#[derive(Clone, Copy)]
pub struct Mixer {
    table: &'static TapeApi,
    pub info: MixerInfo,
}

impl Mixer {
    /// The active device's mixer, or `None` (no mixer, or a kernel older than ABI 41).
    pub fn open() -> Option<Mixer> {
        Self::open_on(crate::api())
    }

    pub fn open_on(table: &'static TapeApi) -> Option<Mixer> {
        let end = core::mem::offset_of!(TapeApi, mixer_control_set) + core::mem::size_of::<usize>();
        if table.version < 41 || (table.size as usize) < end {
            return None;
        }
        let mut info = MixerInfo::default();
        ((table.mixer_info)(&mut info) == 0).then_some(Mixer { table, info })
    }

    pub fn sources(&self) -> u32 {
        self.info.inputs + self.info.playbacks
    }

    /// The device's name for the mixer (from `MixerInfo::name`).
    pub fn title(&self) -> &str {
        text(&self.info.name)
    }

    /// A channel's name (`MIXER_GROUP_*`) into `buffer`; empty when it has none.
    pub fn name<'a>(&self, group: u32, index: u32, buffer: &'a mut [u8]) -> &'a str {
        let count = (self.table.mixer_name)(group, index, buffer.as_mut_ptr(), buffer.len());
        if count <= 0 { "" } else { core::str::from_utf8(&buffer[..count as usize]).unwrap_or("") }
    }

    /// A crosspoint's gain, `None` when the device cannot say (`MIXER_UNKNOWN`) or the call failed.
    pub fn get(&self, output: u32, source: u32) -> Option<u32> {
        let value = (self.table.mixer_get)(output, source);
        (value >= 0).then_some(value as u32)
    }

    /// Whether the crosspoint is unknown rather than failing.
    pub fn unknown(&self, output: u32, source: u32) -> bool {
        (self.table.mixer_get)(output, source) == MIXER_UNKNOWN
    }

    pub fn set(&self, output: u32, source: u32, gain: u32) -> bool {
        (self.table.mixer_set)(output, source, gain) == 0
    }

    pub fn master(&self, output: u32) -> Option<u32> {
        self.get(output, MIXER_MASTER)
    }

    pub fn set_master(&self, output: u32, gain: u32) -> bool {
        self.set(output, MIXER_MASTER, gain)
    }

    pub fn serial(&self) -> u64 {
        (self.table.mixer_serial)()
    }

    /// The meters (inputs, playback, outputs) into `out`: how many were written.
    pub fn meters(&self, out: &mut [MixerMeter]) -> usize {
        (self.table.mixer_meters)(out.as_mut_ptr(), out.len())
    }

    pub fn control(&self, index: u32) -> Option<MixerControl> {
        let mut control = MixerControl::default();
        ((self.table.mixer_control)(index, &mut control) == 0).then_some(control)
    }

    pub fn control_item<'a>(&self, index: u32, item: u32, buffer: &'a mut [u8]) -> &'a str {
        let count = (self.table.mixer_control_item)(index, item, buffer.as_mut_ptr(), buffer.len());
        if count <= 0 { "" } else { core::str::from_utf8(&buffer[..count as usize]).unwrap_or("") }
    }

    pub fn control_set(&self, index: u32, value: i64) -> bool {
        (self.table.mixer_control_set)(index, value) == 0
    }
}

/// A zero-padded UTF-8 field (`MixerInfo::name`, `MixerControl::name`, `unit`) as text.
pub fn text(field: &[u8]) -> &str {
    let end = field.iter().position(|&byte| byte == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).unwrap_or("")
}
