use std::time::Duration;

use super::{Effect, Event, RETRY, State, step};
use crate::machine::Seq;
use crate::time::{self, Instant};

const FPS4: Duration = Duration::from_millis(250);

fn run(seq: &mut Seq, state: &mut State, interval: Duration, event: Event) -> Vec<Effect> {
    step(seq, state, interval, event).into_iter().collect()
}

fn ms(t0: Instant, ms: u64) -> Instant {
    t0 + Duration::from_millis(ms)
}

#[test]
fn a_stream_asks_for_nothing_until_it_is_shown_and_then_asks_at_once() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Hide), []);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: t0 }), [Effect::Capture]);
    assert_eq!(state, State::Capturing);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: t0 }), [], "already asking");
}

#[test]
fn frames_come_no_faster_than_the_cap_however_often_the_content_changes() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    // The compositor answers 10 ms in: the next ask waits a full interval from that answer.
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Ready { now: ms(t0, 10) }), [Effect::WakeAt(ms(t0, 260))]);
    let (attempt, due) = state.due().unwrap();
    assert_eq!(due, ms(t0, 260));
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Due { attempt, now: ms(t0, 200) }), [], "an early wake changes nothing");
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Due { attempt, now: ms(t0, 260) }), [Effect::Capture]);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Due { attempt, now: ms(t0, 270) }), [], "a hold is answered once");
}

#[test]
fn a_still_source_costs_one_outstanding_ask_and_nothing_more() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    run(&mut seq, &mut state, FPS4, Event::Ready { now: t0 });
    let (attempt, _) = state.due().unwrap();
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Due { attempt, now: ms(t0, 250) }), [Effect::Capture]);
    // Nothing changes on screen: the compositor holds the ask, and the stream waits with it,
    // with no timer.
    assert_eq!(state, State::Capturing);
    assert_eq!(state.due(), None);
}

#[test]
fn hiding_gives_up_the_frame_asked_for_and_a_late_answer_changes_nothing() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Hide), [Effect::Cancel]);
    assert_eq!(state, State::Paused);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Ready { now: ms(t0, 5) }), []);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Failed { now: ms(t0, 5) }), []);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: ms(t0, 9) }), [Effect::Capture], "no frame came, so none caps the next");
}

#[test]
fn showing_again_soon_after_a_frame_still_keeps_the_cap() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    run(&mut seq, &mut state, FPS4, Event::Ready { now: ms(t0, 100) });
    let (held, _) = state.due().unwrap();
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Hide), []);
    assert_eq!(state, State::PausedAfter { last: ms(t0, 100) });
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Due { attempt: held, now: ms(t0, 350) }), [], "the hidden stream's timer is stale");
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: ms(t0, 150) }), [Effect::WakeAt(ms(t0, 350))]);
    assert_ne!(state.due().unwrap().0, held, "a new hold, so the old timer stays stale");
    run(&mut seq, &mut state, FPS4, Event::Hide);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: ms(t0, 900) }), [Effect::Capture], "long after: at once");
}

#[test]
fn a_new_cap_moves_the_held_ask() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    run(&mut seq, &mut state, FPS4, Event::Ready { now: t0 });
    let fps10 = Duration::from_millis(100);
    assert_eq!(run(&mut seq, &mut state, fps10, Event::Retimed { now: ms(t0, 20) }), [Effect::WakeAt(ms(t0, 100))]);
    assert_eq!(run(&mut seq, &mut state, fps10, Event::Retimed { now: ms(t0, 120) }), [Effect::Capture], "already past the new cap");
    assert_eq!(run(&mut seq, &mut state, fps10, Event::Retimed { now: ms(t0, 130) }), [], "nothing held to move");
}

#[test]
fn without_a_cap_each_frame_asks_for_the_next_at_once() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, Duration::ZERO, Event::Show { now: t0 });
    assert_eq!(run(&mut seq, &mut state, Duration::ZERO, Event::Ready { now: t0 }), [Effect::Capture]);
}

#[test]
fn a_failed_frame_is_asked_for_again_after_a_pause() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Failed { now: t0 }), [Effect::WakeAt(t0 + RETRY)], "at least RETRY, not the cap's 250 ms");
}

#[test]
fn a_stopped_session_ends_the_stream_for_good() {
    let (mut seq, t0) = (Seq::default(), time::now());
    let mut state = State::default();
    run(&mut seq, &mut state, FPS4, Event::Show { now: t0 });
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Stopped), [Effect::Cancel, Effect::End]);
    assert_eq!(state, State::Ended);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Show { now: t0 }), []);
    assert_eq!(run(&mut seq, &mut state, FPS4, Event::Stopped), []);

    let mut paused = State::default();
    assert_eq!(run(&mut seq, &mut paused, FPS4, Event::Stopped), [Effect::End], "nothing to give up");
}
