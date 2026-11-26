//! The source side of a Wayland drag (`wl_data_source` with `wl_data_device.start_drag`): what
//! a [`DragItem`] offers and sends for each type, which actions it allows, the drag icon's
//! pixels and placement, and the session's state machine.
//!
//! Pure and cross-platform, so every rule runs unit tests without a compositor; `wire/drag.rs`
//! binds it to the protocol.

use std::path::Path;

use crate::drag_out::{DragItem, DragOperation, DragOutcome};
use crate::machine::Effects;

use super::machine::{SurfaceId, URI_LIST_MIME};

/// Plain-text types, UTF-8 first: GTK and Qt targets ask for the first one they know.
pub(crate) const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// PNG image data.
pub(crate) const PNG_MIME: &str = "image/png";

/// `wl_data_device_manager.dnd_action` bits.
pub(crate) const ACTION_COPY: u32 = 1;
pub(crate) const ACTION_MOVE: u32 = 2;
pub(crate) const ACTION_ASK: u32 = 4;

/// The longest side of a drag icon, in buffer pixels; a larger picture is shown scaled down.
pub(crate) const MAX_ICON_PIXELS: u32 = 1024;

/// The `dnd_action` mask a drag offers: what the item allows. Wayland has no link action, so a
/// drag that offers only [`DragOperation::Link`] offers a copy.
pub(crate) fn actions(item: &DragItem) -> u32 {
    let mask = [(DragOperation::Copy, ACTION_COPY), (DragOperation::Move, ACTION_MOVE), (DragOperation::Ask, ACTION_ASK)]
        .into_iter()
        .filter(|&(op, _)| item.offers(op))
        .fold(0, |m, (_, bit)| m | bit);
    if mask == 0 { ACTION_COPY } else { mask }
}

/// The MIME types the source offers, in order of preference.
pub(crate) fn mime_types(item: &DragItem) -> Vec<&'static str> {
    let mut mimes = Vec::with_capacity(5);
    if !item.paths().is_empty() {
        mimes.push(URI_LIST_MIME);
    }
    if item.png_data().is_some() {
        mimes.push(PNG_MIME);
    }
    if text(item).is_some() {
        mimes.extend(TEXT_MIMES);
    }
    mimes
}

/// The bytes the source writes when the target asks for `mime`; `None` for a type it never
/// offered.
pub(crate) fn payload(item: &DragItem, mime: &str) -> Option<Vec<u8>> {
    if mime == URI_LIST_MIME {
        if item.paths().is_empty() {
            return None;
        }
        return Some(item.paths().iter().map(|p| file_uri(p) + "\r\n").collect::<String>().into_bytes());
    }
    if mime == PNG_MIME {
        return item.png_data().map(<[u8]>::to_vec);
    }
    if TEXT_MIMES.contains(&mime) {
        return text(item).map(String::into_bytes);
    }
    None
}

/// The item's own text, else its paths one per line.
fn text(item: &DragItem) -> Option<String> {
    if let Some(text) = item.plain_text() {
        return Some(text.to_owned());
    }
    (!item.paths().is_empty()).then(|| item.paths().iter().map(|p| crate::drag_out::path_text(p)).collect::<Vec<_>>().join("\n"))
}

/// A `file://` URI for `path` (made absolute against the working directory), every byte
/// outside RFC 3986's unreserved set and `/` percent-encoded.
pub(crate) fn file_uri(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut uri = String::from("file://");
    for &b in path_bytes(&absolute).iter() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(path.to_string_lossy().replace('\\', "/").into_bytes())
}

/// How the drag icon is drawn: its buffer size in pixels, its `wl_surface` buffer scale, and its
/// top-left relative to the pointer hotspot in surface coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IconPlan {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) scale: i32,
    pub(crate) offset: (i32, i32),
}

/// The icon for a picture shown at `frame` (x, y, w, h in window points) while the pointer is at
/// `pointer` (window points), on a window drawn at `window_scale`. The buffer scale is the
/// window's rounded up, so the icon stays sharp at fractional scales, and each side is a whole
/// multiple of it, as the protocol requires.
pub(crate) fn icon_plan(frame: [f32; 4], pointer: (f32, f32), window_scale: f64) -> IconPlan {
    let scale = (window_scale.ceil() as i32).max(1);
    let s = scale as f32;
    let (w, h) = (frame[2].max(1.0) * s, frame[3].max(1.0) * s);
    let shrink = (MAX_ICON_PIXELS as f32 / w.max(h)).min(1.0);
    let side = |v: f32| {
        let px = ((v * shrink).round() as u32).max(1);
        px.div_ceil(scale as u32) * scale as u32
    };
    let offset = ((frame[0] - pointer.0).round() as i32, (frame[1] - pointer.1).round() as i32);
    IconPlan { width: side(w), height: side(h), scale, offset }
}

