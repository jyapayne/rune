//! Overlays that come and go while the app runs: added from `App::on_displays_changed` or
//! `AppHandle::add_overlay`, removed through `OverlayHandle::remove`, and a Wayland overlay
//! whose display went away, which stays unavailable rather than moving to another display.

use super::*;
use crate::ConfiguredOverlay;

/// `App::on_displays_changed`'s handler.
pub(crate) type OnDisplays = Box<dyn FnMut(&[crate::Display], &mut DisplaysCx<'_>)>;

/// An overlay `AppHandle::add_overlay` asked for, built on the event thread from what its app
/// gives every overlay.
pub(crate) type Deferred = Box<dyn FnOnce(&AppSeed) -> Box<dyn PendingOverlay> + Send>;

/// The handle of an overlay another thread asks for, and what builds its host on the event
/// thread, where its spec is made too (a `ConfiguredOverlay` holds what can't cross threads).
pub(crate) fn deferred<T, C>(spec: impl FnOnce() -> C + Send + 'static, state: T) -> (OverlayHandle<T>, Deferred)
where
    T: Send + 'static,
    C: Into<ConfiguredOverlay<T>>,
{
    let shared = Arc::new(Shared::default());
    let handle = OverlayHandle { shared: shared.clone() };
    (handle, Box::new(move |seed| Box::new(OverlayHost::seeded(spec().into(), state, seed, shared))))
}

/// What an `App::on_displays_changed` handler adds overlays through.
pub struct DisplaysCx<'a> {
    seed: &'a AppSeed,
    added: &'a mut Vec<Box<dyn PendingOverlay>>,
}

impl DisplaysCx<'_> {
    /// Adds an overlay as `App::overlay` does before the app runs: hidden until
    /// `OverlayHandle::show`, with the app's fonts and font smoothing. Its host is set up when
    /// the handler returns (`ConfiguredOverlay::setup`, a prewarm), in the same turn of the
    /// event loop, and what the handler asked of the handle meanwhile applies then.
    pub fn overlay<T: 'static>(&mut self, spec: impl Into<ConfiguredOverlay<T>>, state: T) -> OverlayHandle<T> {
        let shared = Arc::new(Shared::default());
        self.added.push(Box::new(OverlayHost::seeded(spec.into(), state, self.seed, shared.clone())));
        OverlayHandle { shared }
    }
}

/// What the event loop keeps of an `App` beside its window host, for the overlays added while
/// it runs: what each takes from the app, the handle's requests for them, and the app's
/// displays handler.
pub(crate) struct AppLink {
    shared: Arc<crate::app::SharedCore>,
    seed: AppSeed,
    on_displays: Option<OnDisplays>,
}

impl AppLink {
    pub(crate) fn new(shared: Arc<crate::app::SharedCore>, seed: AppSeed, on_displays: Option<OnDisplays>) -> Self {
        AppLink { shared, seed, on_displays }
    }

    /// Whether the app has an `on_displays_changed` handler.
    pub(crate) fn listens(&self) -> bool { self.on_displays.is_some() }

    /// Builds the overlays `AppHandle::add_overlay` asked for since the last call.
    pub(crate) fn take_added(&self, added: &mut Vec<Box<dyn PendingOverlay>>) {
        for make in self.shared.take_overlays() {
            added.push(make(&self.seed));
        }
    }

    /// Runs the app's displays handler, collecting the overlays it adds.
    pub(crate) fn displays_changed(&mut self, displays: &[crate::Display], added: &mut Vec<Box<dyn PendingOverlay>>) {
        if let Some(handler) = &mut self.on_displays {
            handler(displays, &mut DisplaysCx { seed: &self.seed, added });
        }
    }
}

impl SharedCore {
    pub(super) fn removed(&self) -> bool { self.removed.load(Ordering::Acquire) }
}

impl<T: 'static> OverlayHandle<T> {
    /// Removes the overlay for good, as if it had never been added: on the event loop's next
    /// turn its surface, renderer and runtime go, and its state is dropped there. Updates still
    /// queued don't run. From then on every call on this handle and its clones does nothing,
    /// and `availability` says `Unavailable`. For an overlay tied to something that went away,
    /// such as a bar on a display that was unplugged (`App::on_displays_changed`).
    pub fn remove(&self) {
        if self.shared.core.removed.swap(true, Ordering::AcqRel) { return; }
        self.shared.updates.lock().unwrap().clear();
        self.shared.core.wake();
    }
}

/// Why an overlay on a Wayland output of its own can't show once that output is gone.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
const DISPLAY_GONE: &str = "its display is gone";

impl OverlayCore {
    /// The registry name of the Wayland output the overlay asks for by identity
    /// (`DisplayTarget::Id`, or `Native` with a Wayland id); None when it leaves the choice to
    /// the compositor.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    fn own_output(&self) -> Option<u32> {
        let native = match &self.options.display {
            DisplayTarget::Id(id) => id.native_id(),
            DisplayTarget::Native(native) => Some(*native),
            DisplayTarget::Primary | DisplayTarget::UnderPointer => None,
        };
        match native {
            Some(crate::NativeDisplayId::Wayland(name)) => Some(name),
            _ => None,
        }
    }

    /// A Wayland overlay's surface, for a show or a prewarm: layer-shell, or XWayland where the
    /// compositor has none. An overlay on an output of its own gets neither once that output is
    /// gone: a layer surface with no output is put on whichever the compositor likes, beside
    /// what is there already (a second bar, reserving its edge twice).
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn wayland_surface(&mut self, app: &mut dyn OverlayApp, event_loop: &ActiveEventLoop) -> Option<Live> {
        use winit::platform::wayland::MonitorHandleExtWayland;
        let own = self.own_output();
        if own.is_some_and(|output| !event_loop.available_monitors().any(|m| m.native_id() == output)) {
            self.fail(DISPLAY_GONE.into());
            return None;
        }
        match self.layer(app, event_loop) {
            // winit still listed an output the registry no longer had: the surface was made
            // with none. Nothing was drawn on it yet.
            Some(live) if own.is_some() && matches!(&live.backing, Backing::Layer(layer) if !layer.has_output()) => {
                drop(live);
                self.fail(DISPLAY_GONE.into());
                None
            }
            Some(live) => Some(live),
            // A lock surface is a lock surface or nothing.
            None if self.lock.is_some() => {
                if self.shared.failure.lock().unwrap().is_none() {
                    self.fail("the compositor could not make its lock surface: the lock has ended or was refused".into());
                }
                None
            }
            None => self.xwayland(app),
        }
    }

    #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
    pub(super) fn wayland_surface(&mut self, app: &mut dyn OverlayApp, event_loop: &ActiveEventLoop) -> Option<Live> {
        self.layer(app, event_loop).or_else(|| self.xwayland(app))
    }

    /// The compositor closed the overlay's layer surface: its output went away, or the display
    /// connection failed. The overlay hides and is `Unavailable` until a later show makes a new
    /// surface, so whoever follows the displays can remove it.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    pub(super) fn surface_closed(&mut self, app: &mut dyn OverlayApp) {
        self.hide(app);
        // A prewarmed surface closed before its first show was never shown to hide.
        if matches!(self.surface.live().map(|l| &l.backing), Some(Backing::Layer(_))) {
            self.surface.take_live();
            self.drop_hidden_images(app);
        }
        *self.shared.failure.lock().unwrap() = Some("the compositor closed its surface: its display is gone, or the display connection failed".into());
        self.set_availability(Availability::Unavailable);
    }
}

#[cfg(test)]
#[path = "hotplug_tests.rs"]
mod tests;
