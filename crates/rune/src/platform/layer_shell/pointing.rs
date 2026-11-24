//! The layer surface's pointer and touch: `wl_pointer` and `wl_touch` dispatch into the
//! surface's input, and the cursor-shape-v1 shape for a Rune cursor. The axis and finger state
//! machines live in `layer_shell_machine`.

use wayland_client::protocol::{wl_pointer::{self, WlPointer}, wl_touch::{self, WlTouch}};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;

use super::keys::key_mods;
use super::super::layer_shell_machine::{pointer, touch};
use super::{Input, State};

/// Whether the pointer is over the surface: the serial of its enter, which setting a cursor
/// needs, and the cursor the surface set since (none: the one it arrived with).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Over {
    #[default]
    Outside,
    Inside { serial: u32, cursor: Option<crate::el::Cursor> },
}

impl Over {
    pub(super) fn inside(self) -> bool { matches!(self, Over::Inside { .. }) }
}

impl State {
    pub(super) fn step_pointer(&mut self, event: pointer::Event) {
        for effect in pointer::step(&mut self.pointer_axis, event) {
            let pointer::Effect::Wheel(wheel) = effect;
            self.input.push(Input::Wheel(wheel));
        }
    }

    fn flush_pointer_frame(&mut self, framed: bool) {
        let mods = self.xkb.as_ref().map_or_else(crate::Mods::default, key_mods);
        self.step_pointer(pointer::Event::Frame { mods, framed });
    }
}

fn pointer_axis(axis: wl_pointer::Axis) -> Option<pointer::Axis> {
    match axis {
        wl_pointer::Axis::VerticalScroll => Some(pointer::Axis::Vertical),
        wl_pointer::Axis::HorizontalScroll => Some(pointer::Axis::Horizontal),
        _ => None,
    }
}

fn pointer_axis_source(source: wl_pointer::AxisSource) -> Option<pointer::AxisSource> {
    match source {
        wl_pointer::AxisSource::Wheel => Some(pointer::AxisSource::Wheel),
        wl_pointer::AxisSource::Finger => Some(pointer::AxisSource::Finger),
        wl_pointer::AxisSource::Continuous => Some(pointer::AxisSource::Continuous),
        wl_pointer::AxisSource::WheelTilt => Some(pointer::AxisSource::WheelTilt),
        _ => None,
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(state: &mut Self, wl_pointer: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                // Until the surface sets a cursor, the pointer keeps the one it arrived with.
                let inside = state.surface.as_ref() == Some(&surface);
                state.step_pointer(pointer::Event::Reset);
                state.over = if inside { Over::Inside { serial, cursor: None } } else { Over::Outside };
                if inside { state.input.push(Input::Move(surface_x as f32, surface_y as f32)); }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } if state.over.inside() => state.input.push(Input::Move(surface_x as f32, surface_y as f32)),
            wl_pointer::Event::Leave { surface, .. } if state.surface.as_ref() == Some(&surface) => {
                state.over = Over::Outside;
                state.step_pointer(pointer::Event::Reset);
                state.input.push(Input::Leave);
            }
            wl_pointer::Event::Button { button, state: wayland_client::WEnum::Value(button_state), .. } if state.over.inside() => {
                let pressed = button_state == wl_pointer::ButtonState::Pressed;
                match pointer::button_from_linux(button) {
                    crate::PointerButton::Primary => {
                        state.input.push(Input::Button(pressed));
                        if pressed && let Some(xkb) = &state.xkb {
                            state.input.push(Input::Physical(crate::PhysicalInput::Modifiers(key_mods(xkb)), None));
                        }
                    }
                    crate::PointerButton::Secondary => state.input.push(Input::Secondary(pressed)),
                    other => state.input.push(Input::ExtraButton(other, pressed)),
                }
            }
            wl_pointer::Event::Axis { axis: wayland_client::WEnum::Value(axis), value, .. } if state.over.inside() => {
                if let Some(axis) = pointer_axis(axis) {
                    state.step_pointer(pointer::Event::Axis { axis, value });
                    if wl_pointer.version() < 5 { state.flush_pointer_frame(false); }
                }
            }
            wl_pointer::Event::AxisSource { axis_source: wayland_client::WEnum::Value(source) } if state.over.inside() => {
                if let Some(source) = pointer_axis_source(source) { state.step_pointer(pointer::Event::AxisSource(source)); }
            }
            wl_pointer::Event::AxisDiscrete { axis: wayland_client::WEnum::Value(axis), discrete } if state.over.inside() => {
                if let Some(axis) = pointer_axis(axis) { state.step_pointer(pointer::Event::AxisDiscrete { axis, discrete }); }
            }
            wl_pointer::Event::AxisValue120 { axis: wayland_client::WEnum::Value(axis), value120 } if state.over.inside() => {
                if let Some(axis) = pointer_axis(axis) { state.step_pointer(pointer::Event::AxisValue120 { axis, value120 }); }
            }
            wl_pointer::Event::AxisStop { axis: wayland_client::WEnum::Value(axis), .. } if state.over.inside() => {
                if let Some(axis) = pointer_axis(axis) { state.step_pointer(pointer::Event::AxisStop { axis }); }
            }
            wl_pointer::Event::Frame if state.over.inside() => state.flush_pointer_frame(true),
            _ => {}
        }
    }
}
impl Dispatch<WlTouch, ()> for State {
    fn event(state: &mut Self, _: &WlTouch, event: wl_touch::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use touch::Event as Touch;
        let event = match event {
            wl_touch::Event::Down { surface, id, x, y, .. } if state.surface.as_ref() == Some(&surface) => Touch::Down { id, x: x as f32, y: y as f32 },
            wl_touch::Event::Motion { id, x, y, .. } => Touch::Motion { id, x: x as f32, y: y as f32 },
            wl_touch::Event::Up { id, .. } => Touch::Up { id },
            wl_touch::Event::Cancel => Touch::Cancel,
            _ => return,
        };
        for effect in touch::step(&mut state.finger, event) {
            state.input.push(match effect {
                touch::Effect::Move(x, y) => Input::Move(x, y),
                touch::Effect::Press => Input::Button(true),
                touch::Effect::Release => Input::Button(false),
                touch::Effect::Leave => Input::Leave,
            });
        }
    }
}
impl Dispatch<WpCursorShapeManagerV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeManagerV1, _: <WpCursorShapeManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpCursorShapeDeviceV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: <WpCursorShapeDeviceV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// The cursor-shape-v1 shape for `c`, as `os_cursor` maps winit's. An image is the arrow.
pub(super) fn shape(c: crate::el::Cursor) -> Shape {
    use crate::el::Cursor;
    match c {
        Cursor::Default | Cursor::Hidden | Cursor::Image(_) => Shape::Default,
        Cursor::Pointer => Shape::Pointer,
        Cursor::Text => Shape::Text,
        Cursor::Grab => Shape::Grab,
        Cursor::Grabbing => Shape::Grabbing,
        Cursor::NotAllowed => Shape::NotAllowed,
        Cursor::Move => Shape::Move,
        Cursor::ResizeColumn => Shape::ColResize,
        Cursor::ResizeRow => Shape::RowResize,
        Cursor::Crosshair => Shape::Crosshair,
        Cursor::Cell => Shape::Cell,
        Cursor::ZoomIn => Shape::ZoomIn,
        Cursor::ZoomOut => Shape::ZoomOut,
    }
}
