//! `audio-net duplex`: sending the capture side and playing a received stream in one tape, at once.
//!
//! The send and receive loops are written as ordinary blocking loops that call `tape_rt::idle()` when a pass found
//! nothing to do. Rather than rewrite them as state machines, duplex runs them as two cooperative tasks (a tiny
//! stack switch, `duplex_switch` below): the receive loop on a stack of its own, the send loop on the tape's. A task
//! gives way to the other only inside `idle()`, so there is no preemption and no locking; a pass that finds work
//! keeps the CPU until it runs dry. The loops see a copy of the tape API (`SHIM`) with three calls replaced:
//!
//! * `should_abort`: the first task to see Escape sets a flag, so both stop (the real call reports it only once);
//! * `audio_start`/`audio_stop`: the sender asks for capture, the receiver for playback, and the device must run
//!   both from one `audio_start`, so a second request restarts the streams with the union, and the streams stop
//!   only when both tasks have stopped.

use crate::{Options, Proto, opus, send, receive};
use core::arch::global_asm;
use core::mem::MaybeUninit;
use tape_rt::{TapeApi, println};

global_asm!(
    ".text",
    ".global duplex_switch",
    "duplex_switch:",
    // rdi: where to save the current stack pointer; rsi: the stack pointer to continue on.
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

unsafe extern "C" {
    fn duplex_switch(save: *mut usize, load: usize);
}

const STACK_BYTES: usize = 512 * 1024;

static mut MAIN_SP: usize = 0;
static mut TASK_SP: usize = 0;
/// True while the second task exists and has not finished.
static mut TASK_LIVE: bool = false;
static mut ON_TASK: bool = false;
/// Set by `go()`: from then on `idle()` switches tasks.
static mut SWITCHING: bool = false;
static mut BODY: Option<*mut dyn FnMut()> = None;
static mut ABORT: bool = false;
static mut SHIM: MaybeUninit<TapeApi> = MaybeUninit::uninit();
static mut REAL: Option<&'static TapeApi> = None;
static mut WANT_CAPTURE: bool = false;
static mut WANT_PLAYBACK: bool = false;
static mut RUNNING: bool = false;
static mut OPEN: i32 = 0;

fn real() -> &'static TapeApi {
    unsafe { REAL.unwrap() }
}

extern "C" fn shim_should_abort() -> u32 {
    unsafe {
        if ABORT {
            return 1;
        }
        let status = (real().should_abort)();
        if status != 0 {
            ABORT = true;
        }
        status
    }
}

extern "C" fn shim_audio_start(rate: u32, capture: u32, playback: u32) -> i32 {
    unsafe {
        WANT_CAPTURE |= capture != 0;
        WANT_PLAYBACK |= playback != 0;
        if RUNNING {
            (real().audio_stop)();
        }
        let status = (real().audio_start)(rate, u32::from(WANT_CAPTURE), u32::from(WANT_PLAYBACK));
        RUNNING = status == 0;
        if status == 0 {
            OPEN += 1;
        }
        status
    }
}

extern "C" fn shim_audio_stop() {
    unsafe {
        OPEN -= 1;
        if OPEN <= 0 && RUNNING {
            (real().audio_stop)();
            RUNNING = false;
        }
    }
}

/// Called after `idle()`: hands the CPU to the other task.
fn switch_tasks() {
    unsafe {
        if !SWITCHING || !TASK_LIVE {
            return;
        }
        if ON_TASK {
            ON_TASK = false;
            duplex_switch(&raw mut TASK_SP, MAIN_SP);
        } else {
            ON_TASK = true;
            duplex_switch(&raw mut MAIN_SP, TASK_SP);
        }
    }
}

/// The send loop calls this once it is set up (resolved its peer, started the device): from here on the receive task
/// may run. Before it, `idle()` must not switch: the sender's ARP resolution reads the NIC itself and would lose its
/// reply to the receive task.
pub fn go() {
    unsafe { SWITCHING = true };
}

extern "C" fn task_entry() -> ! {
    unsafe {
        if let Some(body) = BODY {
            (*body)();
        }
        TASK_LIVE = false;
        ON_TASK = false;
        duplex_switch(&raw mut TASK_SP, MAIN_SP);
    }
    loop {}
}

/// Runs the send loop and the receive loop together.
pub fn run(api: &'static TapeApi, options: &Options) {
    if options.opus != (options.proto == Proto::Rtp && options.opus) {
        return println!("audio-net: codec=opus needs proto=rtp.");
    }
    let mut sending = options.clone();
    sending.channels = options.in_channels;
    let mut receiving = options.clone();
    receiving.channels = options.out_channels;
    let stack_address = (api.alloc)(STACK_BYTES);
    if stack_address == 0 {
        return println!("audio-net duplex: out of memory.");
    }
    unsafe {
        REAL = Some(api);
        core::ptr::copy_nonoverlapping(api as *const TapeApi, (&raw mut SHIM).cast::<TapeApi>(), 1);
        let shim = &mut *(&raw mut SHIM).cast::<TapeApi>();
        shim.should_abort = shim_should_abort;
        shim.audio_start = shim_audio_start;
        shim.audio_stop = shim_audio_stop;
        ABORT = false;
        (WANT_CAPTURE, WANT_PLAYBACK, RUNNING, OPEN, SWITCHING, ON_TASK) = (false, false, false, 0, false, false);
    }
    let shim: &'static TapeApi = unsafe { &*(&raw const SHIM).cast::<TapeApi>() };
    let mut receive_body = || {
        if receiving.opus {
            opus::receive(shim, &receiving);
        } else {
            receive(shim, &receiving);
        }
    };
    let body: &mut dyn FnMut() = &mut receive_body;
    unsafe {
        // Initial stack of the receive task: six saved registers and a return address into `task_entry`.
        let top = (stack_address + STACK_BYTES) & !15;
        let initial = top - 64;
        let words = initial as *mut usize;
        for index in 0..6 {
            words.add(index).write(0);
        }
        words.add(6).write(task_entry as *const () as usize);
        TASK_SP = initial;
        BODY = Some(core::mem::transmute::<*mut dyn FnMut(), *mut (dyn FnMut() + 'static)>(body as *mut dyn FnMut()));
        TASK_LIVE = true;
        tape_rt::set_idle_hook(Some(switch_tasks));
    }
    if sending.opus {
        opus::send(shim, &sending);
    } else {
        send(shim, &sending);
    }
    // The sender is done (time, Escape or an error). If it never got going, the receive task never ran.
    unsafe {
        if !SWITCHING {
            TASK_LIVE = false;
        }
        while TASK_LIVE {
            (real().wait_for_event)();
            switch_tasks();
        }
        tape_rt::set_idle_hook(None);
    }
    (api.free)(stack_address, STACK_BYTES);
}
