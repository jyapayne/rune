use std::path::Path;

use super::source::{
    self, ACTION_ASK, ACTION_COPY, ACTION_MOVE, Effect, Event, IconPlan, MAX_ICON_PIXELS, Pointer,
    PointerEvent, State, icon_pixels, icon_plan,
};
use crate::drag_out::{DragItem, DragOperation, DragOutcome};

fn steps(state: &mut State, events: &[Event]) -> Vec<Effect> {
    events.iter().flat_map(|&e| source::step(state, e)).collect()
}

#[test]
fn a_drag_offers_copy_by_default_and_the_actions_the_item_allows() {
    assert_eq!(source::actions(&DragItem::new().text("a")), ACTION_COPY);
    assert_eq!(source::actions(&DragItem::new().text("a").allow_move()), ACTION_COPY | ACTION_MOVE);
    assert_eq!(source::actions(&DragItem::new().text("a").operation(DragOperation::Move)), ACTION_MOVE);
    assert_eq!(source::actions(&DragItem::new().text("a").allow_move().allow(DragOperation::Ask)), ACTION_COPY | ACTION_MOVE | ACTION_ASK);
}

#[test]
fn a_link_only_drag_offers_a_copy_because_wayland_has_no_link_action() {
    assert_eq!(source::actions(&DragItem::new().text("a").operation(DragOperation::Link)), ACTION_COPY);
}

#[test]
fn files_offer_a_uri_list_and_their_paths_as_text() {
    let item = DragItem::new().file("/tmp/a b.txt").file("/tmp/c");
    assert_eq!(source::mime_types(&item), ["text/uri-list", "text/plain;charset=utf-8", "UTF8_STRING", "text/plain"]);
    assert_eq!(source::payload(&item, "text/uri-list").unwrap(), b"file:///tmp/a%20b.txt\r\nfile:///tmp/c\r\n");
    assert_eq!(source::payload(&item, "text/plain;charset=utf-8").unwrap(), b"/tmp/a b.txt\n/tmp/c");
}

