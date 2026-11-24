//! A layer surface's life with the compositor, kept out of `platform/layer_shell.rs` so its
//! tests run headless on every host: the commit that asks for a configure, the configure that
//! makes it drawable, the buffer that maps it, the null buffer that unmaps it for the next
//! show, and the close that ends it.
//!
//! Nothing draws before a configure is acked, and nothing waits for one on the UI thread: the
//! wait is a state with a deadline, and its timeout an event. A hide of a mapped surface
//! attaches a null buffer, which puts the layer surface back in the state it was made in; the
//! next show commits its role state again and waits for a new configure.

use std::time::Duration;

use crate::machine::{Attempt, Effects, Seq};
use crate::time::Instant;

/// How long a surface waits for the compositor's `configure` before giving up, so a silent
/// compositor leaves an overlay that says why rather than one that never shows.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum State {
    /// Holds no buffer and has asked for no configure: just made, or reset by an unmap.
    #[default]
    Unmapped,
    /// A null buffer was committed. `fence` is the `wl_display.sync` sent behind it: a configure
    /// read before its `done` left the compositor before the unmap reached it, and acking that
    /// after the reset is a protocol error. `remap`: a show waits for the fence to commit the
    /// role state again.
    Unmapping { fence: Attempt, remap: bool },
    /// The role state was committed without a buffer under `attempt`. Nothing draws until the
    /// compositor configures the surface, which it has until `due` to do.
    AwaitingConfigure { attempt: Attempt, due: Instant },
    /// Configured, so frames may draw, with no buffer attached yet: the compositor shows
    /// nothing. `size` is what it asked for that `dispatch` hasn't read.
    Configured { size: Option<(u32, u32)> },
    /// A buffer is attached: the compositor shows the surface.
    Mapped { size: Option<(u32, u32)> },
    /// The compositor never configured it. Final.
    Unanswered,
    /// Destroyed by the compositor, or its connection died. Final.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The surface was made, or its overlay shows, at `now`: it needs a configure to draw.
    Map { now: Instant },
    /// Its overlay hides and keeps the surface.
    Unmap,
    /// `zwlr_layer_surface_v1.configure`; 0 on a side means the size asked for.
    Configure { serial: u32, width: u32, height: u32 },
    /// A frame presented: a buffer was committed.
    Presented,
    /// The `wl_display.sync` behind an unmap answered, at `now`.
    Fenced { fence: Attempt, now: Instant },
    /// The configure deadline of `attempt` may have passed.
    Timeout { attempt: Attempt, now: Instant },
    /// The compositor destroyed the surface, or the connection died.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Send the role state (size, anchor, margins, exclusive zone, keyboard interactivity),
    /// the input region and the buffer scale, and commit without a buffer.
    Commit,
    /// `zwlr_layer_surface_v1.ack_configure`.
    Ack(u32),
    /// The first buffer since the configure mapped the surface: guard it with a focus grab,
    /// where its overlay holds one.
    Mapped,
    /// Let go of what the mapped surface held (focus and its grab, IME, key repeat, the pointer
    /// over it, its frame callback), attach a null buffer, commit, and `wl_display.sync` with
    /// this fence.
    Unmap { fence: Attempt },
    /// Wake at the configure deadline, to feed `Event::Timeout` back.
    WakeAt(Instant),
    /// Say that the compositor never configured the surface.
    GaveUp,
}

