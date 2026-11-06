//! Shell sessions. Session 0 is the console (serial, PS/2, USB keyboard); sessions 1.. are network terminals (`term net`, see
//! `net.rs`; later SSH shell channels). Each has its own line editor and history, working folder, foreground tape (and its
//! typed bytes), `top` and script variables, so several people can use the machine at once.
//!
//! All shell commands still run on the service thread, one byte at a time. `with(id, f)` says which session the code in `f`
//! acts for; that is how `println!` knows where text goes (`net::relay`) and how the file calls find the working folder.
//! Code on another thread (a tape) belongs to the session that has it in the foreground, if any. Text with no session
//! (services, kernel messages) is shown on every open terminal, as the console always shows everything.

use crate::sync::SpinLock;

/// The console plus four network terminals.
pub const MAX: usize = kernel_core::lineedit::EDITORS;

/// (thread id, session) of the `with` calls in progress; nested calls on one thread are possible, so a small stack.
static ACTIVE: SpinLock<([(usize, usize); 8], usize)> = SpinLock::new(([(0, 0); 8], 0));

/// Runs `f` as session `id`.
pub fn with<R>(id: usize, f: impl FnOnce() -> R) -> R {
    let me = crate::sched::current_id();
    {
        let mut active = ACTIVE.lock();
        let top = active.1;
        if top < active.0.len() {
            active.0[top] = (me, id);
            active.1 += 1;
        }
    }
    let result = f();
    {
        let mut active = ACTIVE.lock();
        // The innermost entry of this thread (others may have pushed since: they pop before we do).
        if let Some(position) = active.0[..active.1].iter().rposition(|entry| entry.0 == me) {
            let length = active.1;
            active.0.copy_within(position + 1..length, position);
            active.1 -= 1;
        }
    }
    result
}

/// The session the calling code acts for: the one it runs `with`, else the one whose foreground tape it is.
pub fn current() -> Option<usize> {
    let me = crate::sched::current_id();
    {
        let active = ACTIVE.lock();
        if let Some(entry) = active.0[..active.1].iter().rev().find(|entry| entry.0 == me) {
            return Some(entry.1);
        }
    }
    let pid = crate::sched::current_pid();
    if pid == 0 {
        return None;
    }
    (0..MAX).find(|&id| crate::shell::foreground(id) == pid)
}

/// `current`, with the console for code that belongs to no session.
pub fn current_or_console() -> usize {
    current().unwrap_or(0)
}
