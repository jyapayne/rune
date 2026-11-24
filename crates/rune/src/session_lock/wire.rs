//! The lock's Wayland objects: `ext_session_lock_manager_v1` and the `ext_session_lock_v1` it
//! made, on winit's display connection in a private event queue of their own.
//!
//! Lock surfaces live on their overlays' own queues (`platform::layer_shell`), each asked for
//! through the lock object (`get_lock_surface`), so only `locked` and `finished` arrive here.
//!
//! A `Wire` is never dropped while its lock may hold the session: the registry keeps it until
//! `unlock_and_destroy` or `destroy` went out, and keeps it for good otherwise (the process's
//! end closes the connection, and the compositor keeps the session locked).

use std::ptr::NonNull;

use wayland_backend::client::Backend;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1;
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1::{self, ExtSessionLockV1};

use super::machine::Event;

#[derive(Default)]
struct Heard {
    events: Vec<Event>,
    /// The `wl_display.sync` behind the last request answered: the compositor has read it.
    synced: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for Heard {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtSessionLockManagerV1, ()> for Heard {
    fn event(_: &mut Self, _: &ExtSessionLockManagerV1, _: <ExtSessionLockManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtSessionLockV1, ()> for Heard {
    fn event(heard: &mut Self, _: &ExtSessionLockV1, event: ext_session_lock_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_session_lock_v1::Event::Locked => heard.events.push(Event::Locked),
            ext_session_lock_v1::Event::Finished => heard.events.push(Event::Finished),
            _ => {}
        }
    }
}

pub(crate) struct Wire {
    conn: Connection,
    queue: EventQueue<Heard>,
    heard: Heard,
    manager: ExtSessionLockManagerV1,
    lock: ExtSessionLockV1,
}

impl Wire {
    /// Sends `lock` on `display`. None when the compositor has no session-lock manager or the
    /// connection failed: nothing was asked.
    #[allow(unsafe_code)]
    pub(crate) fn lock(display: NonNull<std::ffi::c_void>) -> Option<Wire> {
        // SAFETY: winit owns this `wl_display` for the life of the event loop, and a foreign
        // backend never closes it. A wire outlives the loop only where its lock may still hold
        // the session, and then it is never used again (`super::Live`).
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        Self::lock_on(Connection::from_backend(backend))
    }

    /// `lock` on a connection of the caller's: the tests' fake compositor.
    pub(crate) fn lock_on(conn: Connection) -> Option<Wire> {
        let (globals, queue) = registry_queue_init::<Heard>(&conn).ok()?;
        let qh = queue.handle();
        let Ok(manager) = globals.bind::<ExtSessionLockManagerV1, _, _>(&qh, 1..=1, ()) else {
            let registry = globals.registry().id();
            conn.backend().destroy_object(&registry).ok();
            conn.flush().ok();
            return None;
        };
        let lock = manager.lock(&qh, ());
        conn.flush().ok()?;
        Some(Wire { conn, queue, heard: Heard::default(), manager, lock })
    }

    /// What the compositor said since the last call.
    pub(crate) fn dispatch(&mut self) -> Vec<Event> {
        let _ = self.queue.dispatch_pending(&mut self.heard);
        self.conn.flush().ok();
        std::mem::take(&mut self.heard.events)
    }

    /// True when libwayland already queued events for this queue (a read elsewhere in the turn).
    pub(crate) fn has_pending_events(&self) -> bool {
        self.queue.prepare_read().is_none()
    }

    /// The lock object, for `get_lock_surface` sent through an overlay's own connection.
    pub(crate) fn lock_object(&self) -> &ExtSessionLockV1 {
        &self.lock
    }

    /// `unlock_and_destroy`, then a `wl_display.sync`: the compositor reads the unlock before
    /// anything this client sends after it.
    pub(crate) fn unlock(&mut self) {
        self.lock.unlock_and_destroy();
        self.close();
    }

    /// `destroy`, for a lock that never locked.
    pub(crate) fn destroy(&mut self) {
        self.lock.destroy();
        self.close();
    }

    /// Lets the manager go and fences what was sent with a `wl_display.sync`, which `closed`
    /// waits for before the wire may drop.
    fn close(&mut self) {
        self.manager.destroy();
        let _ = self.conn.display().sync(&self.queue.handle(), ());
        self.conn.flush().ok();
    }

    /// The compositor has read the last request (`unlock` or `destroy`): nothing is left on
    /// this queue, and the wire may drop.
    pub(crate) fn closed(&self) -> bool {
        self.heard.synced
    }
}

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for Heard {
    fn event(heard: &mut Self, _: &wayland_client::protocol::wl_callback::WlCallback, event: wayland_client::protocol::wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if matches!(event, wayland_client::protocol::wl_callback::Event::Done { .. }) {
            heard.synced = true;
        }
    }
}
