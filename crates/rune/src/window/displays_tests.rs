use super::{Effect, Event, State, step};
use crate::machine::Effects;
use crate::windows::Display;

fn display(name: &str, x: i32, scale: f64) -> Display {
    Display { id: None, name: Some(name.to_owned()), position: (x, 0), size: (1920, 1080), scale, primary: false }
}

fn looked(displays: &[Display]) -> Event {
    Event::Looked { displays: displays.to_vec(), settled: true }
}

#[test]
fn the_first_look_tells_the_handlers_even_an_empty_list() {
    let mut state = State::default();
    assert_eq!(step(&mut state, looked(&[])), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(Vec::new()));

    let mut state = State::default();
    let one = [display("DP-1", 0, 1.0)];
    assert_eq!(step(&mut state, looked(&one)), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(one.to_vec()));
}

#[test]
fn an_added_display_is_told_once_the_outputs_changed() {
    let one = [display("DP-1", 0, 1.0)];
    let two = [display("DP-1", 0, 1.0), display("HDMI-A-1", 1920, 1.0)];
    let mut state = State::Current(one.to_vec());
    assert!(step(&mut state, Event::OutputsChanged).is_empty());
    assert_eq!(state, State::Stale(Some(one.to_vec())));
    assert_eq!(step(&mut state, looked(&two)), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(two.to_vec()));
}

#[test]
fn a_removed_display_is_told_down_to_none() {
    let two = [display("DP-1", 0, 1.0), display("HDMI-A-1", 1920, 1.0)];
    let mut state = State::Current(two.to_vec());
    let _ = step(&mut state, Event::OutputsChanged);
    assert_eq!(step(&mut state, looked(&two[..1])), Effects::one(Effect::Notify));
    let _ = step(&mut state, Event::OutputsChanged);
    assert_eq!(step(&mut state, looked(&[])), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(Vec::new()));
}

#[test]
fn a_changed_scale_or_position_is_told() {
    let before = [display("DP-1", 0, 1.0), display("DP-2", 1920, 1.0)];
    let scaled = [display("DP-1", 0, 1.5), display("DP-2", 1280, 1.0)];
    let mut state = State::Current(before.to_vec());
    let _ = step(&mut state, Event::OutputsChanged);
    assert_eq!(step(&mut state, looked(&scaled)), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(scaled.to_vec()));
}

#[test]
fn an_output_described_again_with_what_it_had_tells_nothing() {
    let one = [display("DP-1", 0, 1.0)];
    let mut state = State::Current(one.to_vec());
    let _ = step(&mut state, Event::OutputsChanged);
    assert!(step(&mut state, looked(&one)).is_empty());
    assert_eq!(state, State::Current(one.to_vec()));
}

#[test]
fn an_incomplete_list_waits_for_the_next_look() {
    let one = [display("DP-1", 0, 1.0)];
    let two = [display("DP-1", 0, 1.0), display("HDMI-A-1", 1920, 1.0)];

    // At launch: nothing is told until the list is whole.
    let mut state = State::default();
    assert!(step(&mut state, Event::Looked { displays: Vec::new(), settled: false }).is_empty());
    assert_eq!(state, State::Stale(None));
    assert_eq!(step(&mut state, looked(&one)), Effects::one(Effect::Notify));

    // A new output winit doesn't list yet: the handlers keep the old list, then hear the new.
    let _ = step(&mut state, Event::OutputsChanged);
    assert!(step(&mut state, Event::Looked { displays: one.to_vec(), settled: false }).is_empty());
    assert_eq!(state, State::Stale(Some(one.to_vec())));
    assert_eq!(step(&mut state, looked(&two)), Effects::one(Effect::Notify));
    assert_eq!(state, State::Current(two.to_vec()));
}

#[test]
fn changes_while_stale_fold_into_one_telling() {
    let one = [display("DP-1", 0, 1.0)];
    let mut state = State::Current(one.to_vec());
    // Unplugged and plugged back in between two looks.
    let _ = step(&mut state, Event::OutputsChanged);
    let _ = step(&mut state, Event::OutputsChanged);
    assert_eq!(state, State::Stale(Some(one.to_vec())));
    assert!(step(&mut state, looked(&one)).is_empty());
}

#[test]
fn a_look_without_a_change_tells_only_what_differs() {
    let one = [display("DP-1", 0, 1.0)];
    let mut state = State::Current(one.to_vec());
    assert!(step(&mut state, looked(&one)).is_empty());
    assert!(step(&mut state, Event::Looked { displays: Vec::new(), settled: false }).is_empty());
    assert_eq!(state, State::Current(one.to_vec()));
    assert_eq!(step(&mut state, looked(&[])), Effects::one(Effect::Notify));
}
