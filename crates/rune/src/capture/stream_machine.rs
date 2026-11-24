//! When a continuous capture asks the compositor for its next frame. Asked, the compositor
//! answers once the source's content changes (damage), so a still window costs nothing; the
//! answer then holds the next ask back until the frame-rate cap's interval has passed since it
//! came. A stream that isn't shown asks for nothing, and the frame it had asked for is given up.

use std::time::Duration;

use crate::machine::{Attempt, Effects, Seq};
use crate::time::Instant;

/// How long a failed capture waits before it is asked for again, at least.
pub(crate) const RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum State {
    /// Not shown, and no frame came yet: nothing is asked of the compositor.
    #[default]
    Paused,
    /// Not shown since a frame came at `last`.
    PausedAfter { last: Instant },
    /// Shown, with the next ask held back until `due` (`from` + the interval) by the cap.
    Holding { attempt: Attempt, from: Instant, due: Instant },
    /// A frame is asked for; the compositor sends it when the content changes.
    Capturing,
    /// The source is gone, or the compositor ended the session. Final.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The stream is shown at `now`.
    Show { now: Instant },
    Hide,
    /// The interval changed at `now`: a held ask moves with it.
    Retimed { now: Instant },
    /// The hold of `attempt` may be over.
    Due { attempt: Attempt, now: Instant },
    /// The frame asked for came at `now`.
    Ready { now: Instant },
    /// The frame asked for failed at `now`; the session lives on (a resize, a passing error).
    Failed { now: Instant },
    /// The session ended.
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Ask the compositor for a frame.
    Capture,
    /// Give up the frame asked for.
    Cancel,
    /// Wake at this time, to feed `Event::Due` back.
    WakeAt(Instant),
    /// Say the stream ended.
    End,
}

/// One transition; `interval` is the frame-rate cap's (zero: no cap).
pub(crate) fn step(seq: &mut Seq, state: &mut State, interval: Duration, event: Event) -> Effects<Effect> {
    match (*state, event) {
        (State::Ended, _) => Effects::none(),
        (State::Capturing, Event::Stopped) => {
            *state = State::Ended;
            Effects::two(Effect::Cancel, Effect::End)
        }
        (_, Event::Stopped) => {
            *state = State::Ended;
            Effects::one(Effect::End)
        }

        (State::Paused, Event::Show { .. }) => capture(state),
        (State::PausedAfter { last }, Event::Show { now }) => hold(seq, state, last, last + interval, now),
        (State::Holding { .. } | State::Capturing, Event::Show { .. }) => Effects::none(),

        (State::Holding { from, .. }, Event::Hide) => {
            *state = State::PausedAfter { last: from };
            Effects::none()
        }
        (State::Capturing, Event::Hide) => {
            *state = State::Paused;
            Effects::one(Effect::Cancel)
        }
        (State::Paused | State::PausedAfter { .. }, Event::Hide) => Effects::none(),

        (State::Holding { from, .. }, Event::Retimed { now }) => hold(seq, state, from, from + interval, now),
        (_, Event::Retimed { .. }) => Effects::none(),

        (State::Holding { attempt, due, .. }, Event::Due { attempt: answered, now }) if attempt == answered && now >= due => capture(state),
        (_, Event::Due { .. }) => Effects::none(),

        (State::Capturing, Event::Ready { now }) => hold(seq, state, now, now + interval, now),
        (State::Capturing, Event::Failed { now }) => hold(seq, state, now, now + interval.max(RETRY), now),
        // Answers to a frame given up.
        (_, Event::Ready { .. } | Event::Failed { .. }) => Effects::none(),
    }
}

fn capture(state: &mut State) -> Effects<Effect> {
    *state = State::Capturing;
    Effects::one(Effect::Capture)
}

/// The next ask waits until `due`, counted from `from`, unless that has passed by `now`.
fn hold(seq: &mut Seq, state: &mut State, from: Instant, due: Instant, now: Instant) -> Effects<Effect> {
    if now >= due {
        return capture(state);
    }
    *state = State::Holding { attempt: seq.next(), from, due };
    Effects::one(Effect::WakeAt(due))
}

impl State {
    /// When this state's hold ends.
    pub(crate) fn due(self) -> Option<(Attempt, Instant)> {
        match self {
            State::Holding { attempt, due, .. } => Some((attempt, due)),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "stream_machine_tests.rs"]
mod tests;
