//! Tape ABI 37 thread calls for the kernels that have no threads (the embedded x86 and Pi 4 kernels run one tape on one thread
//! of control). `thread_spawn` and `thread_join` say so; a tape then serves from a single loop. The server kernel (FOS-mf) has
//! the real ones (`kernel-mf/src/tapeapi.rs`).

use fosfor_abi::tape::THREAD_UNSUPPORTED;

pub extern "C" fn api_thread_spawn(_entry: extern "C" fn(usize), _argument: usize, _stack_top: usize) -> isize {
    THREAD_UNSUPPORTED
}

pub extern "C" fn api_thread_join(_thread: isize) -> isize {
    THREAD_UNSUPPORTED
}

pub extern "C" fn api_futex_wait(_address: usize, _expected: u32) -> isize {
    THREAD_UNSUPPORTED
}

pub extern "C" fn api_futex_wake(_address: usize, _count: usize) -> isize {
    0
}
