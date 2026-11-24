//! An overlay's Wayland surfaces on their own connections: what a layer or XWayland surface
//! read since the last turn (input, a new size or scale, its end), for a shown overlay's frame
//! and for a hidden one, and what a hide does with the surface.

use super::*;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
use super::draw::{deliver_layer_ime, layer_resize_scale};

impl OverlayCore {
    /// Handles what the compositor sent a shown overlay's layer or XWayland surface since the
    /// last turn, before its frame: input, and a layer's new size or scale. False when the
    /// layer surface is gone, and the overlay hid.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn surface_turn(&mut self, app: &mut dyn OverlayApp) -> bool {
        if let Some(Live { backing: Backing::Layer(layer), .. }) = self.surface.live_mut() {
            let changes = layer.dispatch();
            if let Some(gone) = changes.gone {
                self.surface_gone(app, gone);
                return false;
            }
            for input in changes.input { self.surface_input(app, input); }
            if let Some((w, h)) = changes.size {
                let size = (if w == 0 { self.at.size.0 } else { w as f32 }, if h == 0 { self.at.size.1 } else { h as f32 });
                if size != self.at.size {
                    self.at.size = size;
                    if let Some(live) = self.surface.live_mut() { live.scale = 0.0; }
                }
            }
            if let Some(scale) = changes.scale {
                if let Some(Live { backing: Backing::Layer(layer), .. }) = self.surface.live_mut() { layer.set_buffer_scale(scale); }
                if let Some(gpu) = self.gpu.clone() {
                    self.rescale(app, &gpu, scale as f32);
                }
            }
            if self.surface.live().is_some_and(|l| l.scale == 0.0) && let Some(gpu) = self.gpu.clone() {
                // Read before `app` is reborrowed for `rescale`.
                let scale = layer_resize_scale(app.rt());
                self.rescale(app, &gpu, scale);
            }
        }
        if let Some(Live { backing: Backing::Xwayland(surface), .. }) = self.surface.live_mut() {
            let input = surface.dispatch();
            for input in input { self.surface_input(app, input); }
        }
        true
    }

    /// A layer or XWayland surface kept while hidden (prewarmed and not shown yet, or unmapped
    /// by a hide) still drains its connection so nothing queues for the next show.
    pub(super) fn drain_hidden(&mut self, app: &mut dyn OverlayApp) {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        {
            let gone = match self.surface.live_mut().map(|l| &mut l.backing) {
                Some(Backing::Layer(layer)) => layer.drain(),
                Some(Backing::Xwayland(surface)) => {
                    surface.drain();
                    None
                }
                _ => None,
            };
            if let Some(gone) = gone { self.surface_gone(app, gone); }
        }
        #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
        let _ = app;
    }

    /// The layer surface can't show again: the overlay hides, the surface goes whether or not
    /// a hide would keep it, and the overlay is `Unavailable` until a later show makes a new
    /// one. One the compositor never configured says so, and so does a lock surface whose lock
    /// ended.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    fn surface_gone(&mut self, app: &mut dyn OverlayApp, gone: crate::platform::layer_shell::Gone) {
        use crate::platform::layer_shell::Gone;
        match gone {
            Gone::Closed if self.lock.as_ref().is_some_and(|lock| lock.state().ended()) => {
                self.hide(app);
                self.drop_surface(app);
                *self.shared.failure.lock().unwrap() = Some("its session lock has ended".into());
                self.set_availability(Availability::Unavailable);
            }
            Gone::Closed => self.surface_closed(app),
            Gone::Unanswered => self.surface_refused(app, "the compositor did not configure its surface".into()),
        }
    }

    /// A prewarmed layer surface that keeps its GPU stays through a hide, as a prewarmed window
    /// does elsewhere: unmapped, so the next show makes no `wl_surface`, layer surface or
    /// swapchain, and waits for nothing on the UI thread. A lock surface can't be unmapped.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    fn keeps_layer(&self) -> bool { self.options.prewarm && !self.defers_gpu() && self.lock.is_none() }

    /// The overlay hid: a layer surface it keeps is unmapped; any other layer or XWayland
    /// surface is destroyed.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn hide_surface(&mut self, app: &mut dyn OverlayApp) {
        if self.keeps_layer() && let Some(Live { backing: Backing::Layer(layer), .. }) = self.surface.live_mut() && layer.is_live() {
            layer.hide();
        } else {
            self.drop_surface(app);
        }
    }

    /// Destroys the layer or XWayland surface with its GPU surface, and drops the images its
    /// last frame held.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn drop_surface(&mut self, app: &mut dyn OverlayApp) {
        self.surface.take_live();
        self.drop_hidden_images(app);
    }

    /// One input event a layer or XWayland surface read on its own connection.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    fn surface_input(&mut self, app: &mut dyn OverlayApp, input: crate::platform::layer_shell::Input) {
        use crate::platform::layer_shell::Input;
        // A compositor holding the overlay's focus grab (Hyprland, for `KeyPolicy::Exclusive`)
        // sends it the press that lands outside it, at a position outside its bounds, before
        // ending the grab. That press is not the overlay's, wherever its pointer last was.
        if matches!(input, Input::Button(true) | Input::Secondary(true) | Input::ExtraButton(_, true))
            && !app.rt().pointer().is_some_and(|at| press_on_surface(self.at.size, app.rt().zoom(), at))
        {
            return;
        }
        match input {
            Input::Move(x, y) => {
                let zoom = app.rt().zoom();
                self.dirty |= app.observe_pointer(x / zoom, y / zoom);
                return;
            }
            Input::Leave => app.rt_mut().pointer_leave(),
            Input::Button(down) => if let Some((x, y)) = app.rt().pointer() {
                if down { app.pointer_down(x, y); } else { app.pointer_up(x, y); }
            },
            Input::Secondary(down) => if let Some((x, y)) = app.rt().pointer() { if down { app.rt_mut().secondary_down(x, y) } else { app.secondary_up(x, y) } },
            Input::ExtraButton(button, down) => if let Some((x, y)) = app.rt().pointer() {
                if down { app.button_down(x, y, button); } else { app.button_up(x, y, button); }
            },
            Input::Wheel(wheel) => {
                if crate::input::host_events::wheel_core(app.rt_mut(), wheel) == crate::input::host_events::WheelOutcome::Ignored {
                    return;
                }
            }
            Input::Focus(focused) => {
                app.window_focus(focused);
                // Assistive clients hear which window has the keys, and so where focus is.
                if let Some(adapter) = self.surface.live_mut().and_then(|l| l.a11y.as_mut()) { adapter.focus(focused); }
                if !focused && self.options.keys.takes_keys() {
                    self.ime.clear();
                    if let Some(Live { backing: Backing::Layer(layer), .. }) = self.surface.live_mut()
                        && let Some(ime) = layer.sync_ime(crate::input::host_events::ImeStep {
                            cancel: false,
                            set_allowed: Some(false),
                            cursor_area: None,
                        })
                    {
                        crate::input::host_events::ime_core(app.rt_mut(), None, ime);
                    }
                }
                app.rt_mut().ignore_minimize("an overlay");
                if matches!(app.rt_mut().shell_request.take(), Some(crate::el::Shell::Close)) {
                    self.hide(app);
                    return;
                }
            }
            Input::Key(key) => app.rt_mut().key(key),
            Input::KeyRepeat(key) => app.rt_mut().repeat_key(key),
            Input::KeyUp(key) => {
                self.dirty |= app.key_up(key);
                return;
            }
            Input::Physical(event, fallback) => {
                let keep = matches!(event, crate::PhysicalInput::Key { pressed: true, .. }) || app.rt().wants_key_up();
                self.dirty |= app.physical_with_fallback(event, fallback.filter(|_| keep));
                return;
            }
            Input::Ime(ime) if self.options.keys.takes_keys() => {
                deliver_layer_ime(app.rt_mut(), &mut self.ime, ime);
            }
            Input::Ime(_) => return,
            Input::Keyboard(state) => {
                self.dirty |= app.keyboard_state(&state);
                return;
            }
        }
        self.dirty = true;
    }
}

/// Whether a press with the runtime's pointer at `at` (points of the zoomed view) lands on a
/// surface `size` points across. A pointer the compositor reports beyond the surface's edges
/// is over something else.
#[cfg_attr(not(any(test, all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))), allow(dead_code))]
fn press_on_surface(size: (f32, f32), zoom: f32, (x, y): (f32, f32)) -> bool {
    x >= 0.0 && y >= 0.0 && x * zoom < size.0 && y * zoom < size.1
}

#[cfg(test)]
#[path = "layer_tests.rs"]
mod tests;
