//! Remembering that the operator pressed Escape. The kernel reports Escape once, to whoever asks first, so the
//! loops (which return to the caller when they stop) cannot tell the supervisor in `tape_main` whether they stopped
//! because of Escape or because of an error that is worth retrying. The loops get a copy of the tape API whose
//! `should_abort` is sticky: the first nonzero answer is kept, and every later call repeats it.

use core::mem::MaybeUninit;
use crate::TapeApi;

static mut ABORTED: bool = false;
static mut REAL: Option<&'static TapeApi> = None;
static mut COPY: MaybeUninit<TapeApi> = MaybeUninit::uninit();

extern "C" fn sticky_should_abort() -> u32 {
    unsafe {
        if ABORTED {
            return 1;
        }
        let status = (REAL.unwrap().should_abort)();
        if status != 0 {
            ABORTED = true;
        }
        status
    }
}

/// A copy of `api` with the sticky `should_abort`.
pub fn wrap(api: &'static TapeApi) -> &'static TapeApi {
    unsafe {
        REAL = Some(api);
        ABORTED = false;
        core::ptr::copy_nonoverlapping(api as *const TapeApi, (&raw mut COPY).cast::<TapeApi>(), 1);
        let copy = &mut *(&raw mut COPY).cast::<TapeApi>();
        copy.should_abort = sticky_should_abort;
        &*(&raw const COPY).cast::<TapeApi>()
    }
}

static mut COPY_CUSTOM: MaybeUninit<TapeApi> = MaybeUninit::uninit();

/// A copy of `api` whose `should_abort` is `hook`: the panel uses it to poll keys and redraw while a stream runs.
pub fn wrap_with(api: &'static TapeApi, hook: extern "C" fn() -> u32) -> &'static TapeApi {
    unsafe {
        core::ptr::copy_nonoverlapping(api as *const TapeApi, (&raw mut COPY_CUSTOM).cast::<TapeApi>(), 1);
        let copy = &mut *(&raw mut COPY_CUSTOM).cast::<TapeApi>();
        copy.should_abort = hook;
        &*(&raw const COPY_CUSTOM).cast::<TapeApi>()
    }
}

pub fn aborted() -> bool {
    unsafe { ABORTED }
}
