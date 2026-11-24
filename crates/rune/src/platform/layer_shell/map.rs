//! The layer surface's role: the state it asks the compositor for (size, anchors, margins,
//! exclusive zone, keyboard interactivity, input region, buffer scale), and carrying out what
//! `layer_shell_machine::mapping` decides as the surface is made, configured, shown, hidden and
//! shown again.

use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1};

use super::super::layer_shell_machine::{exclusive_zone, frame_callback, keyboard, mapping, pointer, repeat, touch};
use super::{LayerSurface, Over, State};
use crate::el::Align;
use crate::machine::Attempt;
use crate::overlay::{Placement, ScreenEdge};
use crate::OverlayPlacement;

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(state: &mut Self, _: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                state.step_mapping(mapping::Event::Configure { serial, width, height });
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.step_mapping(mapping::Event::Closed);
                state.drop_frame_callback(conn);
            }
            _ => {}
        }
    }
}

/// The `wl_display.sync` sent behind an unmap: once it answers, whatever the compositor sends
/// it sent after the unmap reached it.
impl Dispatch<WlCallback, Attempt> for State {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, fence: &Attempt, _: &Connection, _: &QueueHandle<Self>) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.fence = None;
            state.step_mapping(mapping::Event::Fenced { fence: *fence, now: crate::time::now() });
        }
    }
}

impl State {
    /// Steps the surface's mapping. A dispatch handler holds only this state, so what the step
    /// asks of the surface's proxies waits in `asked` for `LayerSurface::carry_out`.
    pub(super) fn step_mapping(&mut self, event: mapping::Event) {
        self.asked.extend(mapping::step(&mut self.mapping_seq, &mut self.mapping, event));
    }

    /// The frame callback the surface waited on won't come: it was closed or unmapped, or its
    /// connection died.
    pub(super) fn drop_frame_callback(&mut self, conn: &Connection) {
        frame_callback::step(&mut self.frame, frame_callback::Event::Reset);
        if let Some((_, cb)) = self.frame_callback.take() {
            conn.backend().destroy_object(&cb.id()).ok();
        }
    }

    /// Lets go of what a mapped surface held, before its null buffer: the grab guarding it, key
    /// repeat, a compose sequence, the input method, the pointer and finger over it, the frame
    /// callback it waited on and input not yet delivered. Keyboard focus is the compositor's
    /// to take: its `leave` says when (`keyboard::FocusState`), and until then a hidden
    /// surface's keys are dropped as they come (`LayerSurface::drain`).
    fn release_mapped(&mut self, conn: &Connection) {
        self.release_grab();
        self.step_repeat(repeat::Event::Stop);
        self.reset_compose();
        self.drain_text_input();
        self.over = Over::Outside;
        self.step_pointer(pointer::Event::Reset);
        touch::step(&mut self.finger, touch::Event::Gone);
        self.drop_frame_callback(conn);
        self.input.clear();
    }
}

