//! Locking the session on Wayland with `ext-session-lock-v1`, for a lock screen.
//!
//! [`AppHandle::lock_session`](crate::AppHandle::lock_session) asks the compositor to lock the
//! session and returns a [`SessionLock`]. Overlays made with
//! [`ConfiguredOverlay::session_lock`](crate::ConfiguredOverlay::session_lock) become its lock
//! surfaces, one per display: the compositor sizes each to its output, draws nothing else
//! while the session is locked and sends every key and press to them. Give every display one
//! (`App::on_displays_changed` hears displays added while locked) and show them at once; the
//! compositor may wait for them before it reports [`LockState::Locked`].
//!
//! The session stays locked until [`SessionLock::unlock`]. Nothing else unlocks it: dropping
//! every `SessionLock`, removing the overlays, a panic or the process ending leaves it locked,
//! as the protocol has it (Hyprland then shows its "lock dead" screen until a new lock client
//! takes over; see `misc:allow_session_lock_restore`).
//!
//! ```ignore
//! let lock = app.lock_session(move |state| events.send(Lock(state)).ok());
//! for display in displays {
//!     let surface = app.add_overlay(move || lock_spec(display_target).session_lock(&lock_for_spec), LockView::default());
//!     surface.show();
//! }
//! // … once the password checks out:
//! lock.unlock();
//! ```

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

mod machine;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod wire;

/// Where a session lock is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// Asked for; the compositor hasn't answered yet.
    Pending,
    /// The compositor locked the session: it shows only lock surfaces, and input reaches only
    /// them.
    Locked,
    /// [`SessionLock::unlock`] unlocked it, or gave it up before it was granted. Final.
    Unlocked,
    /// The compositor refused the lock (another client holds one, or a dead locker's lock still
    /// stands) or ended it itself. Final.
    Finished,
    /// Not on Wayland, or the compositor lacks `ext-session-lock-v1`. Final.
    Unsupported,
}

impl LockState {
    /// No longer asked for or held: a lock can't come back from here.
    pub fn ended(self) -> bool {
        matches!(self, LockState::Unlocked | LockState::Finished | LockState::Unsupported)
    }
}

type OnChange = Box<dyn Fn(LockState) + Send + Sync>;

/// What a lock's handles share with the event loop.
pub(crate) struct Shared {
    /// What `state` reads; the event loop writes it.
    public: Mutex<LockState>,
    /// `SessionLock::unlock` was called. Only the event loop steps the lock.
    unlock_asked: AtomicBool,
    on_change: OnChange,
    /// Wakes the event loop that owns the lock.
    wake: Box<dyn Fn() + Send + Sync>,
    /// The event loop's side: the lock's life and its Wayland objects.
    inner: Mutex<Inner>,
}

struct Inner {
    machine: machine::State,
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    wire: Option<wire::Wire>,
}

/// A session lock: what [`AppHandle::lock_session`](crate::AppHandle::lock_session) asked
/// for. Clones share it. Dropping them leaves the lock as it is.
#[derive(Clone)]
pub struct SessionLock {
    pub(crate) shared: Arc<Shared>,
}

impl fmt::Debug for SessionLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionLock").field("state", &self.state()).finish()
    }
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionLock {
    /// A lock asked for, not sent yet: the event loop sends it on its next turn. `wake` wakes
    /// that loop.
    pub(crate) fn asked(on_change: impl Fn(LockState) + Send + Sync + 'static, wake: impl Fn() + Send + Sync + 'static) -> SessionLock {
        let lock = SessionLock::detached(on_change, wake);
        guard(&LIVE).push(lock.shared.clone());
        (lock.shared.wake)();
        lock
    }

    /// A lock no event loop turns: `asked` registers it, and tests turn their own.
    fn detached(on_change: impl Fn(LockState) + Send + Sync + 'static, wake: impl Fn() + Send + Sync + 'static) -> SessionLock {
        let shared = Arc::new(Shared {
            public: Mutex::new(LockState::Pending),
            unlock_asked: AtomicBool::new(false),
            on_change: Box::new(on_change),
            wake: Box::new(wake),
            inner: Mutex::new(Inner {
                machine: machine::State::Asked,
                #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
                wire: None,
            }),
        });
        SessionLock { shared }
    }

    /// Where the lock is now.
    pub fn state(&self) -> LockState {
        *guard(&self.shared.public)
    }

    /// Unlocks the session: the only way it unlocks. Asked before the compositor has locked it,
    /// the unlock waits for `locked` (sending it earlier is a protocol error); asked before the
    /// lock was even sent, nothing is sent. Its lock surfaces close, and their overlays hide.
    /// Does nothing once the lock has ended.
    pub fn unlock(&self) {
        if self.state().ended() { return; }
        self.shared.unlock_asked.store(true, Ordering::Release);
        (self.shared.wake)();
    }

    /// Whether `other` is a handle of this same lock.
    pub fn same(&self, other: &SessionLock) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// Every lock still asked for or held, and those whose last request the compositor hasn't
/// read yet. The event loop turns them (`turn`); a lock that may hold the session is never
/// dropped from here.
static LIVE: Mutex<Vec<Arc<Shared>>> = Mutex::new(Vec::new());

/// What the event loop must know of the locks after a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Turned {
    /// A lock is asked for or held: winit must wake on every Wayland read.
    pub live: bool,
    /// Events already wait in a lock's queue: turn again at once.
    pub pending: bool,
}

