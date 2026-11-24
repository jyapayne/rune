//! The displays as an app's handler hears them (`App::on_displays_changed`): once at launch,
//! and on Wayland again whenever the compositor adds, removes or describes an output anew.
//! Also each app's link for the overlays added while it runs, from that handler or its
//! `AppHandle`.
//!
//! winit 0.30 keeps its Wayland monitor list current but says nothing when it changes, so a
//! `wayland_output::Watcher` follows the outputs on a private queue of winit's connection, kept
//! only while a handler listens. winit reads the socket; with a watcher it wakes the loop on
//! every read (`wayland_wake`), and each turn handles what was queued.

use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;

use super::AnyHost;
use crate::machine::Effects;
use crate::overlay::{AppLink, PendingOverlay};
use crate::windows::Display;
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
use crate::platform::wayland_output::{self, RawOutput, ResolvedOutput, Watcher};

/// Whether the handlers have heard the displays as they are.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum State {
    /// The displays may differ from the list the handlers heard last (`None`: they have heard
    /// none): the next turn looks.
    Stale(Option<Vec<Display>>),
    /// The handlers heard this list, and no display changed since.
    Current(Vec<Display>),
}

impl Default for State {
    fn default() -> Self { State::Stale(None) }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Event {
    /// The compositor added or removed an output, or described one anew.
    OutputsChanged,
    /// A look at the displays. `settled` is false while the list is incomplete: the compositor
    /// described an output winit doesn't list yet.
    Looked { displays: Vec<Display>, settled: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Every handler hears the list the state now holds.
    Notify,
}

/// A change that leaves the list as the handlers heard it (an output described again with
/// what it had) tells them nothing, and an incomplete list waits for the next look.
pub(crate) fn step(state: &mut State, event: Event) -> Effects<Effect> {
    match event {
        Event::OutputsChanged => {
            if let State::Current(heard) = state {
                *state = State::Stale(Some(std::mem::take(heard)));
            }
            Effects::none()
        }
        Event::Looked { settled: false, .. } => Effects::none(),
        Event::Looked { displays, settled: true } => {
            let heard = match &*state {
                State::Stale(heard) => heard.as_ref(),
                State::Current(heard) => Some(heard),
            };
            let news = heard != Some(&displays);
            *state = State::Current(displays);
            if news { Effects::one(Effect::Notify) } else { Effects::none() }
        }
    }
}

/// How the displays are followed for the handlers.
#[derive(Default)]
pub(super) enum Outputs {
    /// Not asked: no handler has needed them yet.
    #[default]
    Unasked,
    /// winit's list, heard once: not Wayland, or its outputs can't be followed.
    Listed,
    /// Wayland: the compositor's outputs, followed as they come, go and change.
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    Watched(Box<Watcher>),
}

impl Outputs {
    fn ask(event_loop: &ActiveEventLoop) -> Outputs {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        if let Some(watcher) = watcher(event_loop) { return Outputs::Watched(watcher); }
        let _ = event_loop;
        Outputs::Listed
    }

    /// The displays now, and whether the list is complete.
    fn list(&self, event_loop: &ActiveEventLoop) -> (Vec<Display>, bool) {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        if let Outputs::Watched(watcher) = self { return watched(event_loop, watcher); }
        let wayland = super::is_wayland(event_loop);
        let primary = event_loop.primary_monitor();
        (event_loop.available_monitors().map(|m| display(&m, wayland, primary.as_ref())).collect(), true)
    }
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn watcher(event_loop: &ActiveEventLoop) -> Option<Box<Watcher>> {
    if !super::is_wayland(event_loop) { return None; }
    let display = winit::raw_window_handle::HasDisplayHandle::display_handle(event_loop).ok()?;
    Watcher::new(display.as_raw()).map(Box::new)
}

/// A display as winit's monitor has it.
fn display(m: &MonitorHandle, wayland: bool, primary: Option<&MonitorHandle>) -> Display {
    Display {
        id: Some(crate::DisplayId::from_monitor(m.clone(), wayland)),
        name: m.name(),
        position: (m.position().x, m.position().y),
        size: (m.size().width, m.size().height),
        scale: m.scale_factor(),
        primary: primary == Some(m),
    }
}

/// A Wayland display with its position, size and scale as the compositor said them of its
/// output (`wayland_output::resolve`), where it did.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn corrected(mut display: Display, raw: Option<&RawOutput>) -> Display {
    let resolved = wayland_output::resolve(raw, ResolvedOutput { position: display.position, size: display.size, scale: display.scale });
    (display.position, display.size, display.scale) = (resolved.position, resolved.size, resolved.scale);
    display
}

/// The displays connected at launch, for the window factory, and on Wayland the watcher that
/// asked the compositor of them, for `Displays::launched` to keep if a handler listens.
pub(super) fn at_launch(event_loop: &ActiveEventLoop, wayland: bool) -> (Vec<Display>, Outputs) {
    let primary = event_loop.primary_monitor();
    let monitors = event_loop.available_monitors();
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
    {
        let watcher = watcher(event_loop);
        let queried = watcher.as_deref().map(Watcher::queried).unwrap_or_default();
        let displays = monitors.map(|m| {
            let display = display(&m, wayland, primary.as_ref());
            match display.native_id() {
                Some(crate::NativeDisplayId::Wayland(output)) => corrected(display, queried.get(&output)),
                _ => display,
            }
        }).collect();
        (displays, watcher.map_or(Outputs::Unasked, Outputs::Watched))
    }
    #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
    (monitors.map(|m| display(&m, wayland, primary.as_ref())).collect(), Outputs::Unasked)
}

/// The outputs the compositor has described, in winit's order, each with the monitor handle an
/// overlay targets it by. Incomplete while winit doesn't list one of them yet: its own queue
/// handles the same read a turn later when another read (a swapchain acquire, a roundtrip)
/// brought the events in.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
fn watched(event_loop: &ActiveEventLoop, watcher: &Watcher) -> (Vec<Display>, bool) {
    use winit::platform::wayland::MonitorHandleExtWayland;
    let logical = watcher.logical();
    let displays: Vec<Display> = event_loop.available_monitors().filter_map(|m| {
        let output = watcher.output(m.native_id())?;
        let mut display = corrected(display(&m, true, None), logical.then_some(&output.raw));
        if output.name.is_some() { display.name.clone_from(&output.name); }
        Some(display)
    }).collect();
    let settled = watcher.described().count() == displays.len();
    (displays, settled)
}

/// One app's link, and where its window host sits.
struct Linked {
    slot: usize,
    app: AppLink,
    /// Its handler heard the list the state holds.
    heard: bool,
}

/// What `Windows` keeps for `App::on_displays_changed` and the overlays added while apps run.
#[derive(Default)]
pub(super) struct Displays {
    links: Vec<Linked>,
    outputs: Outputs,
    state: State,
}

impl Displays {
    /// Takes the links of the apps whose window hosts just started.
    pub(super) fn link(&mut self, links: Vec<(usize, AppLink)>) {
        self.links.extend(links.into_iter().map(|(slot, app)| Linked { slot, app, heard: false }));
    }

    /// Keeps what `at_launch` asked the compositor with; the first turn drops it again when no
    /// handler listens.
    pub(super) fn launched(&mut self, outputs: Outputs) {
        self.outputs = outputs;
    }

    /// Forgets the apps whose window hosts are gone, their handlers with them.
    pub(super) fn prune(&mut self, hosts: &[Option<Box<dyn AnyHost>>]) {
        self.links.retain(|link| hosts.get(link.slot).is_some_and(Option::is_some));
    }

    /// The overlays the `AppHandle` of the app in `slot` asked for since the last call.
    pub(super) fn take_added(&self, slot: usize) -> Vec<Box<dyn PendingOverlay>> {
        let mut added = Vec::new();
        if let Some(link) = self.links.iter().find(|link| link.slot == slot) {
            link.app.take_added(&mut added);
        }
        added
    }

    /// One turn of the event loop: handles what the compositor said of its outputs, and tells
    /// the handlers a list they haven't heard (every one a new list, a new one the list as it
    /// is). Returns the overlays they added.
    pub(super) fn turn(&mut self, event_loop: &ActiveEventLoop) -> Vec<Box<dyn PendingOverlay>> {
        let mut added = Vec::new();
        if !self.links.iter().any(|link| link.app.listens()) {
            // No one listens: the outputs go unfollowed until someone does.
            if !matches!(self.outputs, Outputs::Unasked) {
                (self.outputs, self.state) = Default::default();
            }
            return added;
        }
        if matches!(self.outputs, Outputs::Unasked) {
            self.outputs = Outputs::ask(event_loop);
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        if let Outputs::Watched(watcher) = &mut self.outputs && watcher.dispatch() {
            let _ = step(&mut self.state, Event::OutputsChanged);
        }
        let mut notify = false;
        if matches!(self.state, State::Stale(_)) {
            let (displays, settled) = self.outputs.list(event_loop);
            for effect in step(&mut self.state, Event::Looked { displays, settled }) {
                match effect {
                    Effect::Notify => notify = true,
                }
            }
        }
        if let State::Current(displays) = &self.state {
            for link in self.links.iter_mut().filter(|link| notify || !link.heard) {
                link.heard = true;
                link.app.displays_changed(displays, &mut added);
            }
        }
        added
    }

    /// Whether the outputs are followed on winit's display connection, so the loop must wake
    /// on each read of it.
    pub(super) fn watching(&self) -> bool {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        return matches!(self.outputs, Outputs::Watched(_));
        #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
        false
    }

    /// Whether a read this turn already queued output events for the next turn to handle.
    pub(super) fn pending(&self) -> bool {
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
        return matches!(&self.outputs, Outputs::Watched(watcher) if watcher.has_pending_events());
        #[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))))]
        false
    }

    /// The loop ends: the watcher goes while winit's display connection is still open.
    pub(super) fn close(&mut self) {
        *self = Displays::default();
    }
}

#[cfg(test)]
#[path = "displays_tests.rs"]
mod tests;
