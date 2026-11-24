use std::time::Duration;

use super::{Effect, Event, Gone, State, TIMEOUT, step};
use crate::machine::{Attempt, Seq};
use crate::time::Instant;

fn run(seq: &mut Seq, state: &mut State, event: Event) -> Vec<Effect> {
    step(seq, state, event).into_iter().collect()
}

fn configure(serial: u32) -> Event {
    Event::Configure { serial, width: 280, height: 140 }
}

/// A surface made and configured, then shown and drawn once.
fn mapped(seq: &mut Seq, now: Instant) -> State {
    let mut state = State::default();
    run(seq, &mut state, Event::Map { now });
    run(seq, &mut state, configure(1));
    run(seq, &mut state, Event::Presented);
    assert_eq!(state, State::Mapped { size: Some((280, 140)) });
    state
}

#[test]
fn a_surface_draws_only_once_its_configure_is_acked() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    let mut state = State::default();
    assert!(!state.drawable() && !state.committed(), "nothing was committed yet");
    assert_eq!(run(&mut seq, &mut state, Event::Presented), [], "a frame can't map what isn't configured");
    assert_eq!(state, State::Unmapped);

    let due = now + TIMEOUT;
    assert_eq!(run(&mut seq, &mut state, Event::Map { now }), [Effect::Commit, Effect::WakeAt(due)]);
    assert!(!state.drawable(), "no frame before the configure");
    assert!(state.committed());
    assert_eq!(state.deadline(), Some(due));
    assert_eq!(run(&mut seq, &mut state, Event::Map { now }), [], "it is already asking");

    assert_eq!(run(&mut seq, &mut state, configure(7)), [Effect::Ack(7)]);
    assert!(state.drawable());
    assert_eq!(state.deadline(), None);
    assert_eq!(state.take_size(), Some((280, 140)));
    assert_eq!(state.take_size(), None, "a size is read once");

    assert_eq!(run(&mut seq, &mut state, Event::Presented), []);
    assert_eq!(state, State::Mapped { size: None });
    // The compositor resizes a mapped surface: acked, and the newest size nobody read wins.
    assert_eq!(run(&mut seq, &mut state, Event::Configure { serial: 8, width: 10, height: 20 }), [Effect::Ack(8)]);
    assert_eq!(run(&mut seq, &mut state, Event::Configure { serial: 9, width: 30, height: 40 }), [Effect::Ack(9)]);
    assert_eq!(state.take_size(), Some((30, 40)));
    assert!(state.drawable());
}

#[test]
fn hiding_a_mapped_surface_unmaps_it_and_the_next_show_waits_for_a_new_configure() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    let mut state = mapped(&mut seq, now);

    let effects = run(&mut seq, &mut state, Event::Unmap);
    let [Effect::Unmap { fence }] = effects[..] else { panic!("a mapped surface unmaps: {effects:?}") };
    assert!(!state.drawable() && !state.committed(), "the null buffer reset the role");
    assert_eq!(state.take_size(), None);
    assert_eq!(run(&mut seq, &mut state, Event::Unmap), [], "it unmaps once");

    // A configure the compositor sent before the unmap reached it: acking it after the reset
    // would be a protocol error.
    assert_eq!(run(&mut seq, &mut state, configure(2)), []);
    assert_eq!(run(&mut seq, &mut state, Event::Fenced { fence: Attempt(fence.0 + 9), now }), [], "not this unmap's fence");
    assert_eq!(state, State::Unmapping { fence, remap: false });
    assert_eq!(run(&mut seq, &mut state, Event::Fenced { fence, now }), []);
    assert_eq!(state, State::Unmapped);
    assert_eq!(run(&mut seq, &mut state, configure(3)), [], "unmapped, it owes no ack");
    assert_eq!(run(&mut seq, &mut state, Event::Fenced { fence, now }), [], "a fence answers once");

    let later = now + Duration::from_secs(5);
    assert_eq!(run(&mut seq, &mut state, Event::Map { now: later }), [Effect::Commit, Effect::WakeAt(later + TIMEOUT)]);
    assert!(!state.drawable(), "the remap draws nothing until its own configure");
    assert_eq!(run(&mut seq, &mut state, configure(4)), [Effect::Ack(4)]);
    assert!(state.drawable());
    assert_eq!(run(&mut seq, &mut state, Event::Presented), []);
    assert!(matches!(state, State::Mapped { .. }));
}