/// Steps one lock through `events`, carrying out what it asks of the wire, and returns the
/// states its app must hear, in order.
fn step_all(inner: &mut Inner, shared: &Shared, events: impl IntoIterator<Item = machine::Event>) -> Vec<LockState> {
    let mut reports = Vec::new();
    for event in events {
        for effect in machine::step(&mut inner.machine, event) {
            match effect {
                machine::Effect::Report(state) => {
                    *guard(&shared.public) = state;
                    reports.push(state);
                }
                #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
                machine::Effect::Unlock => if let Some(wire) = &mut inner.wire { wire.unlock() },
                #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
                machine::Effect::Destroy => if let Some(wire) = &mut inner.wire { wire.destroy() },
                #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
                machine::Effect::Unlock | machine::Effect::Destroy => {}
            }
        }
    }
    reports
}

/// Sends a lock that was only asked for. On the event thread, from its turn or from a lock
/// surface made first.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn send(inner: &mut Inner, shared: &Shared, display: Option<std::ptr::NonNull<std::ffi::c_void>>) -> Vec<LockState> {
    if inner.machine != machine::State::Asked { return Vec::new(); }
    match display.and_then(wire::Wire::lock) {
        Some(wire) => {
            inner.wire = Some(wire);
            step_all(inner, shared, [machine::Event::Sent])
        }
        None => step_all(inner, shared, [machine::Event::Unsupported]),
    }
}

/// One turn of one lock: sends it if it was only asked for, carries out an unlock its app
/// asked for, hears the compositor, and tells the app. The second value: the lock is over and
/// its wire gone, so the loop may forget it.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn turn_lock(shared: &Shared, display: Option<std::ptr::NonNull<std::ffi::c_void>>) -> (Turned, bool) {
    let (turned, done, reports) = {
        let mut inner = guard(&shared.inner);
        let mut reports = Vec::new();
        if shared.unlock_asked.swap(false, Ordering::AcqRel) {
            reports.extend(step_all(&mut inner, shared, [machine::Event::UnlockAsked]));
        }
        reports.extend(send(&mut inner, shared, display));
        let heard = inner.wire.as_mut().map(wire::Wire::dispatch).unwrap_or_default();
        reports.extend(step_all(&mut inner, shared, heard));
        let live = inner.machine.live();
        let turned = Turned { live: live || inner.wire.is_some(), pending: inner.wire.as_ref().is_some_and(wire::Wire::has_pending_events) };
        // An ended lock's wire stays until the compositor has read its last request.
        let done = !live && inner.wire.as_ref().is_none_or(wire::Wire::closed);
        if done { inner.wire = None; }
        (turned, done, reports)
    };
    for state in reports { (shared.on_change)(state); }
    (turned, done)
}

/// One event-loop turn of every lock. `display` is winit's `wl_display`, `None` off Wayland.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
pub(crate) fn turn(display: Option<std::ptr::NonNull<std::ffi::c_void>>) -> Turned {
    let locks: Vec<Arc<Shared>> = guard(&LIVE).clone();
    let mut turned = Turned::default();
    let mut done = Vec::new();
    for shared in locks {
        let (one, over) = turn_lock(&shared, display);
        turned.live |= one.live;
        turned.pending |= one.pending;
        if over { done.push(shared); }
    }
    if !done.is_empty() {
        guard(&LIVE).retain(|shared| !done.iter().any(|gone| Arc::ptr_eq(gone, shared)));
    }
    turned
}

/// Off Wayland every lock is unsupported.
#[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
pub(crate) fn turn() -> Turned {
    let locks: Vec<Arc<Shared>> = std::mem::take(&mut *guard(&LIVE));
    for shared in locks {
        let reports = step_all(&mut guard(&shared.inner), &shared, [machine::Event::Unsupported]);
        for state in reports { (shared.on_change)(state); }
    }
    Turned::default()
}

/// The event loop's turn of every lock, on winit's display when it is Wayland.
pub(crate) fn turn_loop(event_loop: &winit::event_loop::ActiveEventLoop) -> Turned {
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    {
        use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        let display = match event_loop.display_handle().map(|handle| handle.as_raw()) {
            Ok(RawDisplayHandle::Wayland(wayland)) => Some(wayland.display),
            _ => None,
        };
        turn(display)
    }
    #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
    {
        let _ = event_loop;
        turn()
    }
}

/// The lock object a lock surface is asked for through, sending the lock first if the event
/// loop hasn't yet. None once the lock has ended or can't be sent.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
pub(crate) fn lock_object(lock: &SessionLock, display: std::ptr::NonNull<std::ffi::c_void>) -> Option<wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1::ExtSessionLockV1> {
    let shared = &lock.shared;
    let (object, reports) = {
        let mut inner = guard(&shared.inner);
        let reports = send(&mut inner, shared, Some(display));
        let object = inner.machine.live().then(|| inner.wire.as_ref().map(|wire| wire.lock_object().clone())).flatten();
        (object, reports)
    };
    for state in reports { (shared.on_change)(state); }
    object
}

#[cfg(all(test, unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod fake_compositor;

#[cfg(test)]
#[path = "session_lock_tests.rs"]
mod tests;
