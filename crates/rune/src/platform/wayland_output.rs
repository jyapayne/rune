//! Wayland output geometry and fractional scale resolution via `wl_output` and `zxdg_output_v1`.
//!
//! winit 0.30 builds `MonitorHandle` from untransformed `wl_output.mode`, integer `wl_output.scale`,
//! and `logical_position * integer_scale`. On fractional-scale or 90/270-degree rotated outputs,
//! those values contradict `Display::native_bounds()`. Asking `wl_output` and `zxdg_output_v1`
//! on a private queue (`Watcher`, one roundtrip) recovers rotated physical dimensions, compositor
//! logical origin, and fractional scale, falling back to winit's values when
//! `zxdg_output_manager_v1` is unavailable. Kept, the watcher follows outputs as they are
//! plugged in, unplugged and reconfigured, which winit tracks without saying so
//! (`window::displays`, for `App::on_displays_changed`).

/// Rotation/reflection advertised by `wl_output.geometry`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OutputTransform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl OutputTransform {
    /// Whether this transform swaps width and height relative to the physical panel mode.
    pub(crate) fn swaps_axes(self) -> bool {
        matches!(self, Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270)
    }
}

/// Raw protocol state collected for one `wl_output` global during a single-roundtrip query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawOutput {
    /// Active `wl_output.mode` width and height in physical hardware pixels, before `transform`.
    pub mode: Option<(i32, i32)>,
    /// `wl_output.geometry` transform.
    pub transform: OutputTransform,
    /// `wl_output.geometry` position, used if `zxdg_output_v1.logical_position` is absent.
    pub geometry_position: Option<(i32, i32)>,
    /// `wl_output.scale` integer factor (v2+); protocol default is 1 when omitted.
    pub integer_scale: Option<i32>,
    /// `zxdg_output_v1.logical_position` in global compositor logical coordinates.
    pub logical_position: Option<(i32, i32)>,
    /// `zxdg_output_v1.logical_size` in global compositor logical coordinates.
    pub logical_size: Option<(i32, i32)>,
}

/// Display geometry resolved for `crate::Display` on Wayland.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResolvedOutput {
    /// Origin in global compositor logical coordinates.
    pub position: (i32, i32),
    /// Physical pixel dimensions after applying `wl_output.transform`.
    pub size: (u32, u32),
    /// Effective display scale (fractional when `zxdg_output_v1.logical_size` scales the mode).
    pub scale: f64,
}

/// Resolves `Display` position, transformed physical size, and effective scale from queried
/// Wayland output metadata, falling back to winit's values when `raw` (or individual fields)
/// are absent.
pub(crate) fn resolve(raw: Option<&RawOutput>, fallback: ResolvedOutput) -> ResolvedOutput {
    let Some(raw) = raw else {
        return fallback;
    };
    let fallback_scale = if fallback.scale > 0.0 && fallback.scale.is_finite() {
        fallback.scale
    } else {
        1.0
    };
    let integer_scale = raw
        .integer_scale
        .filter(|&s| s > 0)
        .map(f64::from)
        .unwrap_or(fallback_scale);

    let size = match raw.mode {
        Some((w, h)) if w > 0 && h > 0 => {
            let (w, h) = (w as u32, h as u32);
            if raw.transform.swaps_axes() { (h, w) } else { (w, h) }
        }
        _ => fallback.size,
    };

    let position = raw
        .logical_position
        .or(raw.geometry_position)
        .unwrap_or(fallback.position);

    let scale = match raw.logical_size {
        Some((lw, lh)) if lw > 0 && lh > 0 && size.0 > 0 && size.1 > 0 => {
            let (lw, lh) = (lw as u32, lh as u32);
            let max_physical = size.0.max(size.1);
            let min_physical = size.0.min(size.1);
            let max_logical = lw.max(lh);
            let min_logical = lw.min(lh);
            // Compositors running in physical layout mode (for example Mutter without
            // `scale-monitor-framebuffer`) advertise `logical_size == physical_size` alongside
            // an integer `wl_output.scale > 1`; prefer `wl_output.scale` so HiDPI outputs do
            // not collapse to 1.0 (matching Chromium Ozone Wayland).
            if max_physical == max_logical && min_physical == min_logical && integer_scale > 1.0 {
                integer_scale
            } else {
                // Using the longer axis matches Chromium Ozone Wayland: it keeps scale invariant
                // across 90/270-degree rotations and minimizes rounding error from integer
                // `zxdg_output_v1.logical_size`.
                f64::from(max_physical) / f64::from(max_logical)
            }
        }
        _ => integer_scale,
    };

    ResolvedOutput { position, size, scale }
}