#[test]
fn text_only_drags_offer_no_uri_list() {
    let item = DragItem::new().text("Hello from Rune");
    assert_eq!(source::mime_types(&item), ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"]);
    assert_eq!(source::payload(&item, "UTF8_STRING").unwrap(), b"Hello from Rune");
    assert_eq!(source::payload(&item, "text/uri-list"), None);
}

#[test]
fn an_items_own_text_wins_over_its_paths() {
    let item = DragItem::new().file("/tmp/a").text("label");
    assert_eq!(source::payload(&item, "text/plain").unwrap(), b"label");
}

#[test]
fn png_data_is_offered_as_an_image() {
    let item = DragItem::new().png(vec![1, 2, 3]);
    assert_eq!(source::mime_types(&item), ["image/png"]);
    assert_eq!(source::payload(&item, "image/png").unwrap(), [1, 2, 3]);
}

#[test]
fn a_type_never_offered_sends_nothing() {
    assert_eq!(source::payload(&DragItem::new().file("/a"), "application/x-unknown"), None);
}

#[test]
fn file_uris_percent_encode_reserved_and_non_ascii_bytes() {
    assert_eq!(source::file_uri(Path::new("/home/x/Ünïcode #1%.txt")), "file:///home/x/%C3%9Cn%C3%AFcode%20%231%25.txt");
    assert_eq!(source::file_uri(Path::new("/a-b_c.d~e")), "file:///a-b_c.d~e");
}

#[test]
fn relative_paths_become_absolute_uris() {
    let uri = source::file_uri(Path::new("rel.txt"));
    assert!(uri.starts_with("file:///") && uri.ends_with("/rel.txt"), "{uri}");
}

#[test]
fn a_finished_drop_reports_the_action_the_target_settled_on() {
    let mut state = State::started();
    let effects = steps(&mut state, &[Event::Action(ACTION_MOVE), Event::Action(ACTION_COPY), Event::DropPerformed, Event::Finished]);
    assert_eq!(effects, [Effect::End(DragOutcome::Copied)]);
    assert_eq!(state, State::Ended);

    let mut state = State::started();
    assert_eq!(steps(&mut state, &[Event::Action(ACTION_MOVE), Event::DropPerformed, Event::Finished]), [Effect::End(DragOutcome::Moved)]);
}

#[test]
fn an_ask_drop_reports_the_action_the_user_picked_after_the_drop() {
    let mut state = State::started();
    let effects = steps(&mut state, &[Event::Action(ACTION_ASK), Event::DropPerformed, Event::Action(ACTION_MOVE), Event::Finished]);
    assert_eq!(effects, [Effect::End(DragOutcome::Moved)]);
}

#[test]
fn a_target_that_finishes_without_an_action_took_a_copy() {
    let mut state = State::started();
    assert_eq!(steps(&mut state, &[Event::DropPerformed, Event::Finished]), [Effect::End(DragOutcome::Copied)]);
}

#[test]
fn a_cancelled_drag_ends_cancelled_before_or_after_the_drop() {
    let mut state = State::started();
    assert_eq!(steps(&mut state, &[Event::Action(ACTION_COPY), Event::Cancelled]), [Effect::End(DragOutcome::Cancelled)]);
    let mut state = State::started();
    assert_eq!(steps(&mut state, &[Event::Action(ACTION_COPY), Event::DropPerformed, Event::Cancelled]), [Effect::End(DragOutcome::Cancelled)]);
}

#[test]
fn a_drag_whose_seat_or_window_goes_away_ends_cancelled() {
    let mut state = State::started();
    assert_eq!(steps(&mut state, &[Event::Gone]), [Effect::End(DragOutcome::Cancelled)]);
}

#[test]
fn a_drag_reports_its_end_exactly_once() {
    let mut state = State::started();
    let effects = steps(&mut state, &[Event::DropPerformed, Event::Finished, Event::Cancelled, Event::Gone, Event::Finished, Event::Action(ACTION_MOVE)]);
    assert_eq!(effects, [Effect::End(DragOutcome::Copied)]);
    assert_eq!(state, State::Ended);
}

#[test]
fn the_icon_sits_where_the_picture_started_relative_to_the_pointer() {
    let plan = icon_plan([20.0, 40.0, 150.0, 92.0], (60.0, 70.0), 1.0);
    assert_eq!(plan, IconPlan { width: 150, height: 92, scale: 1, offset: (-40, -30) });
}

#[test]
fn the_icon_buffer_scale_rounds_the_window_scale_up_and_sides_are_whole_multiples() {
    let plan = icon_plan([0.0, 0.0, 10.25, 5.0], (0.0, 0.0), 1.5);
    assert_eq!((plan.scale, plan.width, plan.height), (2, 22, 10));
    assert_eq!(plan.width % 2, 0);
}

#[test]
fn a_huge_icon_is_shrunk_to_the_pixel_cap_keeping_its_shape() {
    let plan = icon_plan([0.0, 0.0, 4000.0, 1000.0], (0.0, 0.0), 1.0);
    assert_eq!((plan.width, plan.height), (MAX_ICON_PIXELS, MAX_ICON_PIXELS / 4));
}

#[test]
fn icon_pixels_are_premultiplied_bgra_at_the_plan_size() {
    let rgba = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 128]).unwrap();
    let plan = IconPlan { width: 2, height: 1, scale: 1, offset: (0, 0) };
    assert_eq!(icon_pixels(rgba.clone(), plan), [0, 0, 255, 255, 0, 128, 0, 128]);
    let bigger = icon_pixels(rgba, IconPlan { width: 4, height: 2, ..plan });
    assert_eq!(bigger.len(), 4 * 2 * 4);
}

#[test]
fn a_press_on_a_surface_holds_the_serial_a_drag_starts_with() {
    let mut pointer = Pointer::default();
    pointer.step(PointerEvent::Enter { surface: 3, at: (1.0, 2.0) });
    assert_eq!(pointer.grab(3), None, "no button held yet");
    pointer.step(PointerEvent::Press { serial: 41 });
    pointer.step(PointerEvent::Motion((12.0, 2.0)));
    assert_eq!(pointer.grab(3), Some((41, (12.0, 2.0))));
    assert_eq!(pointer.grab(4), None, "another window's drag can't use it");
}

#[test]
fn a_release_or_leave_ends_the_grab() {
    let mut pointer = Pointer::default();
    pointer.step(PointerEvent::Enter { surface: 3, at: (1.0, 2.0) });
    pointer.step(PointerEvent::Press { serial: 41 });
    pointer.step(PointerEvent::Release);
    assert_eq!(pointer, Pointer::Over { surface: 3, at: (1.0, 2.0) });
    pointer.step(PointerEvent::Press { serial: 42 });
    pointer.step(PointerEvent::Leave);
    assert_eq!(pointer, Pointer::Away);
}

#[test]
fn buttons_pressed_away_from_this_clients_surfaces_are_ignored() {
    let mut pointer = Pointer::default();
    pointer.step(PointerEvent::Press { serial: 9 });
    pointer.step(PointerEvent::Motion((5.0, 5.0)));
    assert_eq!(pointer, Pointer::Away);
}
