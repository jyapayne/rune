//! Drags out of Rune windows on the shared Wayland controller: a `wl_data_source` handed to
//! `wl_data_device.start_drag` with the press serial from this client's own `wl_pointer`, a
//! `wl_shm` drag icon, payloads written on worker threads, and the session stepped by
//! `source::step` until the compositor finishes or cancels it.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};

use super::{CONTROLLERS, Controller, QueueState, wayland_surface};
use crate::drag_out::{DragImage, DragItem, DragOutStart, DragOutcome};
use crate::platform::wayland_drop::source::{self as dnd, IconPlan, Pointer, PointerEvent};

type Done = Box<dyn FnOnce(DragOutcome)>;

/// The drag this client is running: at most one, as one pointer carries it.
pub(super) struct ActiveDrag {
    seat: u32,
    source: WlDataSource,
    icon: Option<Icon>,
    item: Arc<DragItem>,
    state: dnd::State,
    done: Done,
}

impl ActiveDrag {
    fn end(self, outcome: DragOutcome) {
        self.source.destroy();
        drop(self.icon);
        (self.done)(outcome);
    }
}

/// The drag icon: a surface showing one `wl_shm` buffer.
struct Icon {
    surface: WlSurface,
    buffer: WlBuffer,
    plan: IconPlan,
}

impl Icon {
    #[allow(clippy::too_many_arguments)]
    fn new(compositor: &WlCompositor, shm: &WlShm, qh: &QueueHandle<QueueState>, picture: &DragImage, bounds: [f32; 4], press: (f32, f32), pointer: (f32, f32), scale: f64) -> Option<Self> {
        let rgba = crate::image::png_rgba8(picture.png_data()).ok()?;
        let natural = (rgba.width() as f32 / scale as f32, rgba.height() as f32 / scale as f32);
        let plan = dnd::icon_plan(picture.frame(bounds, press, natural), pointer, scale);
        let pixels = dnd::icon_pixels(rgba, plan);
        let mut file = File::from(memfd_create("rune-drag-icon", MemfdFlags::CLOEXEC).ok()?);
        file.write_all(&pixels).ok()?;
        let (w, h) = (i32::try_from(plan.width).ok()?, i32::try_from(plan.height).ok()?);
        let pool = shm.create_pool(file.as_fd(), i32::try_from(pixels.len()).ok()?, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, Format::Argb8888, qh, ());
        // The buffer keeps the pool's memory mapped.
        pool.destroy();
        Some(Self { surface: compositor.create_surface(qh, ()), buffer, plan })
    }

    /// Shows the picture once `start_drag` gave the surface its role, its top-left where the
    /// element started relative to the pointer.
    fn show(&self) {
        if self.surface.version() >= 3 {
            self.surface.set_buffer_scale(self.plan.scale);
        }
        self.surface.attach(Some(&self.buffer), self.plan.offset.0, self.plan.offset.1);
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        self.surface.destroy();
        self.buffer.destroy();
    }
}

/// Starts a drag out of `window` for `start`, from inside the pointer move that crossed the
/// threshold while the button is still held. `done` runs once with how it ended: `Cancelled`
/// at once off Wayland, without a held press on the window, or before the controller is up.
pub(crate) fn start_drag(window: &winit::window::Window, start: DragOutStart, picture: Option<DragImage>, done: Done) {
    let scale = window.scale_factor();
    let Some((display, surface)) = wayland_surface(window) else {
        done(DragOutcome::Cancelled);
        return;
    };
    CONTROLLERS.with(|map| match map.borrow_mut().get_mut(&display) {
        Some(controller) => controller.start_drag(surface, start, picture, scale, done),
        None => done(DragOutcome::Cancelled),
    });
}

pub(super) fn release_pointer(pointer: WlPointer, backend: &Backend) {
    if pointer.version() >= 3 {
        pointer.release();
    } else {
        backend.destroy_object(&pointer.id()).ok();
    }
}

impl Controller {
    fn start_drag(&mut self, origin: ObjectId, start: DragOutStart, picture: Option<DragImage>, scale: f64, done: Done) {
        // The press that armed this drag came in with the events winit just read; take it in.
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            done(DragOutcome::Cancelled);
            return;
        }
        self.state.end_drag(dnd::Event::Gone);
        let qh = self.queue.handle();
        self.state.start_drag(&self.conn, &qh, origin, start, picture, scale, done);
        if self.state.needs_flush {
            self.state.needs_flush = false;
            // A failed flush means the connection is gone; the next pump notices.
            self.queue.flush().ok();
        }
    }
}