/// One output as [`Watcher`] knows it between events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WatchedOutput {
    pub raw: RawOutput,
    /// The connector name (`DP-2`): `wl_output.name` (v4), or `zxdg_output_v1.name` from a
    /// compositor whose `wl_output` is older.
    pub name: Option<String>,
    /// The compositor has finished describing it at least once (`wl_output.done`; a version 1
    /// output never says so, and counts from its first event).
    pub described: bool,
}

impl WatchedOutput {
    /// Whether it can be listed: described, with its logical geometry too where the compositor
    /// has `zxdg_output_manager_v1` (`logical`), so a new output isn't listed half known.
    pub(crate) fn ready(&self, logical: bool) -> bool {
        self.described && (!logical || self.raw.logical_size.is_some())
    }
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod wayland {
    use std::collections::HashMap;

    use raw_window_handle::RawDisplayHandle;
    use wayland_backend::client::Backend;
    use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_output::{self, WlOutput};
    use wayland_client::protocol::wl_registry::{self, WlRegistry};
    use wayland_client::{Connection, Dispatch, DispatchError, EventQueue, Proxy, QueueHandle, WEnum};
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::{
        self, ZxdgOutputManagerV1,
    };
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

    use super::{OutputTransform, RawOutput, WatchedOutput};

    /// A `wl_output` global the watcher bound, with its `zxdg_output_v1` where there is one.
    struct Bound {
        info: WatchedOutput,
        wl: WlOutput,
        xdg: Option<ZxdgOutputV1>,
    }

    impl Bound {
        fn release(self) {
            if let Some(xdg) = self.xdg {
                xdg.destroy();
            }
            if self.wl.version() >= 3 {
                self.wl.release();
            } else if let Some(backend) = self.wl.backend().upgrade() {
                backend.destroy_object(&self.wl.id()).ok();
            }
        }
    }

    #[derive(Default)]
    struct State {
        /// Keyed by registry global name (`NativeDisplayId::Wayland`).
        outputs: HashMap<u32, Bound>,
        manager: Option<ZxdgOutputManagerV1>,
        /// An output came, went or was described anew since `Watcher::dispatch` last said so.
        changed: bool,
    }

    impl State {
        fn bind(&mut self, registry: &WlRegistry, name: u32, version: u32, qh: &QueueHandle<Self>) {
            // Events still queued when the watcher goes are drained after its registry.
            if !registry.is_alive() {
                return;
            }
            let wl = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);
            let xdg = self.manager.as_ref().map(|manager| manager.get_xdg_output(&wl, qh, name));
            // A global announced while the list was read is bound from the list and again here.
            if let Some(twice) = self.outputs.insert(name, Bound { info: WatchedOutput::default(), wl, xdg }) {
                twice.release();
            }
        }
    }

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            state: &mut Self,
            registry: &WlRegistry,
            event: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            match event {
                wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                    state.bind(registry, name, version, qh);
                }
                wl_registry::Event::GlobalRemove { name } => {
                    if let Some(gone) = state.outputs.remove(&name) {
                        gone.release();
                        state.changed = true;
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<ZxdgOutputManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZxdgOutputManagerV1,
            _: zxdg_output_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<WlOutput, u32> for State {
        fn event(
            state: &mut Self,
            output: &WlOutput,
            event: wl_output::Event,
            &global_name: &u32,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // A removed output's last events change nothing.
            let Some(bound) = state.outputs.get_mut(&global_name) else { return };
            let entry = &mut bound.info;
            match event {
                wl_output::Event::Geometry { x, y, transform, .. } => {
                    entry.raw.geometry_position = Some((x, y));
                    entry.raw.transform = map_transform(transform);
                }
                wl_output::Event::Mode { flags, width, height, .. } => {
                    let is_current = match flags {
                        WEnum::Value(bits) => bits.contains(wl_output::Mode::Current),
                        WEnum::Unknown(raw) => (raw & wl_output::Mode::Current.bits()) != 0,
                    };
                    if is_current {
                        entry.raw.mode = Some((width, height));
                    }
                }
                wl_output::Event::Scale { factor } => {
                    entry.raw.integer_scale = Some(factor);
                }
                wl_output::Event::Name { name } => {
                    entry.name = Some(name);
                }
                wl_output::Event::Done => {
                    entry.described = true;
                    state.changed = true;
                }
                _ => {}
            }
            // Version 1 has no `done`: each event stands alone.
            if output.version() < 2 {
                entry.described = true;
                state.changed = true;
            }
        }
    }

    impl Dispatch<ZxdgOutputV1, u32> for State {
        fn event(
            state: &mut Self,
            _: &ZxdgOutputV1,
            event: zxdg_output_v1::Event,
            &global_name: &u32,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let Some(bound) = state.outputs.get_mut(&global_name) else { return };
            let entry = &mut bound.info;
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    entry.raw.logical_position = Some((x, y));
                }
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    entry.raw.logical_size = Some((width, height));
                }
                zxdg_output_v1::Event::Name { name } => {
                    if entry.name.is_none() {
                        entry.name = Some(name);
                    }
                }
                // Before version 3; later ones end a change with `wl_output.done`.
                zxdg_output_v1::Event::Done => {
                    state.changed = true;
                }
                _ => {}
            }
        }
    }

    fn map_transform(transform: WEnum<wl_output::Transform>) -> OutputTransform {
        match transform {
            WEnum::Value(wl_output::Transform::Normal) => OutputTransform::Normal,
            WEnum::Value(wl_output::Transform::_90) => OutputTransform::Rotate90,
            WEnum::Value(wl_output::Transform::_180) => OutputTransform::Rotate180,
            WEnum::Value(wl_output::Transform::_270) => OutputTransform::Rotate270,
            WEnum::Value(wl_output::Transform::Flipped) => OutputTransform::Flipped,
            WEnum::Value(wl_output::Transform::Flipped90) => OutputTransform::Flipped90,
            WEnum::Value(wl_output::Transform::Flipped180) => OutputTransform::Flipped180,
            WEnum::Value(wl_output::Transform::Flipped270) => OutputTransform::Flipped270,
            _ => OutputTransform::Normal,
        }
    }

    fn destroy_globals(conn: &Connection, globals: GlobalList) {
        let registry_id = globals.registry().id();
        globals.destroy();
        conn.backend().destroy_object(&registry_id).ok();
    }

    /// Dispatches what the queue holds; how many events that was.
    fn drain_event_queue(queue: &mut EventQueue<State>, state: &mut State) -> usize {
        let mut handled = 0;
        loop {
            match queue.dispatch_pending(state) {
                Ok(0) => break,
                Ok(n) => handled += n,
                Err(DispatchError::BadMessage { .. }) => {}
                Err(_) => break,
            }
        }
        handled
    }

    /// Every `wl_output` global's mode, transform, integer scale, name and `zxdg_output_v1`
    /// logical geometry, on a private queue of winit's display connection, keyed by registry
    /// global name (`NativeDisplayId::Wayland`). Making it asks once and waits for the answer
    /// (one roundtrip); kept, it follows outputs as they come, go and change. winit reads the
    /// socket, and `dispatch` handles what that read queued here.
    pub(crate) struct Watcher {
        conn: Connection,
        globals: Option<GlobalList>,
        queue: EventQueue<State>,
        state: State,
    }

    impl Watcher {
        /// None off Wayland, or when the display connection fails.
        #[allow(unsafe_code)]
        pub(crate) fn new(display: RawDisplayHandle) -> Option<Watcher> {
            let RawDisplayHandle::Wayland(display) = display else {
                return None;
            };
            // SAFETY: winit owns this `wl_display` for the life of the event loop, which outlives
            // the watcher (`Windows::exiting` drops it); a foreign backend never closes it.
            let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
            let conn = Connection::from_backend(backend);
            let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
            let qh = queue.handle();
            let manager = globals.bind::<ZxdgOutputManagerV1, _, _>(&qh, 1..=3, ()).ok();
            let listed: Vec<(u32, u32)> = globals.contents().with_list(|list| {
                list.iter().filter(|g| g.interface == "wl_output").map(|g| (g.name, g.version)).collect()
            });
            let mut state = State { manager, ..State::default() };
            for (name, version) in listed {
                state.bind(globals.registry(), name, version, &qh);
            }
            // From here `Drop` destroys every proxy and frees the queue, also when the answer
            // never comes.
            let mut watcher = Watcher { conn, globals: Some(globals), queue, state };
            if !watcher.state.outputs.is_empty() {
                watcher.queue.roundtrip(&mut watcher.state).ok()?;
            }
            Some(watcher)
        }

        /// Handles what the compositor sent since the last call. True when an output came, went
        /// or was described anew.
        pub(crate) fn dispatch(&mut self) -> bool {
            // A new output's binds go out now rather than with winit's next flush.
            if drain_event_queue(&mut self.queue, &mut self.state) > 0 {
                self.queue.flush().ok();
            }
            std::mem::take(&mut self.state.changed)
        }

        /// Whether a read this turn (a swapchain acquire, a roundtrip) already queued events
        /// here; checked in memory, as `LayerSurface::has_pending_events` does.
        pub(crate) fn has_pending_events(&self) -> bool {
            self.queue.prepare_read().is_none()
        }

        /// Whether outputs carry compositor logical geometry (`zxdg_output_manager_v1`).
        /// Without it callers keep winit's position, size and scale.
        pub(crate) fn logical(&self) -> bool {
            self.state.manager.is_some()
        }

        /// The output with this registry name, once the compositor has described it.
        pub(crate) fn output(&self, name: u32) -> Option<&WatchedOutput> {
            let logical = self.logical();
            self.state.outputs.get(&name).map(|bound| &bound.info).filter(|output| output.ready(logical))
        }

        /// The registry names of the outputs the compositor has described.
        pub(crate) fn described(&self) -> impl Iterator<Item = u32> + '_ {
            let logical = self.logical();
            self.state.outputs.iter().filter(move |(_, bound)| bound.info.ready(logical)).map(|(name, _)| *name)
        }

        /// What the roundtrip that made the watcher learned of each output, for the launch's
        /// display list. Empty when `zxdg_output_manager_v1` is unavailable, so callers keep
        /// winit's values.
        pub(crate) fn queried(&self) -> HashMap<u32, RawOutput> {
            if !self.logical() {
                return HashMap::new();
            }
            self.state.outputs.iter().map(|(name, bound)| (*name, bound.info.raw)).collect()
        }
    }

    impl Drop for Watcher {
        /// Destroys every proxy and drains what is queued, so the private queue is freed.
        fn drop(&mut self) {
            for (_, bound) in self.state.outputs.drain() {
                bound.release();
            }
            if let Some(manager) = self.state.manager.take() {
                manager.destroy();
            }
            if let Some(globals) = self.globals.take() {
                destroy_globals(&self.conn, globals);
            }
            drain_event_queue(&mut self.queue, &mut self.state);
            self.queue.flush().ok();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
pub(crate) use wayland::Watcher;

#[cfg(test)]
#[path = "wayland_output_tests.rs"]
mod tests;