#[test]
fn a_show_during_an_unmap_commits_once_the_fence_answers() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    let mut state = mapped(&mut seq, now);
    let effects = run(&mut seq, &mut state, Event::Unmap);
    let [Effect::Unmap { fence }] = effects[..] else { panic!("{effects:?}") };

    assert_eq!(run(&mut seq, &mut state, Event::Map { now }), [], "the commit waits for the fence");
    assert_eq!(state, State::Unmapping { fence, remap: true });
    assert_eq!(run(&mut seq, &mut state, configure(2)), [], "still a configure from before the unmap");
    let answered = now + Duration::from_millis(3);
    assert_eq!(run(&mut seq, &mut state, Event::Fenced { fence, now: answered }), [Effect::Commit, Effect::WakeAt(answered + TIMEOUT)]);
    assert_eq!(run(&mut seq, &mut state, configure(3)), [Effect::Ack(3)]);
    assert!(state.drawable());

    // Hidden again before the fence answered, the show is forgotten.
    let mut state = mapped(&mut seq, now);
    let effects = run(&mut seq, &mut state, Event::Unmap);
    let [Effect::Unmap { fence }] = effects[..] else { panic!("{effects:?}") };
    run(&mut seq, &mut state, Event::Map { now });
    assert_eq!(run(&mut seq, &mut state, Event::Unmap), []);
    assert_eq!(run(&mut seq, &mut state, Event::Fenced { fence, now }), []);
    assert_eq!(state, State::Unmapped);
}

#[test]
fn a_surface_that_never_drew_stays_configured_through_a_hide() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    // Prewarmed: configured at startup, no buffer. There is nothing to unmap, and a null buffer
    // on a surface that never had one resets nothing, so the next show would wait for a
    // configure that never comes.
    let mut state = State::default();
    run(&mut seq, &mut state, Event::Map { now });
    run(&mut seq, &mut state, configure(1));
    assert_eq!(run(&mut seq, &mut state, Event::Unmap), []);
    assert!(state.drawable());
    assert_eq!(run(&mut seq, &mut state, Event::Map { now }), [], "the show draws at once");
    assert!(state.drawable());

    // Hidden while its configure is on the way: the configure is acked when it comes.
    let mut state = State::default();
    run(&mut seq, &mut state, Event::Map { now });
    assert_eq!(run(&mut seq, &mut state, Event::Unmap), []);
    assert_eq!(run(&mut seq, &mut state, configure(2)), [Effect::Ack(2)]);
    assert_eq!(state, State::Configured { size: Some((280, 140)) });
}

#[test]
fn the_configure_timeout_is_an_event_fenced_by_its_attempt() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    let mut state = State::default();
    run(&mut seq, &mut state, Event::Map { now });
    let attempt = state.attempt().expect("a configure deadline is armed");
    let due = state.deadline().unwrap();

    assert_eq!(run(&mut seq, &mut state, Event::Timeout { attempt, now: due - Duration::from_millis(1) }), [], "not due yet");
    assert_eq!(run(&mut seq, &mut state, Event::Timeout { attempt: Attempt(attempt.0 + 1), now: due }), [], "another wait's timer");
    assert_eq!(state.gone(), None);
    assert_eq!(run(&mut seq, &mut state, Event::Timeout { attempt, now: due }), [Effect::GaveUp]);
    assert_eq!(state.gone(), Some(Gone::Unanswered));
    assert!(!state.drawable() && state.deadline().is_none());

    // A configure that beat the deadline disarms it: the timer of that attempt changes nothing.
    let mut state = State::default();
    run(&mut seq, &mut state, Event::Map { now });
    let attempt = state.attempt().unwrap();
    run(&mut seq, &mut state, configure(1));
    assert_eq!(state.attempt(), None);
    assert_eq!(run(&mut seq, &mut state, Event::Timeout { attempt, now: now + TIMEOUT * 2 }), []);
    assert!(state.drawable());

    // A remap arms its own deadline; the first wait's timer is stale.
    let mut state = mapped(&mut seq, now);
    let first = seq.current();
    let effects = run(&mut seq, &mut state, Event::Unmap);
    let [Effect::Unmap { fence }] = effects[..] else { panic!("{effects:?}") };
    run(&mut seq, &mut state, Event::Fenced { fence, now });
    run(&mut seq, &mut state, Event::Map { now });
    assert_ne!(state.attempt(), Some(first));
    assert_eq!(run(&mut seq, &mut state, Event::Timeout { attempt: first, now: now + TIMEOUT * 2 }), []);
    assert_eq!(state.gone(), None);
}

