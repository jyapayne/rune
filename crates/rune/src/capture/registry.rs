//! The compositor's globals a capture uses, its outputs by connector name, and its windows by
//! Hyprland address: `ext-foreign-toplevel-list-v1` lists the windows, and
//! `hyprland-toplevel-mapping-v1` answers each listed handle with its window's address, the one
//! `hyprctl clients -j` shows.

use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols_hyprland::toplevel_export::v1::client::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;
use wayland_protocols_hyprland::toplevel_mapping::v1::client::hyprland_toplevel_mapping_manager_v1::HyprlandToplevelMappingManagerV1;
use wayland_protocols_hyprland::toplevel_mapping::v1::client::hyprland_toplevel_window_mapping_handle_v1::{self, HyprlandToplevelWindowMappingHandleV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use super::client::Client;
use super::{Protocols, WindowAddress};

/// The capture globals bound, each where the compositor offers it.
#[derive(Default)]
pub(super) struct Globals {
    /// Whether `ext-*` capture is used at all.
    ext: bool,
    pub(super) shm: Option<WlShm>,
    pub(super) ext_copy: Option<ExtImageCopyCaptureManagerV1>,
    pub(super) ext_outputs: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub(super) ext_toplevels: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    list: Option<ExtForeignToplevelListV1>,
    mapping: Option<HyprlandToplevelMappingManagerV1>,
    pub(super) wlr: Option<ZwlrScreencopyManagerV1>,
    pub(super) hyprland_export: Option<HyprlandToplevelExportManagerV1>,
}

impl Globals {
    pub(super) fn new(ext: bool) -> Globals {
        Globals { ext, ..Globals::default() }
    }

    pub(super) fn protocols(&self) -> Protocols {
        Protocols {
            ext_image_copy_capture: self.ext_copy.is_some(),
            ext_output_source: self.ext_outputs.is_some(),
            ext_toplevel_source: self.ext_toplevels.is_some(),
            ext_toplevel_list: self.list.is_some(),
            hyprland_toplevel_mapping: self.mapping.is_some(),
            // Every frame goes through a shm buffer.
            wlr_screencopy: self.wlr.is_some() && self.shm.is_some(),
            hyprland_toplevel_export: self.hyprland_export.is_some() && self.shm.is_some(),
        }
    }
}

pub(super) struct Output {
    global: u32,
    pub(super) wl: WlOutput,
    pub(super) name: Option<String>,
}

pub(super) struct Toplevel {
    pub(super) handle: ExtForeignToplevelHandleV1,
    pub(super) address: Option<WindowAddress>,
    pub(super) title: String,
    pub(super) app_id: String,
}

impl Dispatch<WlRegistry, ()> for Client {
    fn event(state: &mut Self, registry: &WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                let g = &mut state.globals;
                match interface.as_str() {
                    "wl_shm" => g.shm = Some(registry.bind(name, 1, qh, ())),
                    "wl_output" if version >= 4 => {
                        let wl = registry.bind(name, 4, qh, name);
                        state.outputs.push(Output { global: name, wl, name: None });
                    }
                    "ext_image_copy_capture_manager_v1" if g.ext => g.ext_copy = Some(registry.bind(name, 1, qh, ())),
                    "ext_output_image_capture_source_manager_v1" if g.ext => g.ext_outputs = Some(registry.bind(name, 1, qh, ())),
                    "ext_foreign_toplevel_image_capture_source_manager_v1" if g.ext => g.ext_toplevels = Some(registry.bind(name, 1, qh, ())),
                    // Not capture protocols: they name windows whichever protocol captures them.
                    "ext_foreign_toplevel_list_v1" => g.list = Some(registry.bind(name, 1, qh, ())),
                    "hyprland_toplevel_mapping_manager_v1" => g.mapping = Some(registry.bind(name, 1, qh, ())),
                    "zwlr_screencopy_manager_v1" => g.wlr = Some(registry.bind(name, version.min(3), qh, ())),
                    "hyprland_toplevel_export_manager_v1" => g.hyprland_export = Some(registry.bind(name, version.min(2), qh, ())),
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(at) = state.outputs.iter().position(|output| output.global == name) {
                    let output = state.outputs.remove(at);
                    output.wl.release();
                    state.publish_outputs();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for Client {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_output::Event::Name { name } => {
                if let Some(output) = state.outputs.iter_mut().find(|output| output.global == *global) {
                    output.name = Some(name);
                }
            }
            wl_output::Event::Done => state.publish_outputs(),
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Client {
    fn event(state: &mut Self, _: &ExtForeignToplevelListV1, event: ext_foreign_toplevel_list_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            if let Some(mapping) = &state.globals.mapping {
                mapping.get_window_for_toplevel(&toplevel, qh, toplevel.clone());
            }
            state.toplevels.push(Toplevel { handle: toplevel, address: None, title: String::new(), app_id: String::new() });
        }
    }

    event_created_child!(Client, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Client {
    fn event(state: &mut Self, handle: &ExtForeignToplevelHandleV1, event: ext_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(at) = state.toplevels.iter().position(|toplevel| &toplevel.handle == handle) else { return };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => state.toplevels[at].title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => state.toplevels[at].app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Done => state.publish_windows(),
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                let toplevel = state.toplevels.remove(at);
                toplevel.handle.destroy();
                state.publish_windows();
                if let Some(address) = toplevel.address {
                    state.window_closed(address);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<HyprlandToplevelWindowMappingHandleV1, ExtForeignToplevelHandleV1> for Client {
    fn event(
        state: &mut Self,
        mapping: &HyprlandToplevelWindowMappingHandleV1,
        event: hyprland_toplevel_window_mapping_handle_v1::Event,
        handle: &ExtForeignToplevelHandleV1,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let hyprland_toplevel_window_mapping_handle_v1::Event::WindowAddress { address_hi, address } = event
            && let Some(toplevel) = state.toplevels.iter_mut().find(|toplevel| &toplevel.handle == handle)
        {
            toplevel.address = Some(WindowAddress(u64::from(address_hi) << 32 | u64::from(address)));
            state.publish_windows();
        }
        // Answered either way (an address, or failed: the window is gone).
        mapping.destroy();
    }
}

delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ExtImageCopyCaptureManagerV1);
delegate_noop!(Client: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Client: ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(Client: ExtImageCaptureSourceV1);
delegate_noop!(Client: HyprlandToplevelMappingManagerV1);
delegate_noop!(Client: ZwlrScreencopyManagerV1);
delegate_noop!(Client: HyprlandToplevelExportManagerV1);
