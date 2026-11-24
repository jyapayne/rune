//! `hyprland-focus-grab-v1`, for an overlay that takes every key (`KeyPolicy::Exclusive`). An
//! exclusive layer surface never hears of a press outside itself, so while it is mapped its
//! surface is the whitelist of a focus grab: a press or touch anywhere else ends the grab, and
//! the compositor says so. The compositor's global is optional; without it there is no grab.

use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use super::super::layer_shell_machine::keyboard;
use super::State;

/// Bindings generated from the protocol's XML (`crates/rune/protocols`), as the
/// wayland-protocols crates generate theirs.
#[allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals, unused_imports, unused_variables, missing_docs, clippy::all)]
mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-focus-grab-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-focus-grab-v1.xml");
}

pub(super) use protocol::hyprland_focus_grab_manager_v1::HyprlandFocusGrabManagerV1;
pub(super) use protocol::hyprland_focus_grab_v1::HyprlandFocusGrabV1;
use protocol::hyprland_focus_grab_v1;

impl Dispatch<HyprlandFocusGrabManagerV1, ()> for State {
    fn event(_: &mut Self, _: &HyprlandFocusGrabManagerV1, _: <HyprlandFocusGrabManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<HyprlandFocusGrabV1, ()> for State {
    fn event(state: &mut Self, grab: &HyprlandFocusGrabV1, event: hyprland_focus_grab_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // A grab the surface already let go of was destroyed, and says nothing.
        if matches!(event, hyprland_focus_grab_v1::Event::Cleared) && state.grab.as_ref() == Some(grab) {
            // Cleared, it is inert: the next show's first frame makes a new one.
            state.release_grab();
            state.step_keyboard_focus(keyboard::FocusEvent::GrabCleared);
        }
    }
}

impl State {
    /// Guards the surface a buffer just mapped: from here a press outside it clears the grab.
    /// Nothing where the overlay doesn't take every key, or the compositor lacks the protocol.
    pub(super) fn grab_focus(&mut self, surface: &WlSurface, qh: &QueueHandle<Self>) {
        let Some(manager) = &self.grab_manager else { return };
        if self.grab.is_some() { return; }
        let grab = manager.create_grab(qh, ());
        grab.add_surface(surface);
        grab.commit();
        self.grab = Some(grab);
    }

    /// Ends the grab, if one guards the surface. Destroyed, it sends no `cleared`.
    pub(super) fn release_grab(&mut self) {
        if let Some(grab) = self.grab.take() { grab.destroy(); }
    }
}