pub(crate) fn step(seq: &mut Seq, state: &mut State, event: Event) -> Effects<Effect> {
    match (*state, event) {
        (State::Closed | State::Unanswered, _) => {}
        (_, Event::Closed) => *state = State::Closed,

        (State::Unmapped, Event::Map { now }) => return await_configure(seq, state, now),
        (State::Unmapping { fence, .. }, Event::Map { .. }) => *state = State::Unmapping { fence, remap: true },
        // Already waiting for a configure or holding one: the surface draws as soon as it can.
        (State::AwaitingConfigure { .. } | State::Configured { .. } | State::Mapped { .. }, Event::Map { .. }) => {}

        (State::Mapped { .. }, Event::Unmap) => {
            let fence = seq.next();
            *state = State::Unmapping { fence, remap: false };
            return Effects::one(Effect::Unmap { fence });
        }
        (State::Unmapping { fence, .. }, Event::Unmap) => *state = State::Unmapping { fence, remap: false },
        // No buffer to take off: a configure still on its way is acked when it comes, and the
        // surface waits configured for the next show.
        (State::Unmapped | State::AwaitingConfigure { .. } | State::Configured { .. }, Event::Unmap) => {}

        (State::Unmapping { fence, remap }, Event::Fenced { fence: answered, now }) if fence == answered => {
            if remap { return await_configure(seq, state, now); }
            *state = State::Unmapped;
        }
        (_, Event::Fenced { .. }) => {}

        (State::AwaitingConfigure { .. } | State::Configured { .. }, Event::Configure { serial, width, height }) => {
            *state = State::Configured { size: Some((width, height)) };
            return Effects::one(Effect::Ack(serial));
        }
        (State::Mapped { .. }, Event::Configure { serial, width, height }) => {
            *state = State::Mapped { size: Some((width, height)) };
            return Effects::one(Effect::Ack(serial));
        }
        // Reset by the unmap, the surface owes no ack until its next commit asks for one.
        (State::Unmapped | State::Unmapping { .. }, Event::Configure { .. }) => {}

        (State::Configured { size }, Event::Presented) => {
            *state = State::Mapped { size };
            return Effects::one(Effect::Mapped);
        }
        (_, Event::Presented) => {}

        (State::AwaitingConfigure { attempt, due }, Event::Timeout { attempt: fired, now }) if attempt == fired && now >= due => {
            *state = State::Unanswered;
            return Effects::one(Effect::GaveUp);
        }
        (_, Event::Timeout { .. }) => {}
    }
    Effects::none()
}

fn await_configure(seq: &mut Seq, state: &mut State, now: Instant) -> Effects<Effect> {
    let (attempt, due) = (seq.next(), now + TIMEOUT);
    *state = State::AwaitingConfigure { attempt, due };
    Effects::two(Effect::Commit, Effect::WakeAt(due))
}

/// Why a surface can't show again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gone {
    /// The compositor destroyed it, or its connection died.
    Closed,
    /// The compositor never configured it.
    Unanswered,
}

impl State {
    /// A configure is acked: a frame may attach a buffer.
    pub(crate) fn drawable(&self) -> bool { matches!(self, State::Configured { .. } | State::Mapped { .. }) }

    /// The compositor holds a commit of the role state, so a change to it is committed at once.
    /// Unmapped, a commit would ask for a configure nobody waits for; the next show sends the
    /// role state whole.
    pub(crate) fn committed(&self) -> bool {
        matches!(self, State::AwaitingConfigure { .. } | State::Configured { .. } | State::Mapped { .. })
    }

    /// Why the surface can't show again, once it can't.
    pub(crate) fn gone(&self) -> Option<Gone> {
        match self {
            State::Closed => Some(Gone::Closed),
            State::Unanswered => Some(Gone::Unanswered),
            _ => None,
        }
    }

    /// The attempt whose configure deadline is armed.
    pub(crate) fn attempt(&self) -> Option<Attempt> {
        match *self {
            State::AwaitingConfigure { attempt, .. } => Some(attempt),
            _ => None,
        }
    }

    /// When `step` next wants a `Timeout`.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match *self {
            State::AwaitingConfigure { due, .. } => Some(due),
            _ => None,
        }
    }

    /// Takes the newest size configured since the last read.
    pub(crate) fn take_size(&mut self) -> Option<(u32, u32)> {
        match self {
            State::Configured { size } | State::Mapped { size } => size.take(),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "mapping_tests.rs"]
mod tests;
