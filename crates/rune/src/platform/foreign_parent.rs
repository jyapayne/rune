//! Windows that belong to another app's window, as a file dialog belongs to the app that asked
//! for it. xdg-desktop-portal names that window `wayland:<handle>`, a handle the app exported
//! with xdg-foreign v2, or `x11:<window id>`.
//!
//! On Wayland each of this app's windows imports the handle (`zxdg_importer_v2`) and becomes
//! its child, so the compositor keeps it above that window and places it there, and, when
//! modal, says so (`xdg_wm_dialog_v1`). Both bind on winit's own display connection in a
//! private queue, as `activation` does. Other identifiers, compositors without the protocols,
//! and other platforms leave the windows as they are.

use std::sync::OnceLock;

/// The window the app's windows belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parent {
    pub identifier: String,
    pub modal: bool,
}

static PARENT: OnceLock<Parent> = OnceLock::new();

/// Makes every window of this app a child of another app's window, identified as
/// xdg-desktop-portal identifies it (`wayland:<xdg-foreign handle>`), and modal to it when
/// `modal`: how a portal's dialog stays with the app that asked for it. Call before `run`;
/// the first call wins. On Wayland only; other identifiers are ignored.
pub fn set_parent_window(identifier: impl Into<String>, modal: bool) {
    PARENT.set(Parent { identifier: identifier.into(), modal }).ok();
}

#[cfg_attr(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))), allow(dead_code))]
pub(crate) fn parent() -> Option<&'static Parent> {
    PARENT.get()
}

/// The xdg-foreign handle in a portal's window identifier; `None` for any other kind.
#[cfg_attr(not(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"))), allow(dead_code))]
pub(crate) fn wayland_handle(identifier: &str) -> Option<&str> {
    identifier.strip_prefix("wayland:").filter(|handle| !handle.is_empty())
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
pub(crate) use wayland::Bond;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod wayland {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::dialog::v1::client::xdg_dialog_v1::XdgDialogV1;
    use wayland_protocols::xdg::dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1;
    use wayland_protocols::xdg::foreign::zv2::client::zxdg_imported_v2::ZxdgImportedV2;
    use wayland_protocols::xdg::foreign::zv2::client::zxdg_importer_v2::ZxdgImporterV2;
    use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
    use winit::platform::wayland::WindowExtWayland;
    use winit::window::Window;

    use super::{parent, wayland_handle};

    struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }

    delegate_noop!(State: ZxdgImporterV2);
    // `destroyed`: the parent went away; the window simply has none any more.
    delegate_noop!(State: ignore ZxdgImportedV2);
    delegate_noop!(State: XdgWmDialogV1);
    delegate_noop!(State: XdgDialogV1);
    // Only made from winit's pointer, to name the toplevel; its events go to winit's queue.
    delegate_noop!(State: ignore XdgToplevel);

    /// One window's tie to its parent, kept with the window and dropped before it: dropping
    /// it unties them.
    pub(crate) struct Bond {
        queue: EventQueue<State>,
        imported: ZxdgImportedV2,
        dialog: Option<XdgDialogV1>,
    }

    impl Bond {
        /// `None` without a Wayland parent, off Wayland, or when the compositor lacks
        /// `zxdg_importer_v2`. Without `xdg_wm_dialog_v1` the window is a child, not modal.
        #[allow(unsafe_code)]
        pub fn new(window: &Window) -> Option<Bond> {
            let parent = parent()?;
            let handle = wayland_handle(&parent.identifier)?;
            let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(raw)) = (window.display_handle().ok()?.as_raw(), window.window_handle().ok()?.as_raw()) else {
                return None;
            };
            // SAFETY: winit owns this `wl_display` and keeps it alive for the event loop, which
            // outlives every window; a foreign backend never closes it.
            let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
            let conn = Connection::from_backend(backend);
            let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
            let qh = queue.handle();
            let importer = globals.bind::<ZxdgImporterV2, _, _>(&qh, 1..=1, ()).ok()?;
            // SAFETY: the pointer is the live `wl_surface` proxy winit created for this window,
            // and the bond is dropped before the window.
            let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), raw.surface.as_ptr().cast()) }.ok()?;
            let surface = WlSurface::from_id(&conn, surface_id).ok()?;
            let imported = importer.import_toplevel(handle.to_owned(), &qh, ());
            imported.set_parent_of(&surface);
            importer.destroy();
            let dialog = parent.modal.then(|| modal(&conn, &globals, &qh, window)).flatten();
            queue.flush().ok();
            Some(Bond { queue, imported, dialog })
        }
    }

    /// Marks `window` modal to its parent.
    #[allow(unsafe_code)]
    fn modal(conn: &Connection, globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<State>, window: &Window) -> Option<XdgDialogV1> {
        let wm = globals.bind::<XdgWmDialogV1, _, _>(qh, 1..=1, ()).ok()?;
        let toplevel = window.xdg_toplevel()?;
        // SAFETY: the pointer is the live `xdg_toplevel` proxy winit created for this window,
        // and the dialog object made from it is dropped, with the bond, before the window.
        let id = unsafe { ObjectId::from_ptr(XdgToplevel::interface(), toplevel.as_ptr().cast()) }.ok()?;
        let toplevel = XdgToplevel::from_id(conn, id).ok()?;
        let dialog = wm.get_xdg_dialog(&toplevel, qh, ());
        dialog.set_modal();
        wm.destroy();
        Some(dialog)
    }

    impl Drop for Bond {
        fn drop(&mut self) {
            if let Some(dialog) = self.dialog.take() {
                dialog.destroy();
            }
            self.imported.destroy();
            self.queue.flush().ok();
        }
    }
}

#[cfg(test)]
#[path = "foreign_parent_tests.rs"]
mod tests;
