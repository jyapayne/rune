//! A compositor for `capture`'s tests: the capture globals over a socket pair, on a thread, with
//! pictures the test controls. A picture's pixel `(x, y)` is B=x, G=y, R=its generation, A=255
//! (`x` and `y` modulo 256), so a test reads back where every pixel came from and which content
//! it shows. Captures after a session's first wait for `Command::Change`, as a compositor waits
//! for damage.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1};
use wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1};
use wayland_protocols::ext::image_capture_source::v1::server::ext_foreign_toplevel_image_capture_source_manager_v1::{self as toplevel_sources, ExtForeignToplevelImageCaptureSourceManagerV1};
use wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1};
use wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::{self as output_sources, ExtOutputImageCaptureSourceManagerV1};
use wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason};
use wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1};
use wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1};
use wayland_protocols_hyprland::toplevel_export::v1::server::hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1};
use wayland_protocols_hyprland::toplevel_export::v1::server::hyprland_toplevel_export_manager_v1::{self, HyprlandToplevelExportManagerV1};
use wayland_protocols_hyprland::toplevel_mapping::v1::server::hyprland_toplevel_mapping_manager_v1::{self, HyprlandToplevelMappingManagerV1};
use wayland_protocols_hyprland::toplevel_mapping::v1::server::hyprland_toplevel_window_mapping_handle_v1::{self, HyprlandToplevelWindowMappingHandleV1};
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::wl_buffer::{self, WlBuffer};
use wayland_server::protocol::wl_output::{self, WlOutput};
use wayland_server::protocol::wl_shm::{self, Format, WlShm};
use wayland_server::protocol::wl_shm_pool::{self, WlShmPool};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource};

/// The output every fake compositor has.
pub(super) const OUTPUT: &str = "FAKE-1";

/// Which capture globals the compositor offers.
#[derive(Clone, Copy)]
pub(super) struct Offers {
    pub(super) ext: bool,
    pub(super) list: bool,
    pub(super) wlr: bool,
    pub(super) hyprland_export: bool,
}

impl Offers {
    pub(super) const ALL: Offers = Offers { ext: true, list: true, wlr: true, hyprland_export: true };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Key {
    Output,
    Window(u64),
}

pub(super) enum Command {
    /// The picture's content changes.
    Change(Key),
    Resize(Key, [u32; 2]),
    /// A window appears in the list.
    Open(u64, [u32; 2]),
    Close(u64),
}

/// What the compositor saw, for the test to read.
#[derive(Clone, Debug, Default)]
pub(super) struct Log {
    /// Captures asked for (ext `capture`, wlr and Hyprland `copy`).
    pub(super) asked: u32,
    /// Frames sent ready.
    pub(super) ready: u32,
    /// Frames the client destroyed while they waited for a change.
    pub(super) given_up: u32,
}

pub(super) struct Fake {
    tx: Option<Sender<Command>>,
    log: Arc<Mutex<Log>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    pub(super) fn send(&self, command: Command) {
        self.tx.as_ref().expect("running").send(command).expect("the compositor runs");
    }

