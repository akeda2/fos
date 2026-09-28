pub mod x86 {
    #[inline]
    pub unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        unsafe {
            core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline]
    pub unsafe fn outb(port: u16, value: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// 32-bit input, used for PCI configuration-space access (the
    /// `0xcf8`/`0xcfc` address/data port pair only accepts full
    /// double-word transfers).
    #[inline]
    pub unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        unsafe {
            core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// 32-bit output; see `inl`.
    #[inline]
    pub unsafe fn outl(port: u16, value: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// 16-bit input, used for the RTL8139 NIC's 16-bit registers (`CAPR`,
    /// `CBR`, `IMR`, `ISR`). Kept alongside `outw` for symmetry even
    /// though this driver only currently writes those registers.
    #[inline]
    #[allow(dead_code)]
    pub unsafe fn inw(port: u16) -> u16 {
        let value: u16;
        unsafe {
            core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// 16-bit output; see `inw`.
    #[inline]
    pub unsafe fn outw(port: u16, value: u16) {
        unsafe {
            core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// A short, port-I/O-based delay: a throwaway write to port `0x80`,
    /// conventionally unused (the original IBM PC used it for POST debug
    /// codes) but still wired to the ISA/LPC bus on essentially all real
    /// PC-compatible chipsets, so an `out` to it takes roughly the same
    /// bus cycle time as any other I/O port access. Some real hardware --
    /// unlike QEMU's emulated 8259, which accepts back-to-back writes with
    /// no delay -- needs a short gap between successive command bytes sent
    /// to slower legacy devices (the PIC being the classic example); this
    /// gives them one without needing a calibrated timer this early in
    /// boot, before the PIT has even been programmed.
    #[inline]
    pub unsafe fn io_wait() {
        unsafe { outb(0x80, 0) }
    }

    /// Reads a Model-Specific Register. `rdmsr`'s result is split across
    /// `edx:eax` (high:low 32 bits of the 64-bit value) by the instruction
    /// itself, not by any convention this code chooses -- this is how the
    /// local APIC's base address/enable bit (`IA32_APIC_BASE`, MSR `0x1b`)
    /// is read, the MSR equivalent of `inl`/`outl`'s port-I/O access.
    #[inline]
    pub unsafe fn rdmsr(msr: u32) -> u64 {
        let low: u32;
        let high: u32;
        unsafe {
            core::arch::asm!(
                "rdmsr",
                in("ecx") msr,
                out("eax") low,
                out("edx") high,
                options(nomem, nostack, preserves_flags)
            );
        }
        (u64::from(high) << 32) | u64::from(low)
    }

    /// Writes a Model-Specific Register; see `rdmsr`.
    #[inline]
    pub unsafe fn wrmsr(msr: u32, value: u64) {
        let low = value as u32;
        let high = (value >> 32) as u32;
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") msr,
                in("eax") low,
                in("edx") high,
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    pub fn halt_forever() -> ! {
        loop {
            unsafe {
                core::arch::asm!("cli; hlt", options(nomem, nostack));
            }
        }
    }

    /// Halts the CPU for exactly one `hlt` and returns once any interrupt
    /// wakes it -- unlike `halt_forever`, which `cli`s first and never
    /// returns. Meant to be called once per otherwise-idle iteration of a
    /// polling loop that has nothing to do right now, so the CPU can stop
    /// burning cycles/power/heat spinning on nothing until the next timer,
    /// keyboard, or other interrupt actually gives it something to check.
    ///
    /// A textbook idle loop pairs this with `sti` immediately beforehand
    /// (`sti; hlt` as one atomic sequence) to close the race where a wakeup
    /// becomes pending after the caller's last check but before `hlt`
    /// executes, which would otherwise sleep through it indefinitely. That
    /// full defense isn't needed here: every caller of this function reaches
    /// it with interrupts already unconditionally enabled (`sti` runs once,
    /// early, in `interrupts::initialize()`, and nothing on these paths ever
    /// `cli`s without a matching `sti` before returning), and the PIT
    /// (`interrupts::TIMER_VECTOR`, IRQ0) is never masked and fires at a
    /// fixed 100 Hz regardless of anything else. So even in the
    /// worst-case race -- a wakeup arrives and is "missed" right at this
    /// call's boundary -- the next PIT tick resumes execution within
    /// roughly 10 ms, not indefinitely. Callers should treat that ~10 ms as
    /// the real, bounded latency cost of using this function, not assume
    /// it wakes only for the specific event they were waiting on.
    #[inline]
    pub fn halt_until_interrupt() {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }

    /// Bound on the 8042 input-buffer-clear poll in [`reset_via_8042`]. A
    /// large, arbitrary spin count: cheap to make generous since normal
    /// hardware clears the bit almost immediately, and this path is never
    /// hit in a loop where the cost of a long bound matters.
    const INPUT_BUFFER_CLEAR_ATTEMPTS: u32 = 1_000_000;

    /// Pure register-free delay between reset methods in [`reset`] below
    /// -- can't be `interrupts::ticks()`-based, since `debug::
    /// handle_crash` can reach this function from a fault handler with
    /// interrupts already disabled, where ticks would never advance at
    /// all. Only needs to cover a real chipset's own settle time for a
    /// method that actually took effect (microseconds on real hardware,
    /// where `pause` is cheap) before falling through to try the next
    /// method; kept modest specifically because QEMU's TCG interpreter
    /// makes each `pause` far more expensive to emulate than any real
    /// CPU takes to execute one -- an earlier `50_000_000`-iteration
    /// version of this function was confirmed correct but cost this
    /// project's own `make test-debug-mode` tens of extra seconds per
    /// `reset()` call for no real benefit, real hardware never needing
    /// anywhere near that long to either reset or prove a method dead.
    fn spin_delay() {
        let mut attempts = 2_000_000u32;
        while attempts > 0 {
            attempts -= 1;
            core::hint::spin_loop();
        }
    }

    /// Asks the 8042 keyboard controller to pulse the CPU's own RESET
    /// line -- the classic real-mode/protected-mode-era way to reboot a
    /// PC-compatible without BIOS or ACPI help, still honored by QEMU's
    /// emulated 8042 and by many real chipsets for backward compatibility.
    /// `0x64` is the controller's status/command port (see `STATUS_PORT`
    /// in `keyboard.rs`); bit 1 of a status read means its input buffer
    /// still holds a byte the controller hasn't consumed yet, so this
    /// polls that clear first (the same convention `keyboard.rs`'s own
    /// status-port polling uses) before sending `0xfe` ("pulse output
    /// port bit 0", which is wired to RESET) to that same port as a
    /// command.
    ///
    /// **Confirmed real-hardware finding, on a Dell Precision 3620**
    /// (Skylake-era, Sunrise Point PCH -- the same test host used for
    /// `e1000e.rs`'s I219-LM bring-up): this whole method does nothing on
    /// this machine, and worse, the *whole function* can hang completely
    /// and indefinitely -- not merely fail to reset, but never return
    /// control at all, confirmed by isolating it via a temporary build
    /// that skipped straight to `reset_via_acpi_smi`, which actually
    /// reset the machine once this function was no longer in the way.
    /// The input-buffer-clear poll below used to be a bare unbounded
    /// `loop`, on the assumption every chipset eventually reports bit 1
    /// clear the way QEMU's emulated 8042 always has; bounding it
    /// (`INPUT_BUFFER_CLEAR_ATTEMPTS`) was **not** sufficient to fix the
    /// hang, because the hang is not actually in that software loop at
    /// all -- it is most likely a single `inb`/`outb` to port `0x64`
    /// itself blocking forever at the firmware level. Real business-class
    /// boards without physical PS/2 hardware frequently emulate the KBC
    /// purely for legacy USB-keyboard compatibility by trapping `0x60`/
    /// `0x64` accesses into an SMM handler; if that handler is ever
    /// wedged, the trapping instruction itself never completes, and no
    /// software-side attempt bound can detect or recover from that --
    /// the CPU simply never comes back to run the next instruction.
    /// This is why `reset` below now tries this method *last*, after
    /// every method known not to carry this risk: a hang here can still
    /// block whatever would have run after it, but at least nothing
    /// that already had a working chance to reset the machine is starved
    /// by it.
    fn reset_via_8042() {
        let mut attempts = INPUT_BUFFER_CLEAR_ATTEMPTS;
        loop {
            let status = unsafe { inb(0x64) };
            if status & 0x02 == 0 {
                break;
            }
            attempts -= 1;
            if attempts == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        unsafe { outb(0x64, 0xfe) };
    }

    /// The real ACPI reset mechanism on the same Dell Precision 3620 --
    /// this is not a generic algorithm, it is one specific machine's own
    /// `RESET_REG`/`RESET_VALUE`, read directly out of its FADT (the
    /// `FACP` ACPI table) via `cat /sys/firmware/acpi/tables/FACP | xxd`
    /// under its already-working Linux install, since this project has
    /// no ACPI table parser of its own to discover a given machine's real
    /// values (locating the RSDP, then the FADT, then decoding a Generic
    /// Address Structure). Both the 8042 pulse above and every `0xcf9`
    /// value tried below did *nothing* on this board -- not even a screen
    /// blank -- confirmed on real hardware even after adding the `0xcf9`
    /// `FULL_RST` bit and a universal, chipset-independent triple fault;
    /// meanwhile ordinary Linux `reboot`/`shutdown` on this exact same
    /// machine works flawlessly, which is the whole reason this FADT
    /// dump was worth pulling in the first place. Its `RESET_REG` is a
    /// System-I/O Generic Address Structure (address space `0x01`, 8-bit
    /// width/access) at port `0xb2` -- the classic "APM control"/SMI
    /// command port, not `0xcf9` at all -- and its `RESET_VALUE` is
    /// `0x73`: this board's BIOS (AMI, Dell OEM table ID `CBX3`) performs
    /// a reset entirely inside SMM, triggered by an SMI raised through
    /// that port/value pair, which is also exactly why the legacy KBC/
    /// `0xcf9` paths above are silently absorbed here -- this platform's
    /// chosen reset path bypasses them entirely rather than ever routing
    /// through either one. This is real, machine-specific data, not a
    /// general "every ACPI machine uses port 0xb2/value 0x73" rule --
    /// on different hardware this pair would need to come from that
    /// machine's own FADT the same way -- but writing an SMI command
    /// byte to port 0xb2 is itself a decades-old, effectively universal
    /// x86 PC convention (the same port real-mode APM BIOS calls always
    /// used), so trying it here costs nothing extra on hardware where it
    /// isn't this exact value.
    ///
    /// **The `RESET_VALUE` write alone was still not sufficient**,
    /// confirmed on the real machine. The same FADT dump also gives the
    /// `SMI_CMD` field (offset 0x30: also port `0xb2`, consistent) and,
    /// right next to it, `ACPI_ENABLE` = `0xd1` / `ACPI_DISABLE` = `0xd2`
    /// (offsets 0x34/0x35) -- the commands an OS sends once, early at
    /// boot, to switch the platform from legacy BIOS power management
    /// into ACPI mode (handing PM1 register control to the OS). Linux
    /// always does this switch during its own boot; this project has no
    /// ACPI support at all and never does. The working hypothesis: this
    /// board's SMM firmware only honors the `RESET_VALUE` command once
    /// that mode switch has actually happened, so `ACPI_ENABLE` is sent
    /// first here, immediately before the real reset command, purely to
    /// put the SMM handler into the state it expects -- there is no
    /// reason to enable ACPI mode any earlier or leave it enabled
    /// otherwise, since every other line of this kernel already assumes
    /// legacy (non-ACPI) power management throughout, and a working
    /// reset makes the distinction moot immediately afterward anyway.
    fn reset_via_acpi_smi() {
        const ACPI_SMI_CMD_PORT: u16 = 0xb2;
        const ACPI_ENABLE_VALUE: u8 = 0xd1;
        const ACPI_RESET_VALUE: u8 = 0x73;
        unsafe { outb(ACPI_SMI_CMD_PORT, ACPI_ENABLE_VALUE) };
        spin_delay();
        unsafe { outb(ACPI_SMI_CMD_PORT, ACPI_RESET_VALUE) };
        spin_delay();
    }

    /// The PCH/ICH "reset control register" (`RST_CNT`, port `0xcf9`).
    /// Preserves every other bit already in the register (a real BIOS/OS
    /// may have set reserved or platform-specific bits there) and only
    /// ever touches bits 1 (`SYS_RST`, select a hard reset rather than
    /// the weaker "soft" mode), 2 (`RST_CPU`, which self-clears and
    /// triggers a reset once written with `SYS_RST` already set), and 3
    /// (`FULL_RST`, see below).
    ///
    /// **Confirmed real-hardware finding, same Dell Precision 3620 as
    /// `reset_via_8042`:** the classic two-value sequence this function
    /// used to send alone -- `SYS_RST` armed, then `SYS_RST | RST_CPU`
    /// (`0x06`), the same value Linux's older `pci_reboot()` uses and
    /// what most reset-trick writeups quote -- had zero observable
    /// effect on this board (confirmed: the machine stayed completely
    /// frozen through this, the 8042 attempt before it, *and* the triple
    /// fault after it, i.e. all three legacy methods together did
    /// nothing). But ordinary Linux `reboot`/`shutdown` on this exact
    /// machine work flawlessly -- and modern Linux's default reboot path
    /// on an ACPI-capable system is neither the KBC trick nor this
    /// classic `0x06` PCI reset, it's the ACPI-defined reset register
    /// from the FADT's `RESET_REG`/`RESET_VALUE`, which for this PCH
    /// generation (and most Intel platforms since) is the *same* port
    /// `0xcf9` but with `FULL_RST` (bit 3, `0x08`) also set -- `0x0e`
    /// rather than `0x06`. Without `FULL_RST`, some modern PCHs reset
    /// only the CPU's own execution state without actually cycling the
    /// rest of the platform, which can be silently absorbed rather than
    /// producing a visible reset. This function now sends both values in
    /// sequence -- the classic `0x06` first (harmless on hardware that
    /// already handles it, costs one extra write elsewhere), then the
    /// modern `0x0e` -- rather than committing to only one, since this
    /// project has no ACPI FADT parsing to look up the *actual*
    /// platform-declared `RESET_REG`/`RESET_VALUE` and confirm which one
    /// a given board really wants.
    fn reset_via_cf9() {
        const RST_CNT_PORT: u16 = 0xcf9;
        const SYS_RST: u8 = 1 << 1;
        const RST_CPU: u8 = 1 << 2;
        const FULL_RST: u8 = 1 << 3;
        let existing = unsafe { inb(RST_CNT_PORT) } & !(SYS_RST | RST_CPU | FULL_RST);
        unsafe { outb(RST_CNT_PORT, existing | SYS_RST) };
        spin_delay();
        unsafe { outb(RST_CNT_PORT, existing | SYS_RST | RST_CPU) };
        spin_delay();
        unsafe { outb(RST_CNT_PORT, existing | SYS_RST | RST_CPU | FULL_RST) };
        spin_delay();
    }

    /// The universal, chipset-independent last resort: deliberately
    /// triple-faults the CPU. `lidt` loads a descriptor table with
    /// `limit = 0` (no valid entries at all, not even vector 8's own
    /// double-fault gate), then `int3` raises an exception the CPU can't
    /// dispatch -- there is no IDT entry for it. Per the x86
    /// architecture, failing to invoke a fault handler is itself a fault
    /// (`#GP`), which also can't be dispatched (same empty IDT), which is
    /// a double fault, which *also* can't be dispatched -- a second
    /// nested fault while already handling a double fault is defined to
    /// trigger a triple fault, and real x86 silicon responds to that by
    /// asserting its own reset line. This does not depend on any
    /// chipset's KBC or ACPI reset-control register being wired up at
    /// all, unlike both methods above, so it is the one guaranteed to
    /// work on any real x86 PC -- the same reasoning SeaBIOS/GRUB's own
    /// last-resort reboot paths rely on.
    /// Attempts the triple fault described above. Unlike the other
    /// `reset_via_*` attempts, callers must assume this one might itself
    /// never return control on truly exotic hardware (the same
    /// architectural guarantee that makes it "universal" also means
    /// there is nothing softer to fall back to if it somehow doesn't
    /// work) -- but every real x86 CPU this project has tested against,
    /// including the real hardware that needed the other methods added,
    /// honors it, so `reset` below still keeps trying further methods
    /// after this call as a purely defensive measure.
    fn reset_via_triple_fault() {
        #[repr(C, packed)]
        struct EmptyIdtr {
            limit: u16,
            base: u32,
        }
        let idtr = EmptyIdtr { limit: 0, base: 0 };
        unsafe {
            core::arch::asm!("lidt [{0}]", in(reg) &idtr, options(nostack));
            core::arch::asm!("int3", options(nomem, nostack));
        }
    }

    /// Resets the machine by trying, in order, every method this project
    /// knows of, since real hardware isn't consistent about which one
    /// actually works and none of these methods can report back "that
    /// didn't work" for this function to act on -- a working reset
    /// simply never returns control here.
    ///
    /// **Order is safety-critical, not just a preference, following a
    /// real confirmed finding on a Dell Precision 3620** (see
    /// `reset_via_8042`'s and `reset_via_acpi_smi`'s doc comments): that
    /// machine's `reset_via_8042` doesn't just fail to reset, it can hang
    /// the CPU completely and indefinitely with no software-detectable
    /// bound, because the hang is most likely inside a single trapped
    /// `inb`/`outb` instruction itself, not in this project's own polling
    /// loop around it. Putting it first, as an earlier version of this
    /// function did, meant a hang there permanently starved every method
    /// after it -- confirmed by isolating the hang via a temporary build
    /// that skipped `reset_via_8042` entirely and successfully reset the
    /// same machine through `reset_via_acpi_smi`. So `reset_via_8042` now
    /// goes *last*: this machine's own real ACPI SMI reset first (see
    /// that function's doc comment for why it's safe to try even on
    /// hardware where it isn't the right value), then the PCH reset-
    /// control register (present on any ICH/PCH-based real machine),
    /// then the universal triple fault (the strongest guarantee of the
    /// four, and the one actually confirmed to work end-to-end on real
    /// hardware once 8042 was no longer blocking it), and only then the
    /// legacy 8042 pulse -- still worth trying on older/simpler real
    /// hardware that might have no ACPI reset register and a KBC that
    /// genuinely is wired to RESET, but never again first. `spin_delay`
    /// between each gives a real reset every chance to actually take
    /// effect before assuming it didn't and trying the next one.
    pub fn reset() -> ! {
        reset_via_acpi_smi();
        reset_via_cf9();
        reset_via_triple_fault();
        spin_delay();
        reset_via_8042();
        // Only reachable if every method above somehow didn't reset the
        // machine -- fail safe by halting rather than falling through to
        // undefined state.
        halt_forever()
    }
}
