//! A session lock's life, kept apart from the Wayland objects so its tests run headless: asked
//! for, sent to the compositor, locked, and ended by an unlock or by the compositor's `finished`.
//!
//! What may end a lock is narrow on purpose. Only `Event::UnlockAsked` (the app's explicit
//! [`SessionLock::unlock`](super::SessionLock::unlock)) unlocks a locked session; nothing else
//! steps a locked lock to `Unlocked`. A lock the app stops holding, or a process that dies,
//! never reaches this machine at all, so the compositor keeps the session locked, as the
//! protocol has it.

use super::LockState;
use crate::machine::Effects;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The app asked for a lock; the event loop has not sent it yet.
    Asked,
    /// The compositor has the request and has answered neither `locked` nor `finished`.
    /// `unlock`: unlock as soon as it locks, since an unlock before `locked` is a protocol error.
    Pending { unlock: bool },
    Locked,
    /// `unlock_and_destroy` sent, or the lock was given up before it was sent. Final.
    Unlocked,
    /// The compositor refused the lock or ended it (`finished`). Final.
    Finished,
    /// Not Wayland, or no `ext_session_lock_manager_v1`. Final.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// `ext_session_lock_manager_v1.lock` was sent.
    Sent,
    /// The compositor can't lock the session at all.
    Unsupported,
    /// `ext_session_lock_v1.locked`.
    Locked,
    /// `ext_session_lock_v1.finished`.
    Finished,
    /// The app called `SessionLock::unlock`.
    UnlockAsked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// `ext_session_lock_v1.unlock_and_destroy`: the session unlocks.
    Unlock,
    /// `ext_session_lock_v1.destroy`: the lock never locked, and goes.
    Destroy,
    /// The app hears the lock's new state.
    Report(LockState),
}

pub(crate) fn step(state: &mut State, event: Event) -> Effects<Effect> {
    match (*state, event) {
        (State::Unlocked | State::Finished | State::Unsupported, _) => Effects::none(),

        (State::Asked, Event::Sent) => {
            *state = State::Pending { unlock: false };
            Effects::none()
        }
        // Never sent: nothing to take back.
        (State::Asked, Event::UnlockAsked) => finish(state, State::Unlocked, None),
        (State::Asked, Event::Unsupported) => finish(state, State::Unsupported, None),
        (State::Asked, Event::Locked | Event::Finished) => Effects::none(),

        (State::Pending { unlock: false }, Event::Locked) => {
            *state = State::Locked;
            Effects::one(Effect::Report(LockState::Locked))
        }
        (State::Pending { unlock: true }, Event::Locked) => finish(state, State::Unlocked, Some(Effect::Unlock)),
        // Refused: `destroy` is the only way to let go of a lock that never locked.
        (State::Pending { .. }, Event::Finished) => finish(state, State::Finished, Some(Effect::Destroy)),
        (State::Pending { .. }, Event::UnlockAsked) => {
            *state = State::Pending { unlock: true };
            Effects::none()
        }
        (State::Pending { .. }, Event::Sent | Event::Unsupported) => Effects::none(),

        (State::Locked, Event::UnlockAsked) => finish(state, State::Unlocked, Some(Effect::Unlock)),
        // The compositor ended a lock it had granted: after `locked`, `destroy` is a protocol
        // error and `unlock_and_destroy` is how the object goes.
        (State::Locked, Event::Finished) => finish(state, State::Finished, Some(Effect::Unlock)),
        (State::Locked, Event::Sent | Event::Unsupported | Event::Locked) => Effects::none(),
    }
}

fn finish(state: &mut State, last: State, wire: Option<Effect>) -> Effects<Effect> {
    *state = last;
    let report = Effect::Report(last.public());
    match wire {
        Some(effect) => Effects::two(effect, report),
        None => Effects::one(report),
    }
}

impl State {
    /// What the app sees.
    pub(crate) fn public(self) -> LockState {
        match self {
            State::Asked | State::Pending { .. } => LockState::Pending,
            State::Locked => LockState::Locked,
            State::Unlocked => LockState::Unlocked,
            State::Finished => LockState::Finished,
            State::Unsupported => LockState::Unsupported,
        }
    }

    /// Still asked for or holding the session: the event loop keeps turning it.
    pub(crate) fn live(self) -> bool {
        matches!(self, State::Asked | State::Pending { .. } | State::Locked)
    }
}

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