    pub(super) fn log(&self) -> Log {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Picture {
    size: [u32; 2],
    generation: u8,
}

/// A buffer the client made in its shm pool.
struct ShmBuffer {
    file: Arc<File>,
    offset: u32,
    size: [u32; 2],
    stride: u32,
}

impl ShmBuffer {
    /// Writes `picture`, rows bottom to top when `y_invert`.
    fn paint(&self, picture: &Picture, y_invert: bool) {
        let [w, h] = self.size;
        let mut bytes = vec![0xEE; (self.stride * h) as usize];
        for y in 0..h {
            let row = if y_invert { h - 1 - y } else { y };
            for x in 0..w {
                let at = (row * self.stride + x * 4) as usize;
                bytes[at..at + 4].copy_from_slice(&[x as u8, y as u8, picture.generation, 255]);
            }
        }
        self.file.write_all_at(&bytes, u64::from(self.offset)).expect("the client's memfd takes the picture");
    }
}

struct ExtSession {
    resource: ExtImageCopyCaptureSessionV1,
    key: Key,
    delivered: u32,
    /// The content changed since the last frame.
    changed: bool,
}

/// What a frame was given, and the session it belongs to.
#[derive(Default)]
struct FrameData {
    buffer: Mutex<Option<WlBuffer>>,
}

enum Waiting {
    Ext(ExtImageCopyCaptureFrameV1, usize),
    Wlr(ZwlrScreencopyFrameV1, WlBuffer),
    Hyprland(HyprlandToplevelExportFrameV1, WlBuffer, Key),
}

struct Server {
    dh: DisplayHandle,
    pictures: HashMap<Key, Picture>,
    sessions: Vec<ExtSession>,
    waiting: Vec<Waiting>,
    lists: Vec<(Client, ExtForeignToplevelListV1)>,
    handles: Vec<(ExtForeignToplevelHandleV1, u64)>,
    log: Arc<Mutex<Log>>,
}

struct NoData;

impl ClientData for NoData {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// A compositor offering `offers`, with one 6×4 output and the `windows` listed, and the
/// client end of its socket.
pub(super) fn start(offers: Offers, windows: &[(u64, [u32; 2])]) -> (Fake, UnixStream) {
    let (server_end, client_end) = UnixStream::pair().expect("a socket pair");
    let (tx, rx) = mpsc::channel();
    let log = Arc::new(Mutex::new(Log::default()));
    let (thread_log, windows) = (log.clone(), windows.to_vec());
    let thread = std::thread::spawn(move || serve(offers, windows, server_end, rx, thread_log));
    (Fake { tx: Some(tx), log, thread: Some(thread) }, client_end)
}

fn serve(offers: Offers, windows: Vec<(u64, [u32; 2])>, stream: UnixStream, rx: Receiver<Command>, log: Arc<Mutex<Log>>) {
    let mut display: Display<Server> = Display::new().expect("a display");
    let dh = display.handle();
    dh.create_global::<Server, WlShm, ()>(1, ());
    dh.create_global::<Server, WlOutput, ()>(4, ());
    if offers.ext {
        dh.create_global::<Server, ExtImageCopyCaptureManagerV1, ()>(1, ());
        dh.create_global::<Server, ExtOutputImageCaptureSourceManagerV1, ()>(1, ());
        dh.create_global::<Server, ExtForeignToplevelImageCaptureSourceManagerV1, ()>(1, ());
    }
    if offers.list {
        dh.create_global::<Server, ExtForeignToplevelListV1, ()>(1, ());
        dh.create_global::<Server, HyprlandToplevelMappingManagerV1, ()>(1, ());
    }
    if offers.wlr {
        dh.create_global::<Server, ZwlrScreencopyManagerV1, ()>(3, ());
    }
    if offers.hyprland_export {
        dh.create_global::<Server, HyprlandToplevelExportManagerV1, ()>(2, ());
    }
    let mut pictures = HashMap::from([(Key::Output, Picture { size: [6, 4], generation: 0 })]);
    for (address, size) in windows {
        pictures.insert(Key::Window(address), Picture { size, generation: 0 });
    }
    let mut server = Server { dh: dh.clone(), pictures, sessions: Vec::new(), waiting: Vec::new(), lists: Vec::new(), handles: Vec::new(), log };
    display.handle().insert_client(stream, Arc::new(NoData)).expect("the client joins");
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(command) => server.command(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if display.dispatch_clients(&mut server).is_err() {
            return;
        }
        let _ = display.flush_clients();
    }
}

impl Server {
    fn command(&mut self, command: Command) {
        match command {
            Command::Change(key) => {
                if let Some(picture) = self.pictures.get_mut(&key) {
                    picture.generation = picture.generation.wrapping_add(1);
                }
                for session in self.sessions.iter_mut().filter(|session| session.key == key) {
                    session.changed = true;
                }
                let waiting = std::mem::take(&mut self.waiting);
                for frame in waiting {
                    self.retry(frame, key);
                }
            }
            Command::Resize(key, size) => {
                if let Some(picture) = self.pictures.get_mut(&key) {
                    picture.size = size;
                }
                for session in self.sessions.iter().filter(|session| session.key == key) {
                    constraints(&session.resource, size);
                }
                // A frame waiting with a buffer of the old size can't be copied.
                let waiting = std::mem::take(&mut self.waiting);
                for frame in waiting {
                    match frame {
                        Waiting::Ext(frame, session) if self.sessions[session].key == key => frame.failed(FailureReason::BufferConstraints),
                        other => self.waiting.push(other),
                    }
                }
            }
            Command::Open(address, size) => {
                self.pictures.insert(Key::Window(address), Picture { size, generation: 0 });
                let lists: Vec<_> = self.lists.iter().map(|(client, list)| (client.clone(), list.clone())).collect();
                for (client, list) in lists {
                    self.announce(&client, &list, address);
                }
            }
            Command::Close(address) => {
                self.pictures.remove(&Key::Window(address));
                for (handle, _) in self.handles.iter().filter(|(_, a)| *a == address) {
                    handle.closed();
                }
                for session in self.sessions.iter().filter(|session| session.key == Key::Window(address)) {
                    session.resource.stopped();
                }
            }
        }
    }

    /// A frame that waited for a change to `changed`: copied now if it was its source.
    fn retry(&mut self, frame: Waiting, changed: Key) {
        match frame {
            Waiting::Ext(frame, session) if self.sessions[session].key == changed => self.ext_capture(&frame, session),
            Waiting::Wlr(frame, buffer) if changed == Key::Output => self.wlr_copy(&frame, &buffer),
            Waiting::Hyprland(frame, buffer, key) if key == changed => self.hyprland_copy(&frame, &buffer, key),
            other => self.waiting.push(other),
        }
    }

    fn announce(&mut self, client: &Client, list: &ExtForeignToplevelListV1, address: u64) {
        let Ok(handle) = client.create_resource::<ExtForeignToplevelHandleV1, (), Server>(&self.dh, list.version(), ()) else { return };
        list.toplevel(&handle);
        handle.title(format!("window {address:x}"));
        handle.app_id("fake".into());
        handle.done();
        self.handles.push((handle, address));
    }

    fn ext_capture(&mut self, frame: &ExtImageCopyCaptureFrameV1, session: usize) {
        let Some(picture) = self.pictures.get(&self.sessions[session].key) else {
            frame.failed(FailureReason::Stopped);
            return;
        };
        let session_rec = &mut self.sessions[session];
        if session_rec.delivered > 0 && !session_rec.changed {
            self.waiting.push(Waiting::Ext(frame.clone(), session));
            return;
        }
        let (_, data) = frame.data::<(usize, FrameData)>().expect("frame data");
        let buffer = data.buffer.lock().unwrap().clone().expect("a buffer is attached before capture");
        let shm = buffer.data::<ShmBuffer>().expect("a shm buffer");
        if shm.size != picture.size {
            frame.failed(FailureReason::BufferConstraints);
            return;
        }
        shm.paint(picture, false);
        session_rec.delivered += 1;
        session_rec.changed = false;
        frame.presentation_time(0, 1, 0);
        frame.ready();
        self.log.lock().unwrap().ready += 1;
    }

    fn wlr_copy(&mut self, frame: &ZwlrScreencopyFrameV1, buffer: &WlBuffer) {
        let picture = &self.pictures[&Key::Output];
        buffer.data::<ShmBuffer>().expect("a shm buffer").paint(picture, true);
        frame.flags(zwlr_screencopy_frame_v1::Flags::YInvert);
        frame.ready(0, 2, 0);
        self.log.lock().unwrap().ready += 1;
    }

    fn hyprland_copy(&mut self, frame: &HyprlandToplevelExportFrameV1, buffer: &WlBuffer, key: Key) {
        let Some(picture) = self.pictures.get(&key) else { return };
        buffer.data::<ShmBuffer>().expect("a shm buffer").paint(picture, false);
        frame.flags(hyprland_toplevel_export_frame_v1::Flags::empty());
        frame.ready(0, 3, 0);
        self.log.lock().unwrap().ready += 1;
    }
}

fn constraints(session: &ExtImageCopyCaptureSessionV1, [w, h]: [u32; 2]) {
    session.buffer_size(w, h);
    session.shm_format(Format::Abgr8888);
    session.shm_format(Format::Argb8888);
    session.done();
}

macro_rules! global {
    ($iface:ty) => {
        impl GlobalDispatch<$iface, ()> for Server {
            fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<$iface>, _: &(), init: &mut DataInit<'_, Self>) {
                init.init(resource, ());
            }
        }
    };
}

global!(WlShm);
global!(ExtImageCopyCaptureManagerV1);
global!(ExtOutputImageCaptureSourceManagerV1);
global!(ExtForeignToplevelImageCaptureSourceManagerV1);
global!(HyprlandToplevelMappingManagerV1);
global!(ZwlrScreencopyManagerV1);
global!(HyprlandToplevelExportManagerV1);

impl GlobalDispatch<WlOutput, ()> for Server {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<WlOutput>, _: &(), init: &mut DataInit<'_, Self>) {
        let output = init.init(resource, ());
        output.name(OUTPUT.into());
        output.done();
    }
}

impl GlobalDispatch<ExtForeignToplevelListV1, ()> for Server {
    fn bind(state: &mut Self, _: &DisplayHandle, client: &Client, resource: New<ExtForeignToplevelListV1>, _: &(), init: &mut DataInit<'_, Self>) {
        let list = init.init(resource, ());
        let mut windows: Vec<u64> = state.pictures.keys().filter_map(|key| if let Key::Window(a) = key { Some(*a) } else { None }).collect();
        windows.sort();
        for address in windows {
            state.announce(client, &list, address);
        }
        state.lists.push((client.clone(), list));
    }
}

impl Dispatch<WlShm, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlShm, request: wl_shm::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let wl_shm::Request::CreatePool { id, fd, .. } = request {
            init.init(id, Arc::new(File::from(fd)));
        }
    }
}