/// A decoded picture as `wl_shm` `ARGB8888` pixels (premultiplied, little-endian B, G, R, A) at
/// the plan's size.
pub(crate) fn icon_pixels(mut rgba: image::RgbaImage, plan: IconPlan) -> Vec<u8> {
    if rgba.dimensions() != (plan.width, plan.height) {
        rgba = image::imageops::resize(&rgba, plan.width, plan.height, image::imageops::FilterType::Triangle);
    }
    let mut out = rgba.into_raw();
    for p in out.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        let premul = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
        *p = [premul(p[2]), premul(p[1]), premul(p[0]), p[3]];
    }
    out
}

/// What a seat's pointer is doing over this client's surfaces, as far as a drag needs: the
/// implicit grab's press serial `wl_data_device.start_drag` must name, and where the pointer is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Pointer {
    /// Over another client's surface, or none.
    #[default]
    Away,
    /// Over `surface` at `at` (surface-local points), no button held.
    Over { surface: SurfaceId, at: (f32, f32) },
    /// A button went down on `surface` with `serial`; the pointer is at `at`.
    Pressed { surface: SurfaceId, at: (f32, f32), serial: u32 },
}

/// Events stepping a seat's [`Pointer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PointerEvent {
    Enter { surface: SurfaceId, at: (f32, f32) },
    Leave,
    Motion((f32, f32)),
    Press { serial: u32 },
    Release,
}

impl Pointer {
    pub(crate) fn step(&mut self, event: PointerEvent) {
        *self = match (*self, event) {
            (_, PointerEvent::Enter { surface, at }) => Self::Over { surface, at },
            (_, PointerEvent::Leave) | (Self::Away, _) => Self::Away,
            (Self::Over { surface, .. }, PointerEvent::Motion(at)) => Self::Over { surface, at },
            (Self::Pressed { surface, serial, .. }, PointerEvent::Motion(at)) => Self::Pressed { surface, at, serial },
            (Self::Over { surface, at } | Self::Pressed { surface, at, .. }, PointerEvent::Press { serial }) => Self::Pressed { surface, at, serial },
            (Self::Over { surface, at } | Self::Pressed { surface, at, .. }, PointerEvent::Release) => Self::Over { surface, at },
        };
    }

    /// The press serial and pointer position while a button is held on `surface`.
    pub(crate) fn grab(&self, on: SurfaceId) -> Option<(u32, (f32, f32))> {
        match *self {
            Self::Pressed { surface, at, serial } if surface == on => Some((serial, at)),
            _ => None,
        }
    }
}

/// Where one drag session stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum State {
    /// The pointer carries the drag; `action` is the one the compositor last chose.
    Dragging { action: u32 },
    /// The user let go over a target that took it; waiting for the target to finish.
    Dropped { action: u32 },
    /// Reported and torn down.
    #[default]
    Ended,
}

impl State {
    pub(crate) fn started() -> Self { Self::Dragging { action: 0 } }
}

/// Events stepping a drag session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// `wl_data_source.action`: the action the compositor chose for the current target.
    Action(u32),
    /// `wl_data_source.dnd_drop_performed`.
    DropPerformed,
    /// `wl_data_source.dnd_finished`.
    Finished,
    /// `wl_data_source.cancelled`: no target took it, or the compositor replaced the source.
    Cancelled,
    /// The seat, the window or the display connection went away.
    Gone,
}

/// What a step asks the binding to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Destroy the source and its icon, and report how the drag ended.
    End(DragOutcome),
}

/// Advances `state` by `event`.
pub(crate) fn step(state: &mut State, event: Event) -> Effects<Effect> {
    match (*state, event) {
        (State::Ended, _) => Effects::none(),
        (State::Dragging { .. }, Event::Action(action)) => {
            *state = State::Dragging { action };
            Effects::none()
        }
        (State::Dropped { .. }, Event::Action(action)) => {
            *state = State::Dropped { action };
            Effects::none()
        }
        (State::Dragging { action }, Event::DropPerformed) => {
            *state = State::Dropped { action };
            Effects::none()
        }
        (State::Dropped { .. }, Event::DropPerformed) => Effects::none(),
        (State::Dragging { action } | State::Dropped { action }, Event::Finished) => {
            *state = State::Ended;
            Effects::one(Effect::End(outcome(action)))
        }
        (State::Dragging { .. } | State::Dropped { .. }, Event::Cancelled | Event::Gone) => {
            *state = State::Ended;
            Effects::one(Effect::End(DragOutcome::Cancelled))
        }
    }
}

/// How a finished drop ended for the action it settled on. A target that finished without
/// choosing one (an older `wl_data_offer`) took a copy.
fn outcome(action: u32) -> DragOutcome {
    if action & ACTION_MOVE != 0 { DragOutcome::Moved } else { DragOutcome::Copied }
}
