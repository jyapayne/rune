//! Every short sequence through the layer surface's touch machine, since the compositor chooses
//! the order of a finger's down, motion, lift and cancel. It is checked against a transcription
//! of the code it replaced, so the conversion can't quietly change what an app sees. The
//! exclusive zone a placement asks for is checked here too.

use super::touch;

/// Every sequence of up to `depth` symbols, shortest first, through `each`.
fn each_sequence<T: Copy>(alphabet: &[T], depth: usize, seq: &mut Vec<T>, each: &mut impl FnMut(&[T])) {
    each(seq);
    if seq.len() == depth {
        return;
    }
    for &symbol in alphabet {
        seq.push(symbol);
        each_sequence(alphabet, depth, seq, each);
        seq.pop();
    }
}

/// The old `touch_id: Option<i32>` and the input it pushed, transcribed.
#[derive(Default)]
struct OldTouch {
    id: Option<i32>,
}

impl OldTouch {
    fn step(&mut self, event: touch::Event) -> Vec<touch::Effect> {
        use touch::Effect;
        match event {
            touch::Event::Down { id, x, y } if self.id.is_none() => {
                self.id = Some(id);
                vec![Effect::Move(x, y), Effect::Press]
            }
            touch::Event::Motion { id, x, y } if self.id == Some(id) => vec![Effect::Move(x, y)],
            touch::Event::Up { id } if self.id == Some(id) => {
                self.id = None;
                vec![Effect::Release, Effect::Leave]
            }
            touch::Event::Cancel if self.id.is_some() => {
                self.id = None;
                vec![Effect::Leave, Effect::Release]
            }
            touch::Event::Gone => {
                self.id = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[test]
fn every_touch_sequence_matches_what_the_old_finger_did() {
    use touch::{Effect, Event};
    let alphabet = [
        Event::Down { id: 0, x: 1.0, y: 2.0 },
        Event::Down { id: 1, x: 3.0, y: 4.0 },
        Event::Motion { id: 0, x: 5.0, y: 6.0 },
        Event::Motion { id: 1, x: 7.0, y: 8.0 },
        Event::Up { id: 0 },
        Event::Up { id: 1 },
        Event::Cancel,
        Event::Gone,
    ];
    each_sequence(&alphabet, 5, &mut Vec::new(), &mut |seq| {
        let (mut finger, mut old) = (touch::State::default(), OldTouch::default());
        let (mut presses, mut releases, mut orphaned) = (0usize, 0usize, 0usize);
        for (i, &event) in seq.iter().enumerate() {
            // The seat going away lifts the finger without a release: the only press that ends
            // without one.
            if event == Event::Gone && old.id.is_some() {
                orphaned += 1;
            }
            let got: Vec<Effect> = touch::step(&mut finger, event).into_iter().collect();
            assert_eq!(got, old.step(event), "event {i} of {seq:?}");
            for effect in &got {
                match effect {
                    Effect::Press => presses += 1,
                    Effect::Release => releases += 1,
                    Effect::Move(..) | Effect::Leave => {}
                }
            }
            assert_eq!(matches!(finger, touch::State::Down { .. }), old.id.is_some(), "finger after {seq:?}");
            if let (touch::State::Down { id }, Some(old_id)) = (finger, old.id) {
                assert_eq!(id, old_id, "finger after {seq:?}");
            }
            // One finger: it never releases twice, and only a vanished seat leaves a press behind.
            assert!(releases <= presses && presses - releases <= 1 + orphaned, "presses {presses}, releases {releases}, orphaned {orphaned} in {seq:?}");
        }
    });
}

#[test]
fn fullscreen_placement_uses_exclusive_zone_minus_one_and_others_stay_zero() {
    use super::exclusive_zone;
    use crate::overlay::ScreenEdge;
    use crate::{Align, BarEdge, ExclusiveZone, OverlayPlacement};

    let auto = ExclusiveZone::Auto;
    assert_eq!(exclusive_zone(Some(OverlayPlacement::FullScreen), auto), -1);
    assert_eq!(exclusive_zone(None, auto), 0);
    assert_eq!(
        exclusive_zone(Some(OverlayPlacement::Edge {
            edge: ScreenEdge::Top,
            margin: 8.0,
            align: Align::Center,
            inset: 0.0,
        }), auto),
        0
    );
    assert_eq!(
        exclusive_zone(Some(OverlayPlacement::Rect { x: 10.0, y: 20.0, width: 200.0, height: 100.0 }), auto),
        0
    );
    assert_eq!(exclusive_zone(Some(OverlayPlacement::Bar { edge: BarEdge::Top, thickness: 32.0, margin: 0.0 }), auto), 0);
}

#[test]
fn a_reserved_zone_is_logical_points_only_where_the_anchors_honour_it() {
    use super::exclusive_zone;
    use crate::overlay::ScreenEdge;
    use crate::{Align, BarEdge, ExclusiveZone, OverlayPlacement};

    let bar = |edge| Some(OverlayPlacement::Bar { edge, thickness: 32.0, margin: 4.0 });
    for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
        assert_eq!(exclusive_zone(bar(edge), ExclusiveZone::Reserve(32.0)), 32, "{edge:?}");
    }
    // Points, whatever the output's scale: the protocol's zone is surface-local.
    assert_eq!(exclusive_zone(bar(BarEdge::Top), ExclusiveZone::Reserve(31.6)), 32);
    assert_eq!(exclusive_zone(bar(BarEdge::Top), ExclusiveZone::Reserve(-5.0)), 0);
    assert_eq!(exclusive_zone(bar(BarEdge::Top), ExclusiveZone::Reserve(f32::NAN)), 0);
    let edge = |align| Some(OverlayPlacement::Edge { edge: ScreenEdge::Bottom, margin: 0.0, align, inset: 0.0 });
    assert_eq!(exclusive_zone(edge(Align::Center), ExclusiveZone::Reserve(40.0)), 40);
    // A corner anchor, a free rect, a full-screen surface and the spec's own placement can't reserve.
    assert_eq!(exclusive_zone(edge(Align::Start), ExclusiveZone::Reserve(40.0)), 0);
    assert_eq!(exclusive_zone(edge(Align::End), ExclusiveZone::Reserve(40.0)), 0);
    assert_eq!(exclusive_zone(Some(OverlayPlacement::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 }), ExclusiveZone::Reserve(40.0)), 0);
    assert_eq!(exclusive_zone(Some(OverlayPlacement::FullScreen), ExclusiveZone::Reserve(40.0)), 0);
    assert_eq!(exclusive_zone(None, ExclusiveZone::Reserve(40.0)), 0);
}

#[test]
fn ignoring_others_extends_under_reserved_space_for_any_placement() {
    use super::exclusive_zone;
    use crate::{BarEdge, ExclusiveZone, OverlayPlacement};

    assert_eq!(exclusive_zone(None, ExclusiveZone::IgnoreOthers), -1);
    assert_eq!(exclusive_zone(Some(OverlayPlacement::Bar { edge: BarEdge::Left, thickness: 48.0, margin: 0.0 }), ExclusiveZone::IgnoreOthers), -1);
}