impl Dispatch<WlShmPool, Arc<File>> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlShmPool, request: wl_shm_pool::Request, file: &Arc<File>, _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let wl_shm_pool::Request::CreateBuffer { id, offset, width, height, stride, .. } = request {
            init.init(id, ShmBuffer { file: file.clone(), offset: offset as u32, size: [width as u32, height as u32], stride: stride as u32 });
        }
    }
}

impl Dispatch<WlBuffer, ShmBuffer> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlBuffer, _: wl_buffer::Request, _: &ShmBuffer, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<WlOutput, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlOutput, _: wl_output::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &ExtOutputImageCaptureSourceManagerV1, request: output_sources::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let output_sources::Request::CreateSource { source, .. } = request {
            init.init(source, Key::Output);
        }
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: toplevel_sources::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let toplevel_sources::Request::CreateSource { source, toplevel_handle } = request {
            let address = state.handles.iter().find(|(handle, _)| *handle == toplevel_handle).map_or(0, |(_, address)| *address);
            init.init(source, Key::Window(address));
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, Key> for Server {
    fn request(_: &mut Self, _: &Client, _: &ExtImageCaptureSourceV1, _: ext_image_capture_source_v1::Request, _: &Key, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &ExtImageCopyCaptureManagerV1, request: ext_image_copy_capture_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let ext_image_copy_capture_manager_v1::Request::CreateSession { session, source, .. } = request {
            let key = *source.data::<Key>().expect("source data");
            let index = state.sessions.len();
            let session = init.init(session, index);
            match state.pictures.get(&key) {
                Some(picture) => constraints(&session, picture.size),
                None => session.stopped(),
            }
            state.sessions.push(ExtSession { resource: session, key, delivered: 0, changed: false });
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, usize> for Server {
    fn request(_: &mut Self, _: &Client, _: &ExtImageCopyCaptureSessionV1, request: ext_image_copy_capture_session_v1::Request, session: &usize, _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            init.init(frame, (*session, FrameData::default()));
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, (usize, FrameData)> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        (session, data): &(usize, FrameData),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => *data.buffer.lock().unwrap() = Some(buffer),
            ext_image_copy_capture_frame_v1::Request::Capture => {
                state.log.lock().unwrap().asked += 1;
                state.ext_capture(frame, *session);
            }
            ext_image_copy_capture_frame_v1::Request::Destroy => {
                let before = state.waiting.len();
                state.waiting.retain(|waiting| !matches!(waiting, Waiting::Ext(waiting, _) if waiting == frame));
                state.log.lock().unwrap().given_up += (before - state.waiting.len()) as u32;
            }
            _ => {}
        }
    }
}

impl Dispatch<HyprlandToplevelMappingManagerV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &HyprlandToplevelMappingManagerV1, request: hyprland_toplevel_mapping_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let hyprland_toplevel_mapping_manager_v1::Request::GetWindowForToplevel { handle, toplevel } = request {
            let mapping = init.init(handle, ());
            match state.handles.iter().find(|(h, _)| *h == toplevel) {
                Some((_, address)) => mapping.window_address((address >> 32) as u32, *address as u32),
                None => mapping.failed(),
            }
        }
    }
}

impl Dispatch<HyprlandToplevelWindowMappingHandleV1, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &HyprlandToplevelWindowMappingHandleV1, _: hyprland_toplevel_window_mapping_handle_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &ExtForeignToplevelListV1, _: ext_foreign_toplevel_list_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &ExtForeignToplevelHandleV1, _: ext_foreign_toplevel_handle_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &ZwlrScreencopyManagerV1, request: zwlr_screencopy_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, .. } = request {
            let frame = init.init(frame, ());
            let [w, h] = state.pictures[&Key::Output].size;
            // Rows padded past the picture, as a compositor's own buffers may be.
            frame.buffer(Format::Xrgb8888, w, h, w * 4 + 8);
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, frame: &ZwlrScreencopyFrameV1, request: zwlr_screencopy_frame_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                state.log.lock().unwrap().asked += 1;
                state.wlr_copy(frame, &buffer);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                state.log.lock().unwrap().asked += 1;
                state.waiting.push(Waiting::Wlr(frame.clone(), buffer));
            }
            _ => {}
        }
    }
}