#[test]
fn closed_and_unanswered_are_final() {
    let (mut seq, now) = (Seq::default(), Instant::now());
    let every = |attempt| [
        Event::Map { now }, Event::Unmap, configure(9), Event::Presented,
        Event::Fenced { fence: attempt, now }, Event::Timeout { attempt, now: now + TIMEOUT * 2 }, Event::Closed,
    ];
    for from in [State::Unmapped, mapped(&mut seq, now), State::Configured { size: None }, State::Unmapping { fence: Attempt(3), remap: true }] {
        let mut state = from;
        assert_eq!(run(&mut seq, &mut state, Event::Closed), [], "{from:?}");
        assert_eq!(state.gone(), Some(Gone::Closed));
        for event in every(Attempt(3)) {
            assert_eq!(run(&mut seq, &mut state, event), [], "{event:?} after close");
            assert_eq!(state, State::Closed);
        }
        assert!(!state.drawable() && !state.committed() && state.take_size().is_none());
    }
    let mut state = State::default();
    run(&mut seq, &mut state, Event::Map { now });
    let attempt = state.attempt().unwrap();
    run(&mut seq, &mut state, Event::Timeout { attempt, now: now + TIMEOUT });
    for event in every(attempt) {
        assert_eq!(run(&mut seq, &mut state, event), [], "{event:?} after giving up");
        assert_eq!(state, State::Unanswered);
    }
}

/// What a walk asks of the machine next; fences and attempts are read from its state.
#[derive(Clone, Copy, Debug)]
enum Ask { Map, Unmap, Configure, Present, Fence, Timeout, Close }

fn each_sequence<T: Copy>(alphabet: &[T], depth: usize, seq: &mut Vec<T>, each: &mut impl FnMut(&[T])) {
    each(seq);
    if seq.len() == depth { return; }
    for &symbol in alphabet {
        seq.push(symbol);
        each_sequence(alphabet, depth, seq, each);
        seq.pop();
    }
}

/// The compositor's side of the protocol, as wlroots keeps it: a commit without a buffer
/// initialises the role, a configure may be acked only while initialised, a buffer may be
/// attached only once one was acked, and a null buffer on a mapped surface resets it all.
#[derive(Default)]
struct Compositor { initialised: bool, acked: bool, buffer: bool }

#[test]
fn no_sequence_breaks_the_protocol() {
    let now = Instant::now();
    let alphabet = [Ask::Map, Ask::Unmap, Ask::Configure, Ask::Present, Ask::Fence, Ask::Timeout, Ask::Close];
    each_sequence(&alphabet, 7, &mut Vec::new(), &mut |asks| {
        let (mut seq, mut state, mut wire) = (Seq::default(), State::default(), Compositor::default());
        // The fence of an unmap whose `sync` hasn't answered: until then the compositor's
        // configures predate the reset.
        let mut fence = None;
        for (i, &ask) in asks.iter().enumerate() {
            let event = match ask {
                Ask::Map => Event::Map { now },
                Ask::Unmap => Event::Unmap,
                Ask::Configure => Event::Configure { serial: i as u32, width: 1, height: 1 },
                // The overlay presents only what `drawable` let it acquire.
                Ask::Present if state.drawable() => Event::Presented,
                Ask::Present => continue,
                Ask::Fence => match fence.take() {
                    Some(fence) => Event::Fenced { fence, now },
                    None => continue,
                },
                Ask::Timeout => match state.attempt() {
                    Some(attempt) => Event::Timeout { attempt, now: now + TIMEOUT },
                    None => continue,
                },
                Ask::Close => Event::Closed,
            };
            if event == Event::Presented {
                assert!(wire.acked, "a buffer before any ack in {asks:?}");
                wire.buffer = true;
            }
            for effect in run(&mut seq, &mut state, event) {
                match effect {
                    Effect::Commit => {
                        assert!(!wire.buffer && !wire.initialised, "a role commit on a live role in {asks:?}");
                        assert!(fence.is_none(), "a role commit before the unmap's fence in {asks:?}");
                        wire.initialised = true;
                    }
                    Effect::Ack(_) => {
                        assert!(wire.initialised, "an ack with no role commit in {asks:?}");
                        assert!(fence.is_none(), "an ack of a configure from before the unmap in {asks:?}");
                        wire.acked = true;
                    }
                    Effect::Unmap { fence: sent } => {
                        assert!(wire.buffer, "an unmap of a surface with no buffer in {asks:?}");
                        wire = Compositor::default();
                        fence = Some(sent);
                    }
                    Effect::WakeAt(due) => assert_eq!(state.deadline(), Some(due)),
                    Effect::GaveUp => assert_eq!(state.gone(), Some(Gone::Unanswered)),
                }
            }
            assert_eq!(state.drawable(), wire.acked && state.gone().is_none(), "drawable after {asks:?}");
            assert_eq!(state.committed(), wire.initialised && state.gone().is_none(), "committed after {asks:?}");
            assert_eq!(matches!(state, State::Mapped { .. }), wire.buffer && state.gone().is_none(), "mapped after {asks:?}");
        }
    });
}
