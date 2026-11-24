//! A compositor for `session_lock`'s tests: `ext-session-lock-v1` over a socket pair, on a
//! thread, with outputs the test adds and a seat whose keyboard it moves. It keeps the
//! protocol's rules a real compositor enforces (a commit before the first ack, a null buffer,
//! `destroy` after `locked`, `unlock_and_destroy` before it, a second surface on an output)
//! and logs a violation instead of a request, so a test sees it.

use std::fs::File;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::{self, ExtSessionLockManagerV1};
use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1};
use wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::{self, ExtSessionLockV1};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, GlobalId};
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_compositor::{self, WlCompositor};
use wayland_server::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_server::protocol::wl_output::{self, WlOutput};
use wayland_server::protocol::wl_region::{self, WlRegion};
use wayland_server::protocol::wl_seat::{self, WlSeat};
use wayland_server::protocol::wl_surface::{self, WlSurface};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource};

/// How the compositor answers a lock.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Grant {
    /// `locked` at once.
    Now,
    /// Nothing until `Command::Locked`.
    Later,
    /// `finished` at once: another client holds a lock.
    Refuse,
}

pub(super) enum Command {
    /// A new output `name`, `width` × `height`.
    AddOutput(&'static str, u32, u32),
    Locked,
    Finished,
    /// The keyboard enters the newest lock surface, with Caps Lock on or off.
    Focus { caps_lock: bool },
}

/// What the compositor saw, in order.
pub(super) type Log = Arc<Mutex<Vec<String>>>;

pub(super) struct Fake {
    tx: Option<Sender<Command>>,
    pub(super) log: Log,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    pub(super) fn send(&self, command: Command) {
        self.tx.as_ref().expect("running").send(command).expect("the compositor runs");
    }

    /// Everything logged since the last call.
    pub(super) fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.log.lock().unwrap())
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

struct LockSurface {
    resource: ExtSessionLockSurfaceV1,
    surface: WlSurface,
    output: String,
    acked: bool,
}

struct Server {
    dh: DisplayHandle,
    grant: Grant,
    log: Log,
    outputs: Vec<(GlobalId, &'static str, u32, u32)>,
    lock: Option<ExtSessionLockV1>,
    locked: bool,
    surfaces: Vec<LockSurface>,
    keyboards: Vec<WlKeyboard>,
    /// A focus asked for before the client had a keyboard: given once it has one.
    pending_focus: Option<bool>,
    serial: u32,
}

impl Server {
    fn note(&self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }
}

struct NoData;

impl ClientData for NoData {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// A compositor with one 640 × 480 output, `FAKE-1`, and the client end of its socket.
pub(super) fn start(grant: Grant) -> (Fake, UnixStream) {
    let (server_end, client_end) = UnixStream::pair().expect("a socket pair");
    let (tx, rx) = mpsc::channel();
    let log: Log = Arc::default();
    let thread_log = log.clone();
    let thread = std::thread::spawn(move || serve(grant, server_end, rx, thread_log));
    (Fake { tx: Some(tx), log, thread: Some(thread) }, client_end)
}

fn serve(grant: Grant, stream: UnixStream, rx: Receiver<Command>, log: Log) {
    let mut display: Display<Server> = Display::new().expect("a display");
    let dh = display.handle();
    dh.create_global::<Server, WlCompositor, ()>(4, ());
    dh.create_global::<Server, WlSeat, ()>(7, ());
    dh.create_global::<Server, ExtSessionLockManagerV1, ()>(1, ());
    let first = dh.create_global::<Server, WlOutput, &'static str>(4, "FAKE-1");
    let mut server = Server { dh: dh.clone(), grant, log, outputs: vec![(first, "FAKE-1", 640, 480)], lock: None, locked: false, surfaces: Vec::new(), keyboards: Vec::new(), pending_focus: None, serial: 0 };
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
            Command::AddOutput(name, width, height) => {
                let id = self.dh.create_global::<Server, WlOutput, &'static str>(4, name);
                self.outputs.push((id, name, width, height));
            }
            Command::Locked => self.send_locked(),
            Command::Finished => {
                if let Some(lock) = &self.lock {
                    lock.finished();
                }
            }
            Command::Focus { caps_lock } => self.focus(caps_lock),
        }
    }

    fn send_locked(&mut self) {
        if let Some(lock) = &self.lock {
            lock.locked();
            self.locked = true;
        }
    }

    fn focus(&mut self, caps_lock: bool) {
        if self.keyboards.is_empty() {
            self.pending_focus = Some(caps_lock);
            return;
        }
        let Some(surface) = self.surfaces.last().map(|s| s.surface.clone()) else { return };
        let caps = if caps_lock { 1 << keymap().1 } else { 0 };
        for keyboard in &self.keyboards {
            self.serial += 1;
            keyboard.enter(self.serial, &surface, Vec::new());
            keyboard.modifiers(self.serial, 0, 0, caps, 0);
        }
    }
}

/// A US keymap as text, and the index of its Caps Lock modifier.
fn keymap() -> (String, u32) {
    use xkbcommon::xkb;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&context, "evdev", "pc105", "us", "", None, xkb::COMPILE_NO_FLAGS).expect("xkeyboard-config's US layout");
    (keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1), keymap.mod_get_index(xkb::MOD_NAME_CAPS))
}

