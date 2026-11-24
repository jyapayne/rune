//! An app's overlays: the ones it adds before it runs, the ones added while it runs, and the
//! displays handler that adds and removes them as displays come and go.

use super::{App, AppHandle};

impl<S: 'static> App<S> {
    /// Adds a transparent overlay with its own state, hidden until `OverlayHandle::show`.
    /// Bare specs preserve primary-display edge placement and never request key focus;
    /// `OverlaySpec::options` selects displays, placement and native input policy.
    /// Uses this app's fonts and font smoothing (`ConfiguredOverlay::font_smoothing` overrides it).
    /// Headless runs don't draw overlays.
    pub fn overlay<T: 'static>(&mut self, spec: impl Into<crate::ConfiguredOverlay<T>>, state: T) -> crate::overlay::OverlayHandle<T> {
        let (overlay, handle) = crate::overlay::OverlayHost::new(spec.into(), state, &self.spec, self.runtime.font_smoothing);
        self.overlays.push(Box::new(overlay));
        handle
    }

    /// Hears the displays: `f` runs on the event thread once when the app starts, with every
    /// display connected then, and on Wayland again each time one is added or removed or
    /// changes its mode, scale, position or name, with the whole list as it is now. It runs
    /// whether or not the app has a window or any overlay, so a `windowless` app can keep one
    /// overlay per display: add one through the [`DisplaysCx`](crate::DisplaysCx) for a display
    /// that came (target it with `DisplayTarget::Id(display.id)`), and call
    /// `OverlayHandle::remove` on the one whose display went. On Wayland `Display::name` is
    /// the connector's (`DP-2`), which stays the same when a display is unplugged and plugged
    /// in again while its `id` doesn't, and a display appears in the list once the compositor
    /// has described it. Other platforms call `f` at startup only. Headless runs and the web
    /// never call it.
    ///
    /// ```ignore
    /// let mut bars: Vec<(rune::DisplayId, OverlayHandle<Bar>)> = Vec::new();
    /// app.on_displays_changed(move |displays, cx| {
    ///     bars.retain(|(id, bar)| {
    ///         let stays = displays.iter().any(|d| d.id.as_ref() == Some(id));
    ///         if !stays { bar.remove(); }
    ///         stays
    ///     });
    ///     for display in displays {
    ///         let Some(id) = display.id.clone() else { continue };
    ///         if bars.iter().any(|(have, _)| *have == id) { continue; }
    ///         let bar = cx.overlay(bar_spec(DisplayTarget::Id(id.clone())), Bar::default());
    ///         bar.show();
    ///         bars.push((id, bar));
    ///     }
    /// })
    /// ```
    pub fn on_displays_changed(mut self, f: impl FnMut(&[crate::Display], &mut crate::DisplaysCx<'_>) + 'static) -> Self {
        self.on_displays = Some(Box::new(f));
        self
    }

    /// What the event loop keeps of this app for the overlays added while it runs.
    pub(crate) fn overlay_link(&mut self) -> crate::overlay::AppLink {
        let seed = crate::overlay::AppSeed::of(&self.spec, self.runtime.font_smoothing);
        crate::overlay::AppLink::new(self.shared.core.clone(), seed, self.on_displays.take())
    }
}

impl<S: 'static> AppHandle<S> {
    /// Adds an overlay while the app runs, as `App::overlay` does before: hidden until
    /// `OverlayHandle::show`, with the app's fonts and font smoothing. `spec` runs on the event
    /// thread, where the overlay's host is made, since a `ConfiguredOverlay` may hold what
    /// can't cross threads (`on_created`, a compositor). The handle works at once: what is
    /// asked of it applies when the host exists. Called before `run`, the overlay is added when
    /// the app starts; once the app's window has gone for good (not merely closed with
    /// `keep_running`), and in headless runs and on the web, it never is.
    /// `OverlayHandle::remove` takes it away again.
    pub fn add_overlay<T, C>(&self, spec: impl FnOnce() -> C + Send + 'static, state: T) -> crate::OverlayHandle<T>
    where
        T: Send + 'static,
        C: Into<crate::ConfiguredOverlay<T>>,
    {
        let (handle, make) = crate::overlay::deferred(spec, state);
        self.shared.core.add_overlay(make);
        handle
    }

    /// Asks the compositor to lock the session (Wayland `ext-session-lock-v1`), for a lock
    /// screen: see [`crate::session_lock`]. The lock is sent on the event loop's next turn, or
    /// as soon as one of its lock surfaces is made (`ConfiguredOverlay::session_lock`).
    /// `on_change` hears each new [`LockState`](crate::LockState) on the event thread: `Locked`
    /// once the compositor shows only lock surfaces, then `Unlocked` after
    /// [`SessionLock::unlock`](crate::SessionLock::unlock), or `Finished` if the compositor
    /// refuses or ends the lock, or `Unsupported`. Only `unlock` unlocks: dropping the handles
    /// or the process ending leaves the session locked.
    pub fn lock_session(&self, on_change: impl Fn(crate::LockState) + Send + Sync + 'static) -> crate::SessionLock {
        let core = self.shared.core.clone();
        crate::SessionLock::asked(on_change, move || core.wake())
    }
}
