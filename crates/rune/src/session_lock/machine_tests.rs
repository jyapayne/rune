use super::*;

fn run(events: &[Event]) -> (State, Vec<Effect>) {
    let mut state = State::Asked;
    let mut effects = Vec::new();
    for event in events {
        effects.extend(step(&mut state, *event));
    }
    (state, effects)
}

#[test]
fn a_lock_the_compositor_grants_reports_locked_once() {
    let (state, effects) = run(&[Event::Sent, Event::Locked, Event::Locked]);
    assert_eq!(state, State::Locked);
    assert_eq!(effects, [Effect::Report(LockState::Locked)]);
    assert_eq!(state.public(), LockState::Locked);
    assert!(state.live());
}

#[test]
fn only_an_explicit_unlock_unlocks_a_locked_session() {
    // Nothing the compositor or the loop sends unlocks it.
    let (state, _) = run(&[Event::Sent, Event::Locked, Event::Sent, Event::Unsupported, Event::Locked]);
    assert_eq!(state, State::Locked);
    let (state, effects) = run(&[Event::Sent, Event::Locked, Event::UnlockAsked, Event::UnlockAsked]);
    assert_eq!(state, State::Unlocked);
    assert_eq!(effects, [Effect::Report(LockState::Locked), Effect::Unlock, Effect::Report(LockState::Unlocked)]);
    assert!(!state.live());
}

#[test]
fn an_unlock_before_locked_waits_for_it_since_unlocking_earlier_is_a_protocol_error() {
    let (state, effects) = run(&[Event::Sent, Event::UnlockAsked]);
    assert_eq!(state, State::Pending { unlock: true });
    assert!(effects.is_empty(), "nothing goes on the wire before locked");
    assert_eq!(state.public(), LockState::Pending);
    let (state, effects) = run(&[Event::Sent, Event::UnlockAsked, Event::Locked]);
    assert_eq!(state, State::Unlocked);
    assert_eq!(effects, [Effect::Unlock, Effect::Report(LockState::Unlocked)]);
}

#[test]
fn a_lock_given_up_before_it_was_sent_sends_nothing() {
    let (state, effects) = run(&[Event::UnlockAsked, Event::Sent, Event::Locked]);
    assert_eq!(state, State::Unlocked);
    assert_eq!(effects, [Effect::Report(LockState::Unlocked)]);
}

#[test]
fn a_refused_lock_is_destroyed_and_one_ended_after_locking_is_unlocked_and_destroyed() {
    let (state, effects) = run(&[Event::Sent, Event::Finished]);
    assert_eq!(state, State::Finished);
    assert_eq!(effects, [Effect::Destroy, Effect::Report(LockState::Finished)]);
    // Refused while an unlock waited: still `destroy`, never `unlock_and_destroy`.
    let (_, effects) = run(&[Event::Sent, Event::UnlockAsked, Event::Finished]);
    assert_eq!(effects, [Effect::Destroy, Effect::Report(LockState::Finished)]);
    let (state, effects) = run(&[Event::Sent, Event::Locked, Event::Finished, Event::UnlockAsked]);
    assert_eq!(state, State::Finished);
    assert_eq!(effects, [Effect::Report(LockState::Locked), Effect::Unlock, Effect::Report(LockState::Finished)]);
}

#[test]
fn without_the_protocol_the_lock_is_unsupported() {
    let (state, effects) = run(&[Event::Unsupported, Event::Sent, Event::Locked]);
    assert_eq!(state, State::Unsupported);
    assert_eq!(effects, [Effect::Report(LockState::Unsupported)]);
    assert!(state.public().ended());
}