impl QueueState {
    #[allow(clippy::too_many_arguments)]
    fn start_drag(&mut self, conn: &Connection, qh: &QueueHandle<Self>, origin: ObjectId, start: DragOutStart, picture: Option<DragImage>, scale: f64, done: Done) {
        let surface = origin.protocol_id();
        let grab = self.seats.iter().find_map(|(&name, seat)| seat.pointing.grab(surface).map(|g| (name, seat, g)));
        let (Some((seat_name, seat, (serial, pointer))), Some(manager), Ok(origin)) = (grab, &self.manager, WlSurface::from_id(conn, origin)) else {
            done(DragOutcome::Cancelled);
            return;
        };
        let source = manager.create_data_source(qh, ());
        for mime in dnd::mime_types(&start.item) {
            source.offer(mime.to_owned());
        }
        if source.version() >= 3 {
            source.set_actions(DndAction::from_bits_truncate(dnd::actions(&start.item)));
        }
        let icon = picture.and_then(|picture| {
            Icon::new(self.compositor.as_ref()?, self.shm.as_ref()?, qh, &picture, start.bounds, start.press, pointer, scale)
        });
        seat.device.start_drag(Some(&source), &origin, icon.as_ref().map(|i| &i.surface), serial);
        if let Some(icon) = &icon {
            icon.show();
        }
        self.drag = Some(ActiveDrag { seat: seat_name, source, icon, item: Arc::new(start.item), state: dnd::State::started(), done });
        self.needs_flush = true;
    }

    /// Steps the running drag; when it ends, tears it down and reports how.
    pub(super) fn end_drag(&mut self, event: dnd::Event) {
        let Some(drag) = self.drag.as_mut() else { return };
        let Some(dnd::Effect::End(outcome)) = dnd::step(&mut drag.state, event).into_iter().last() else { return };
        if let Some(drag) = self.drag.take() {
            drag.end(outcome);
            self.needs_flush = true;
        }
    }

    pub(super) fn drag_seat_gone(&mut self, seat: u32) {
        if self.drag.as_ref().is_some_and(|d| d.seat == seat) {
            self.end_drag(dnd::Event::Gone);
        }
    }

    /// Whether this app's windows take the drop of what's being dragged now: anything from
    /// another app, and this app's own drag only with [`DragItem::within_app`].
    pub(super) fn own_drag_drops_here(&self) -> bool {
        self.drag.as_ref().is_none_or(|d| d.item.drops_within_app())
    }

    pub(super) fn seat_capabilities(&mut self, seat: &WlSeat, name: u32, caps: wl_seat::Capability, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.tearing_down {
            return;
        }
        let Some(dev) = self.seats.get_mut(&name) else { return };
        let has_pointer = caps.contains(wl_seat::Capability::Pointer);
        match (has_pointer, dev.pointer.take()) {
            (true, pointer) => dev.pointer = Some(pointer.unwrap_or_else(|| seat.get_pointer(qh, name))),
            (false, Some(pointer)) => {
                release_pointer(pointer, &conn.backend());
                dev.pointing = Pointer::Away;
            }
            (false, None) => return,
        }
        self.needs_flush = true;
    }
}

impl Dispatch<WlPointer, u32> for QueueState {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, seat: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if state.tearing_down {
            return;
        }
        let event = match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                PointerEvent::Enter { surface: surface.id().protocol_id(), at: (surface_x as f32, surface_y as f32) }
            }
            wl_pointer::Event::Leave { .. } => PointerEvent::Leave,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => PointerEvent::Motion((surface_x as f32, surface_y as f32)),
            wl_pointer::Event::Button { serial, state: WEnum::Value(ButtonState::Pressed), .. } => PointerEvent::Press { serial },
            wl_pointer::Event::Button { state: WEnum::Value(ButtonState::Released), .. } => PointerEvent::Release,
            _ => return,
        };
        if let Some(dev) = state.seats.get_mut(seat) {
            dev.pointing.step(event);
        }
    }
}

impl Dispatch<WlDataSource, ()> for QueueState {
    fn event(state: &mut Self, source: &WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(drag) = state.drag.as_ref().filter(|d| d.source == *source) else { return };
        let event = match event {
            wl_data_source::Event::Send { mime_type, fd } => return send(drag.item.clone(), mime_type, fd),
            wl_data_source::Event::Action { dnd_action: WEnum::Value(action) } => dnd::Event::Action(action.bits()),
            wl_data_source::Event::DndDropPerformed => dnd::Event::DropPerformed,
            wl_data_source::Event::DndFinished => dnd::Event::Finished,
            wl_data_source::Event::Cancelled => dnd::Event::Cancelled,
            _ => return,
        };
        state.end_drag(event);
    }
}

/// Writes the item's bytes for `mime` into the target's pipe on a worker thread, so a slow or
/// stalled reader never blocks the UI thread.
fn send(item: Arc<DragItem>, mime: String, fd: OwnedFd) {
    // A drag that can't spawn a writer sends nothing: the target reads an empty pipe.
    std::thread::Builder::new().name("rune-wayland-drag-send".into()).spawn(move || {
        if let Some(bytes) = dnd::payload(&item, &mime) {
            // The target may close the pipe early (it took what it needed, or gave up).
            File::from(fd).write_all(&bytes).ok();
        }
    }).ok();
}

delegate_noop!(QueueState: WlCompositor);
delegate_noop!(QueueState: WlShmPool);
delegate_noop!(QueueState: ignore WlShm);
delegate_noop!(QueueState: ignore WlBuffer);
delegate_noop!(QueueState: ignore WlSurface);
