//! The lock-surface role (`ext_session_lock_surface_v1`): asked for through the session lock's
//! object but made on the overlay's own queue, so its `configure` arrives with the rest of the
//! surface's events and steps the same mapping as a layer surface's.
//!
//! The rules differ from layer-shell's where the protocol says so: the compositor configures
//! the surface unasked, with its output's size; nothing may be committed before the first ack,
//! nor a null buffer ever, so the surface is never unmapped; and it goes when its lock ends.

use std::ptr::NonNull;

use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1;
pub(super) use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1::ExtSessionLockSurfaceV1;
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1;

use super::super::layer_shell_machine::mapping;
use super::State;

impl Dispatch<ExtSessionLockSurfaceV1, ()> for State {
    fn event(state: &mut Self, _: &ExtSessionLockSurfaceV1, event: ext_session_lock_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = event {
            state.step_mapping(mapping::Event::Configure { serial, width, height });
        }
    }
}

/// Asks `lock` for a lock surface of `surface` on `output`. The request goes through the lock
/// object, which lives on the lock's own connection, but is sent on `conn` so the new object
/// joins this surface's queue (`Connection::send_request` with this queue's object data).
pub(super) fn lock_surface(
    conn: &Connection,
    qh: &QueueHandle<State>,
    lock: &crate::SessionLock,
    surface: &WlSurface,
    output: Option<&WlOutput>,
    display: NonNull<std::ffi::c_void>,
) -> Result<ExtSessionLockSurfaceV1, &'static str> {
    let output = output.ok_or("a lock surface needs a display of its own, and this one is gone or wasn't named")?;
    let object = crate::session_lock::lock_object(lock, display).ok_or("its session lock has ended, or the compositor can't lock the session")?;
    let request = ext_session_lock_v1::Request::GetLockSurface { surface: surface.clone(), output: output.clone() };
    let data = qh.make_data::<ExtSessionLockSurfaceV1, ()>(());
    let id = conn.send_request(&object, request, Some(data)).map_err(|_| "the session lock is gone")?;
    ExtSessionLockSurfaceV1::from_id(conn, id).map_err(|_| "the compositor's lock surface is gone")
}