impl LayerSurface {
    /// Sends the size, anchors, margins and exclusive zone `at` and the overlay's placement
    /// ask for; they wait for a commit.
    pub fn place(&mut self, at: Placement) {
        self.at = at;
        self.layer.set_exclusive_zone(exclusive_zone(self.options.placement, self.options.exclusive_zone));
        match self.options.placement {
            Some(OverlayPlacement::FullScreen) => {
                self.layer.set_size(0, 0);
                self.layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                self.layer.set_margin(0, 0, 0, 0);
                return;
            }
            Some(OverlayPlacement::Rect { x, y, width, height }) => {
                self.layer.set_size(width.ceil().max(1.0) as u32, height.ceil().max(1.0) as u32);
                self.layer.set_anchor(Anchor::Top | Anchor::Left);
                self.layer.set_margin(y.round() as i32, 0, 0, x.round() as i32);
                return;
            }
            Some(OverlayPlacement::Bar { edge, thickness, margin }) => {
                // Zero along the edge: the compositor assigns the display's length there and
                // reports it in `configure`.
                let (t, m) = (thickness.ceil().max(1.0) as u32, margin.round() as i32);
                let (anchor, size, margins) = match edge {
                    crate::BarEdge::Top => (Anchor::Top | Anchor::Left | Anchor::Right, (0, t), (m, 0, 0, 0)),
                    crate::BarEdge::Bottom => (Anchor::Bottom | Anchor::Left | Anchor::Right, (0, t), (0, 0, m, 0)),
                    crate::BarEdge::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, (t, 0), (0, 0, 0, m)),
                    crate::BarEdge::Right => (Anchor::Right | Anchor::Top | Anchor::Bottom, (t, 0), (0, m, 0, 0)),
                };
                self.layer.set_size(size.0, size.1);
                self.layer.set_anchor(anchor);
                self.layer.set_margin(margins.0, margins.1, margins.2, margins.3);
                return;
            }
            _ => {}
        }
        let at = match self.options.placement {
            Some(OverlayPlacement::Edge { edge, margin, align, inset }) => Placement { size: at.size, edge, margin, align, inset },
            _ => at,
        };
        let (margin, inset) = (at.margin.round() as i32, at.inset.round() as i32);
        let (vertical, top, bottom) = match at.edge {
            ScreenEdge::Top => (Anchor::Top, margin, 0),
            ScreenEdge::Bottom => (Anchor::Bottom, 0, margin),
        };
        let (horizontal, left, right) = match at.align {
            Align::Start => (Anchor::Left, inset, 0),
            Align::Center => (Anchor::empty(), 0, 0),
            Align::End => (Anchor::Right, 0, inset),
        };
        self.layer.set_size(at.size.0.ceil() as u32, at.size.1.ceil() as u32);
        self.layer.set_anchor(vertical | horizontal);
        self.layer.set_margin(top, right, bottom, left);
    }

    /// The overlay shows. A surface the last hide unmapped commits its role state again and
    /// waits for the compositor's configure, which a later `dispatch` acks: nothing blocks
    /// here, and `frame_ready` says when a frame may draw. One that takes every key asks for
    /// them from here, so the compositor has them with its first buffer.
    pub fn show(&mut self, at: Placement) {
        self.at = at;
        self.ask_interactivity(keyboard::shown_interactivity(self.options.keys));
        self.state.step_mapping(mapping::Event::Map { now: crate::time::now() });
        self.carry_out();
        self.flush_checked();
    }

    /// The overlay hides and keeps the surface. A null buffer unmaps it, which puts the layer
    /// surface back in the state it was made in; the `wl_surface`, the layer surface and what
    /// draws into them stay for the next `show`.
    pub fn hide(&mut self) {
        self.ask_interactivity(keyboard::Interactivity::None);
        self.state.step_mapping(mapping::Event::Unmap);
        self.carry_out();
        self.flush_checked();
    }

    /// A frame presented: wgpu committed a buffer, so the compositor shows the surface.
    pub fn presented(&mut self) {
        self.state.step_mapping(mapping::Event::Presented);
        // Only a show's first buffer asks for anything: the grab that guards the mapped surface.
        if !self.state.asked.is_empty() {
            self.carry_out();
            self.flush_checked();
        }
    }

    /// Changes the keyboard interactivity the role state asks for; it waits for a commit.
    fn ask_interactivity(&mut self, interactivity: keyboard::Interactivity) {
        if self.interactivity == interactivity { return; }
        self.interactivity = interactivity;
        self.send_interactivity();
    }

    /// Sends the keyboard interactivity the role state asks for; it waits for a commit.
    pub(super) fn send_interactivity(&self) {
        self.layer.set_keyboard_interactivity(match self.interactivity {
            keyboard::Interactivity::None => KeyboardInteractivity::None,
            keyboard::Interactivity::OnDemand => KeyboardInteractivity::OnDemand,
            keyboard::Interactivity::Exclusive => KeyboardInteractivity::Exclusive,
        });
    }

    /// When the compositor's configure is overdue, for the overlay's wake timer.
    pub fn configure_deadline(&self) -> Option<crate::time::Instant> {
        self.state.mapping.deadline()
    }

    /// Commits role state (placement, keyboard interactivity, input region) without a new
    /// buffer; wgpu's present commits buffers. An unmapped surface commits nothing, since a
    /// commit there asks for a configure: its next show sends the role state whole.
    pub fn commit(&mut self) {
        if self.state.mapping.committed() { self.surface.commit(); }
        self.flush_checked();
    }

    pub fn set_buffer_scale(&mut self, scale: i32) {
        self.buffer_scale = scale;
        if self.surface.version() >= 3 {
            self.surface.set_buffer_scale(scale);
        }
    }

    /// Carries out what the mapping asked for since the last call. The queue's buffer goes
    /// back for the next: nothing here steps the mapping.
    pub(super) fn carry_out(&mut self) {
        let mut asked = std::mem::take(&mut self.state.asked);
        for effect in asked.drain(..) {
            match effect {
                mapping::Effect::Commit => {
                    self.place(self.at);
                    self.send_interactivity();
                    self.send_input_region();
                    self.set_buffer_scale(self.buffer_scale);
                    self.surface.commit();
                }
                mapping::Effect::Ack(serial) => self.layer.ack_configure(serial),
                mapping::Effect::Mapped => {
                    self.state.step_keyboard_focus(keyboard::FocusEvent::Mapped);
                    self.state.grab_focus(&self.surface, &self.queue.handle());
                }
                mapping::Effect::Unmap { fence } => {
                    self.state.release_mapped(&self.conn);
                    self.surface.attach(None, 0, 0);
                    self.surface.commit();
                    self.state.fence = Some(self.conn.display().sync(&self.queue.handle(), fence));
                }
                // `configure_deadline` hands it to the overlay's wake timer.
                mapping::Effect::WakeAt(_) => {}
                mapping::Effect::GaveUp => {
                    eprintln!("rune: Wayland layer surface '{}' closed: the compositor did not configure it within {:?}", self.namespace, mapping::TIMEOUT);
                }
            }
        }
        self.state.asked = asked;
    }
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
