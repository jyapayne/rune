//! The socket watch thread an XWayland overlay wakes the event loop with.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::super::layer_shell_machine::connection;
use crate::runtime::Waker;

/// Wakes the event loop when an XWayland overlay's own X connection becomes readable.
/// Wayland layer surfaces wake through winit's `set_wake_on_wayland_read` instead of a
/// background reader thread.
pub(crate) struct Watch {
    stop: Arc<AtomicBool>,
    /// How many times the overlay has dispatched, so the watcher waits for the loop to read
    /// what woke it instead of spinning on a socket that stays readable until then.
    turns: Arc<(Mutex<u64>, Condvar)>,
}

impl Watch {
    #[allow(unsafe_code)]
    pub(crate) fn spawn(fd: RawFd, wake: Waker) -> Watch {
        let stop = Arc::new(AtomicBool::new(false));
        let turns = Arc::new((Mutex::new(0u64), Condvar::new()));
        let (stopped, seen) = (stop.clone(), turns.clone());
        std::thread::Builder::new().name("rune-layer-watch".into()).spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: one pollfd for a display socket. winit's outlives the event loop; an
                // XWayland surface drops its watch before closing its connection, so at worst a
                // reused descriptor wakes the loop once more before this thread stops.
                let ret = unsafe { libc::poll(&mut poll, 1, 250) };
                let result = if ret < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        connection::WatchPoll::Interrupted
                    } else {
                        connection::WatchPoll::Error
                    }
                } else if ret == 0 {
                    connection::WatchPoll::TimedOut
                } else {
                    connection::WatchPoll::Ready {
                        readable: poll.revents & libc::POLLIN != 0,
                        hangup_or_error: poll.revents & (libc::POLLHUP | libc::POLLERR) != 0,
                        invalid: poll.revents & libc::POLLNVAL != 0,
                    }
                };
                match connection::watch_action(result) {
                    connection::WatchAction::Continue => {}
                    connection::WatchAction::Stop => break,
                    connection::WatchAction::WakeAndStop => {
                        if !stopped.load(Ordering::Acquire) { wake(); }
                        break;
                    }
                    connection::WatchAction::Wake => {
                        let (count, turned) = &*seen;
                        let before = *count.lock().unwrap_or_else(|e| e.into_inner());
                        wake();
                        turned.wait_timeout_while(
                            count.lock().unwrap_or_else(|e| e.into_inner()),
                            Duration::from_millis(100),
                            |n| *n == before && !stopped.load(Ordering::Acquire),
                        ).ok();
                    }
                }
            }
        }).ok();
        Watch { stop, turns }
    }

    pub(crate) fn turned(&self) {
        let (count, turned) = &*self.turns;
        *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        turned.notify_all();
    }
}

impl Drop for Watch {
    /// The thread notices immediately when parked, or within one poll timeout; joining would
    /// stall hiding.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let (count, turned) = &*self.turns;
        let _guard = count.lock().unwrap_or_else(|e| e.into_inner());
        turned.notify_all();
    }
}