impl GlobalDispatch<WlCompositor, ()> for Server {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<WlCompositor>, _: &(), init: &mut DataInit<'_, Self>) {
        init.init(resource, ());
    }
}

impl Dispatch<WlCompositor, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlCompositor, request: wl_compositor::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                init.init(id, Mutex::new(SurfaceState::default()));
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegion, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlRegion, _: wl_region::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

/// What a client attached to a surface and hasn't committed.
#[derive(Default)]
struct SurfaceState {
    /// `Some(true)`: a buffer; `Some(false)`: a null buffer.
    attached: Option<bool>,
}

impl Dispatch<WlSurface, Mutex<SurfaceState>> for Server {
    fn request(state: &mut Self, _: &Client, surface: &WlSurface, request: wl_surface::Request, data: &Mutex<SurfaceState>, _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => data.lock().unwrap().attached = Some(buffer.is_some()),
            wl_surface::Request::Frame { callback } => {
                init.init(callback, ());
            }
            wl_surface::Request::Commit => {
                let attached = data.lock().unwrap().attached.take();
                let Some(lock_surface) = state.surfaces.iter().find(|s| s.surface == *surface) else { return };
                let what = if !lock_surface.acked {
                    "violation: commit before the first ack"
                } else if attached == Some(false) {
                    "violation: null buffer"
                } else {
                    "commit"
                };
                let output = lock_surface.output.clone();
                state.note(format!("{what} {output}"));
            }
            wl_surface::Request::Destroy if state.surfaces.iter().any(|s| s.surface == *surface) => {
                state.note("violation: wl_surface destroyed before its lock surface");
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlCallback, _: <WlCallback as Resource>::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl GlobalDispatch<WlOutput, &'static str> for Server {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<WlOutput>, name: &&'static str, init: &mut DataInit<'_, Self>) {
        let output = init.init(resource, name);
        output.name((*name).into());
        output.done();
    }
}

impl Dispatch<WlOutput, &'static str> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlOutput, _: wl_output::Request, _: &&'static str, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl GlobalDispatch<WlSeat, ()> for Server {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<WlSeat>, _: &(), init: &mut DataInit<'_, Self>) {
        let seat = init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<WlSeat, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &WlSeat, request: wl_seat::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let wl_seat::Request::GetKeyboard { id } = request {
            let keyboard = init.init(id, ());
            let (text, _) = keymap();
            let fd = rustix::fs::memfd_create("fake-keymap", rustix::fs::MemfdFlags::CLOEXEC).expect("a memfd");
            let mut file = File::from(fd);
            file.write_all(text.as_bytes()).expect("the keymap written");
            file.write_all(&[0]).expect("its terminator");
            keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1, std::os::fd::AsFd::as_fd(&file), text.len() as u32 + 1);
            state.keyboards.push(keyboard);
            if let Some(caps_lock) = state.pending_focus.take() {
                state.focus(caps_lock);
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for Server {
    fn request(_: &mut Self, _: &Client, _: &WlKeyboard, _: wl_keyboard::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl GlobalDispatch<ExtSessionLockManagerV1, ()> for Server {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<ExtSessionLockManagerV1>, _: &(), init: &mut DataInit<'_, Self>) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &ExtSessionLockManagerV1, request: ext_session_lock_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let ext_session_lock_manager_v1::Request::Lock { id } = request {
            let lock = init.init(id, ());
            state.note("lock");
            match state.grant {
                Grant::Refuse => lock.finished(),
                Grant::Now => {
                    lock.locked();
                    state.locked = true;
                }
                Grant::Later => {}
            }
            state.lock = Some(lock);
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, _: &ExtSessionLockV1, request: ext_session_lock_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            ext_session_lock_v1::Request::GetLockSurface { id, surface, output } => {
                let name = output.data::<&'static str>().copied().unwrap_or("?");
                let lock_surface = init.init(id, ());
                if state.surfaces.iter().any(|s| s.output == name) {
                    state.note(format!("violation: a second lock surface on {name}"));
                    return;
                }
                state.note(format!("get_lock_surface {name}"));
                let (width, height) = state.outputs.iter().find(|o| o.1 == name).map_or((1, 1), |o| (o.2, o.3));
                state.serial += 1;
                lock_surface.configure(state.serial, width, height);
                state.surfaces.push(LockSurface { resource: lock_surface, surface, output: name.to_owned(), acked: false });
            }
            ext_session_lock_v1::Request::UnlockAndDestroy => {
                state.note(if state.locked { "unlock_and_destroy" } else { "violation: unlock_and_destroy before locked" });
                state.locked = false;
            }
            ext_session_lock_v1::Request::Destroy => {
                state.note(if state.locked { "violation: destroy after locked" } else { "destroy" });
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for Server {
    fn request(state: &mut Self, _: &Client, resource: &ExtSessionLockSurfaceV1, request: ext_session_lock_surface_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        let Some(at) = state.surfaces.iter().position(|s| s.resource == *resource) else { return };
        match request {
            ext_session_lock_surface_v1::Request::AckConfigure { serial } => {
                state.surfaces[at].acked = true;
                let output = state.surfaces[at].output.clone();
                state.note(format!("ack {serial} {output}"));
            }
            ext_session_lock_surface_v1::Request::Destroy => {
                let gone = state.surfaces.remove(at);
                state.note(format!("lock surface destroyed {}", gone.output));
            }
            _ => {}
        }
    }
}
