//! Noticing an interrupt while `rayline` holds something it must undo first.
//!
//! [`Watch`] catches Ctrl-C (and on Unix SIGTERM and SIGHUP) only while it
//! lives: the caller polls [`Watch::caught`], undoes what it started, then
//! calls [`Watch::finish`] to end the process as the interrupt would have.
//! Dropping it restores how this process handled those signals before.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

/// The interrupt caught while a [`Watch`] lives, else 0.
static CAUGHT: AtomicI32 = AtomicI32::new(0);

pub(crate) struct Watch {
    #[cfg(unix)]
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

impl Watch {
    /// Start catching interrupts. On Unix a signal this process ignores (a
    /// `nohup` hangup, Ctrl-C in a background job) stays ignored.
    pub(crate) fn start() -> io::Result<Self> {
        CAUGHT.store(0, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let mut watch = Self { saved: Vec::new() };
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                // SAFETY: zeroed is a valid `sigaction` for sigaction(2) to
                // fill in.
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                // SAFETY: a null new action only reads the current one into
                // `old`, a valid out pointer.
                if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                // SAFETY: as above.
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                // SAFETY: `action.sa_mask` is a valid sigset to empty.
                unsafe { libc::sigemptyset(&mut action.sa_mask) };
                // SAFETY: `action` is fully initialized and `on_signal` only
                // stores to an atomic, which is async-signal-safe.
                if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                watch.saved.push((signal, old));
            }
            Ok(watch)
        }
        #[cfg(windows)]
        {
            // SAFETY: registers a handler that only stores to an atomic.
            if unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {})
        }
    }

    /// Whether an interrupt has arrived since [`Watch::start`].
    pub(crate) fn caught(&self) -> bool {
        CAUGHT.load(Ordering::SeqCst) != 0
    }

    /// End the process as the caught interrupt would have ended it without
    /// the watch: on Unix the signal is raised again with the earlier
    /// handling restored; on Windows the exit status is Ctrl-C's.
    pub(crate) fn finish(self) -> ! {
        let caught = CAUGHT.load(Ordering::SeqCst);
        drop(self);
        #[cfg(unix)]
        {
            // SAFETY: raise(3) takes a signal number; the earlier handling
            // is back in place, so it acts as an interrupt without the watch.
            unsafe { libc::raise(caught) };
            std::process::exit(128 + caught)
        }
        #[cfg(windows)]
        {
            let _ = caught;
            // STATUS_CONTROL_C_EXIT, the status of a process Ctrl-C ends.
            std::process::exit(0xC000_013A_u32 as i32)
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (signal, old) in &self.saved {
            // SAFETY: restores the action sigaction(2) reported for `signal`.
            unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
        }
        #[cfg(windows)]
        // SAFETY: removes the handler `start` registered.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 0);
        }
    }
}

#[cfg(unix)]
extern "C" fn on_signal(signal: libc::c_int) {
    CAUGHT.store(signal, Ordering::SeqCst);
}

#[cfg(windows)]
unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        CAUGHT.store(1, Ordering::SeqCst);
        1
    } else {
        0
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::Watch;

    #[test]
    fn a_signal_is_caught_while_watched_and_handled_as_before_after() {
        // SIGTERM is ignored first: the watch must leave it ignored.
        // SAFETY: sets SIGTERM's action to ignore.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        let watch = Watch::start().unwrap();
        assert!(!watch.caught());
        // SAFETY: raise(3) with a signal this test handles.
        unsafe { libc::raise(libc::SIGINT) };
        assert!(watch.caught());
        // An ignored signal stays ignored: no handler was installed for it.
        // SAFETY: zeroed is a valid out value for sigaction(2).
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: reads SIGTERM's action into `current`.
        unsafe { libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut current) };
        assert_eq!(current.sa_sigaction, libc::SIG_IGN);
        drop(watch);
        // SAFETY: reads SIGINT's action into `current`.
        unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) };
        assert_eq!(current.sa_sigaction, libc::SIG_DFL);
        // SAFETY: restores SIGTERM's default action.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) };
    }
}
