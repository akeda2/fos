//! `kernel`: both kernels live on the boot volume as `kernel.embedded` and `kernel.mf`, and `kernel use` switches between them.
//! The switch is what KUP does with an image from the network: the file goes into the inactive A/B slot as the pending one, and
//! the loader gives it one try and falls back to the running kernel if it never reaches its milestone. The checks (ELF, the
//! `.fos_flavour` section, the machine type) and the slot write are the host's (`Host::install_kernel`).

use crate::{Host, sayln, split_word};

/// `(flavour, file)`: the kernels a volume can keep.
const KERNELS: [(&str, &str); 2] = [("embedded", "kernel.embedded"), ("mf", "kernel.mf")];

/// A slot is `abi::SLOT_SECTORS` (8 MiB) long.
const SLOT_BYTES: u64 = 16384 * 512;

pub fn run(host: &mut dyn Host, arguments: &str) {
    let (word, rest) = split_word(arguments);
    match word {
        "" | "list" => list(host),
        "use" => switch(host, rest),
        _ => sayln!(host, "Usage: kernel [list|use embedded|mf [force]]"),
    }
}

fn list(host: &mut dyn Host) {
    let running = host.kernel_flavour();
    if running.is_empty() {
        return sayln!(host, "kernel: this kernel cannot switch kernels.");
    }
    sayln!(host, "Running: the {running} kernel.");
    for (flavour, file) in KERNELS {
        let mark = if flavour == running { " (running)" } else { "" };
        match host.length(file) {
            Ok(length) => sayln!(host, "  {file}: {length} bytes{mark}"),
            Err(_) => sayln!(host, "  {file}: not stored{mark}"),
        }
    }
}

fn switch(host: &mut dyn Host, arguments: &str) {
    let (name, rest) = split_word(arguments);
    let Some((flavour, file)) = KERNELS.iter().copied().find(|(flavour, _)| *flavour == name) else {
        return sayln!(host, "Usage: kernel use embedded|mf [force]");
    };
    let forced = rest == "force";
    if !rest.is_empty() && !forced {
        return sayln!(host, "Usage: kernel use embedded|mf [force]");
    }
    let running = host.kernel_flavour();
    if running.is_empty() {
        return sayln!(host, "kernel: this kernel cannot switch kernels.");
    }
    if running == flavour && !forced {
        return sayln!(host, "kernel: already running the {flavour} kernel (`kernel use {flavour} force` writes it again).");
    }
    let length = match host.length(file) {
        Ok(length) if length > 0 && length <= SLOT_BYTES => length as usize,
        Ok(_) => return sayln!(host, "kernel: {file} is empty or bigger than a kernel slot (8 MiB)."),
        Err(_) => return sayln!(host, "kernel: {file} is not on the boot volume (put it there with `fosadm put`)."),
    };
    let mut outcome: Option<Result<u8, &'static str>> = None;
    let mut failure = None;
    let fits = host.with_buffer(length.div_ceil(512) * 512, &mut |host, image| {
        match host.read(file, image) {
            Ok(read) if read == length => outcome = Some(host.install_kernel(image, length, flavour)),
            Ok(_) => failure = Some("it was read only in part"),
            Err(_) => failure = Some("it could not be read"),
        }
    });
    if !fits {
        return sayln!(host, "kernel: no memory for {file}.");
    }
    if let Some(reason) = failure {
        return sayln!(host, "kernel: {file}: {reason}.");
    }
    match outcome {
        Some(Ok(slot)) => {
            sayln!(host, "kernel: {file} written to slot {slot} (pending); restarting to try it.");
            sayln!(host, "The loader falls back to this kernel if the new one does not come up.");
            host.restart();
        }
        Some(Err(message)) => sayln!(host, "kernel: {message}; nothing was written."),
        None => {}
    }
}
