//! An overlay's Wayland surfaces on their own connections: what a layer or XWayland surface
//! read since the last turn (input, a new size or scale, its close), for a shown overlay's
//! frame and for a hidden one.

use super::*;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
use super::draw::{deliver_layer_ime, layer_resize_scale};

impl OverlayCore {
    /// Handles what the compositor sent a shown overlay's layer or XWayland surface since the
    /// last turn, before its frame: input, and a layer's new size or scale. False when the
    /// compositor closed the layer surface, and the overlay hid.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn surface_turn(&mut self, app: &mut dyn OverlayApp) -> bool {
        if let Some(Live { backing: Backing::Layer(layer), .. }) = self.surface.live_mut() {
            let changes = layer.dispatch();
            if changes.closed {
                self.hide(app);
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

    /// A layer or XWayland surface kept while hidden (prewarmed, not shown yet) still drains its
    /// connection so nothing queues for the first show.
    pub(super) fn drain_hidden(&mut self, app: &mut dyn OverlayApp) {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        {
            let closed = match self.surface.live_mut().map(|l| &mut l.backing) {
                Some(Backing::Layer(layer)) => layer.drain(),
                Some(Backing::Xwayland(surface)) => {
                    surface.drain();
                    false
                }
                _ => false,
            };
            if closed { self.hide(app); }
        }
        #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
        let _ = app;
    }

    /// One input event a layer or XWayland surface read on its own connection.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    fn surface_input(&mut self, app: &mut dyn OverlayApp, input: crate::platform::layer_shell::Input) {
        use crate::platform::layer_shell::Input;
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
                if !focused && self.options.keys == KeyPolicy::OnDemand {
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
            Input::Ime(ime) if self.options.keys == KeyPolicy::OnDemand => {
                deliver_layer_ime(app.rt_mut(), &mut self.ime, ime);
            }
            Input::Ime(_) => return,
        }
        self.dirty = true;
    }
}