impl Dispatch<HyprlandToplevelExportManagerV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &HyprlandToplevelExportManagerV1, request: hyprland_toplevel_export_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let hyprland_toplevel_export_manager_v1::Request::CaptureToplevel { frame, handle, .. } = request {
            // A window by its address's low 32 bits; none such: no answer at all, as Hyprland.
            let key = state.pictures.keys().copied().find(|key| matches!(key, Key::Window(a) if *a as u32 == handle));
            let frame = init.init(frame, key);
            if let Some(key) = key {
                let [w, h] = state.pictures[&key].size;
                frame.buffer(Format::Argb8888, w, h, w * 4);
                frame.buffer_done();
            }
        }
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, Option<Key>> for Server {
    fn request(state: &mut Self, _: &Client, frame: &HyprlandToplevelExportFrameV1, request: hyprland_toplevel_export_frame_v1::Request, key: &Option<Key>, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        if let (hyprland_toplevel_export_frame_v1::Request::Copy { buffer, ignore_damage }, Some(key)) = (request, key) {
            state.log.lock().unwrap().asked += 1;
            if ignore_damage != 0 {
                state.hyprland_copy(frame, &buffer, *key);
            } else {
                state.waiting.push(Waiting::Hyprland(frame.clone(), buffer, *key));
            }
        }
    }
}
