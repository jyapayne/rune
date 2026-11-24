//! A Wayland layer-shell surface for overlays: `zwlr_layer_shell_v1` on winit's own display
//! connection, in a private event queue with its own seat. The surface reserves no space;
//! input regions and the rendered buffer share a commit. Global pointer observation and
//! capture exclusion are unavailable. wgpu draws through the raw `wl_surface`.

use std::ptr::NonNull;

use winit::raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle};
use wayland_backend::client::Backend;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::protocol::{wl_keyboard::WlKeyboard, wl_output::{self, WlOutput}, wl_pointer::WlPointer, wl_seat::{self, WlSeat}, wl_touch::WlTouch};
use wayland_client::{Connection, Dispatch, DispatchError, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{KeyboardInteractivity, ZwlrLayerSurfaceV1};

use super::layer_shell_machine::{connection, frame_callback, keyboard, mapping, pointer, repeat, text_input as text_input_machine, touch};
use crate::machine::Seq;
use crate::overlay::Placement;
use crate::{OverlayInput, OverlayOptions, OverlayPlacement};

mod compose;
mod dead;
mod keys;
mod map;
mod pointing;
mod text_input;
mod watch;
pub(crate) use dead::display_error;
use dead::{classify_turn_error, format_wayland_error};
use pointing::{Over, shape};
pub(crate) use super::layer_shell_machine::mapping::Gone;
pub(crate) use watch::Watch;

pub(crate) enum Input {
    Move(f32, f32), Leave, Button(bool), Secondary(bool), ExtraButton(crate::PointerButton, bool),
    Wheel(crate::input::host_events::WheelInput),
    Focus(bool), Key(crate::KeyEvent), KeyRepeat(crate::KeyEvent), KeyUp(crate::KeyEvent),
    Physical(crate::PhysicalInput, Option<crate::KeyEvent>),
    Ime(crate::Ime),
}

#[derive(Default)]
struct State {
    /// The surface's life with the compositor: asked to configure, configured, mapped by a
    /// buffer, unmapped for the next show, or gone.
    mapping: mapping::State,
    mapping_seq: Seq,
    /// What `mapping` asked of the surface's proxies, until `LayerSurface::carry_out`.
    asked: Vec<mapping::Effect>,
    /// The `wl_display.sync` behind an unmap, kept so a drop can destroy it unanswered.
    fence: Option<WlCallback>,
    /// The buffer scale the compositor prefers, until `dispatch` reads it.
    scale: Option<i32>,
    /// Tracks whether the compositor has answered the last presented frame's `wl_surface.frame`
    /// callback so the overlay never blocks in Mesa's FIFO wait when the surface isn't painted.
    frame: frame_callback::State,
    /// Pending `wl_surface.frame` callback proxy, kept so it can be destroyed on replacement,
    /// close, or drop instead of retaining a `QueueHandle` reference to the event queue.
    frame_callback: Option<(u64, WlCallback)>,
    input: Vec<Input>,
    surface: Option<WlSurface>,
    over: Over,
    keyboard_focus: keyboard::FocusState,
    want_pointer: bool,
    want_keys: bool,
    capabilities: Option<wl_seat::Capability>,
    pointer: Option<WlPointer>,
    pointer_axis: pointer::State,
    /// A finger on the surface acts as the primary button, so a tap is a click.
    touch: Option<WlTouch>,
    finger: touch::State,
    /// `wp_cursor_shape_manager_v1` when the compositor has it, and its device for `pointer`.
    shapes: Option<WpCursorShapeManagerV1>,
    shape: Option<WpCursorShapeDeviceV1>,
    keyboard: Option<WlKeyboard>,
    xkb: Option<xkbcommon::xkb::State>,
    compose: compose::Slot,
    mod_state: Option<keyboard::ModState>,
    repeat_seq: crate::machine::Seq,
    repeat: repeat::State,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    text_input_state: text_input_machine::State,
}

impl Dispatch<WlOutput, ()> for State {
    fn event(_: &mut Self, _: &WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: wayland_client::WEnum::Value(caps) } = event {
            state.capabilities = Some(caps);
            state.update_seat(seat, qh);
        }
    }
}
impl State {
    fn step_keyboard_focus(&mut self, event: keyboard::FocusEvent) {
        for effect in keyboard::step(&mut self.keyboard_focus, event) {
            let keyboard::FocusEffect::Focus(focused) = effect;
            self.input.push(Input::Focus(focused));
        }
    }

    fn step_repeat(&mut self, event: repeat::Event) {
        let _ = repeat::step(&mut self.repeat_seq, &mut self.repeat, event);
    }

    fn reset_compose(&mut self) {
        self.compose.reset();
    }

    fn update_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<Self>) {
        let caps = self.capabilities.unwrap_or_else(wl_seat::Capability::empty);
        let pointer = self.want_pointer && caps.contains(wl_seat::Capability::Pointer);
        let keyboard = self.want_keys && caps.contains(wl_seat::Capability::Keyboard);
        if pointer && self.pointer.is_none() {
            let p = seat.get_pointer(qh, ());
            self.shape = self.shapes.as_ref().map(|shapes| shapes.get_pointer(&p, qh, ()));
            self.pointer = Some(p);
        }
        let touch = self.want_pointer && caps.contains(wl_seat::Capability::Touch);
        if touch && self.touch.is_none() { self.touch = Some(seat.get_touch(qh, ())); }
        if keyboard && self.keyboard.is_none() { self.keyboard = Some(seat.get_keyboard(qh, ())); }
        if !pointer && let Some(p) = self.pointer.take() {
            if let Some(shape) = self.shape.take() { shape.destroy(); }
            if p.version() >= 3 { p.release(); } else { destroy_proxy(&p); }
            self.over = Over::Outside;
            self.step_pointer(pointer::Event::Reset);
        }
        if !touch && let Some(t) = self.touch.take() {
            if t.version() >= 3 { t.release(); } else { destroy_proxy(&t); }
            touch::step(&mut self.finger, touch::Event::Gone);
        }
        if !keyboard && let Some(k) = self.keyboard.take() {
            if k.version() >= 3 { k.release(); } else { destroy_proxy(&k); }
            self.xkb = None;
            self.compose.clear();
            self.mod_state = None;
            self.step_repeat(repeat::Event::KeyboardGone);
            self.step_keyboard_focus(keyboard::FocusEvent::KeyboardGone);
        }
    }
}
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlCompositor, ()> for State {
    fn event(_: &mut Self, _: &WlCompositor, _: <WlCompositor as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlRegion, ()> for State {
    fn event(_: &mut Self, _: &WlRegion, _: <WlRegion as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: <ZwlrLayerShellV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlCallback, u64> for State {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            if state.frame_callback.as_ref().is_some_and(|(pending, _)| pending == id) {
                state.frame_callback = None;
            }
            frame_callback::step(&mut state.frame, frame_callback::Event::Done(*id));
        }
    }
}
impl Dispatch<WlSurface, ()> for State {
    fn event(state: &mut Self, _: &WlSurface, event: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            state.scale = Some(factor.max(1));
        }
    }
}

/// What the overlay needs to know after the compositor talked to the surface.
pub(crate) struct Changes {
    pub size: Option<(u32, u32)>,
    pub input: Vec<Input>,
    pub scale: Option<i32>,
    /// Why the surface can't show again, once it can't.
    pub gone: Option<Gone>,
}

pub(crate) struct LayerSurface {
    namespace: String,
    conn: Connection,
    globals: Option<GlobalList>,
    seat_global: Option<(u32, u32)>,
    regions: std::cell::RefCell<Option<Vec<[i32; 4]>>>,
    compositor: WlCompositor,
    shell: ZwlrLayerShellV1,
    output: Option<WlOutput>,
    seat: Option<WlSeat>,
    options: OverlayOptions,
    queue: EventQueue<State>,
    state: State,
    layer: ZwlrLayerSurfaceV1,
    surface: WlSurface,
    /// Where the role state last put the surface, for the commit that maps it again.
    at: Placement,
    /// The keyboard interactivity the role state asks for: none until the overlay asks for keys.
    interactivity: KeyboardInteractivity,
    buffer_scale: i32,
    display: NonNull<std::ffi::c_void>,
}

fn wire_layer(layer: crate::WaylandLayer) -> Layer {
    match layer {
        crate::WaylandLayer::Background => Layer::Background,
        crate::WaylandLayer::Bottom => Layer::Bottom,
        crate::WaylandLayer::Top => Layer::Top,
        crate::WaylandLayer::Overlay => Layer::Overlay,
    }
}

fn destroy_proxy<P: Proxy>(proxy: &P) {
    if let Some(backend) = proxy.backend().upgrade() {
        backend.destroy_object(&proxy.id()).ok();
    }
}

fn destroy_globals(conn: &Connection, globals: GlobalList) {
    let registry_id = globals.registry().id();
    globals.destroy();
    conn.backend().destroy_object(&registry_id).ok();
}

fn drain_event_queue(queue: &mut EventQueue<State>, state: &mut State) {
    loop {
        match queue.dispatch_pending(state) {
            Ok(0) => break,
            Ok(_) | Err(DispatchError::BadMessage { .. }) => {}
            Err(_) => break,
        }
    }
}

impl LayerSurface {
    /// None when the display isn't Wayland or the compositor lacks layer-shell. The role state
    /// is committed and the compositor's first `configure` asked for, not waited for: a later
    /// `dispatch` acks it, and `frame_ready` says when a frame may draw.
    #[allow(unsafe_code)]
    pub fn new(display: RawDisplayHandle, namespace: &str, at: Placement, interactive: bool, options: &OverlayOptions, output_id: Option<u32>) -> Option<LayerSurface> {
        let RawDisplayHandle::Wayland(display) = display else { return None };
        let want_keys = options.keys == crate::KeyPolicy::OnDemand;
        if want_keys { compose::warm(); }
        // SAFETY: winit owns this `wl_display` for the life of the event loop, which outlives
        // every overlay; a foreign backend never closes it.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) = match registry_queue_init::<State>(&conn) {
            Ok(pair) => pair,
            Err(_) => {
                if let Some(reason) = conn.backend().last_error().and_then(|e| format_wayland_error(&e)) {
                    eprintln!("rune: Wayland layer surface '{namespace}' failed: the display connection failed: {reason}");
                }
                return None;
            }
        };
        let qh = queue.handle();
        let Ok(compositor) = globals.bind::<WlCompositor, _, _>(&qh, 1..=6, ()) else {
            destroy_globals(&conn, globals);
            drain_event_queue(&mut queue, &mut State::default());
            queue.flush().ok();
            return None;
        };
        let Ok(shell) = globals.bind::<ZwlrLayerShellV1, _, _>(&qh, 1..=4, ()) else {
            conn.backend().destroy_object(&compositor.id()).ok();
            destroy_globals(&conn, globals);
            drain_event_queue(&mut queue, &mut State::default());
            queue.flush().ok();
            return None;
        };
        let output = output_id.and_then(|id| {
            globals.contents().with_list(|list| list.iter().find(|g| g.name == id && g.interface == "wl_output").map(|g| {
                globals.registry().bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, ())
            }))
        });
        let want_pointer = options.input.resolved(interactive) != OverlayInput::None;
        let seat_global = globals.contents().with_list(|list| list.iter().find(|g| g.interface == "wl_seat").map(|g| (g.name, g.version.min(8))));
        let seat = if want_pointer || want_keys { seat_global.map(|(name, version)| globals.registry().bind::<WlSeat, _, _>(name, version, &qh, ())) } else { None };
        let surface = compositor.create_surface(&qh, ());
        // There is no layer above Overlay; relative ordering of peers on one layer is
        // compositor policy, unlike AppKit's numeric levels.
        let layer = shell.get_layer_surface(&surface, output.as_ref(), wire_layer(options.wayland_layer), namespace.to_owned(), &qh, ());
        let shapes = globals.bind::<WpCursorShapeManagerV1, _, _>(&qh, 1..=1, ()).ok();
        let text_input_manager = want_keys
            .then(|| globals.bind::<ZwpTextInputManagerV3, _, _>(&qh, 1..=1, ()).ok())
            .flatten();
        let text_input = text_input_manager
            .as_ref()
            .zip(seat.as_ref())
            .map(|(manager, seat)| manager.get_text_input(seat, &qh, ()));
        let state = State {
            surface: Some(surface.clone()),
            want_pointer,
            want_keys,
            shapes,
            text_input_manager,
            text_input,
            ..State::default()
        };
        let mut this = LayerSurface {
            namespace: namespace.to_owned(), conn, globals: Some(globals), seat_global,
            // Takes no input until the first frame publishes its hit regions.
            regions: std::cell::RefCell::new(Some(Vec::new())),
            compositor, shell, output, seat, options: options.clone(), queue, state, layer, surface,
            at, interactivity: KeyboardInteractivity::None, buffer_scale: 1, display: display.display,
        };
        this.state.step_mapping(mapping::Event::Map { now: crate::time::now() });
        this.carry_out();
        this.flush_checked();
        if !this.is_live() { return None; }
        Some(this)
    }

    pub fn set_placement(&mut self, placement: Option<OverlayPlacement>) {
        self.options.placement = placement;
    }

    /// Sets the surface's pending input region; true when it changed, so it waits for a commit.
    pub fn input_regions(&self, regions: &[[f32; 4]], interactive: bool) -> bool {
        let next = match self.options.input.resolved(interactive) {
            OverlayInput::All => None,
            OverlayInput::Regions => Some(regions.iter().filter_map(|r| {
                let (x, y) = (r[0].floor() as i32, r[1].floor() as i32);
                let (w, h) = (r[2].ceil() as i32 - x, r[3].ceil() as i32 - y);
                (w > 0 && h > 0).then_some([x, y, w, h])
            }).collect()),
            _ => Some(Vec::new()),
        };
        if *self.regions.borrow() == next { return false; }
        *self.regions.borrow_mut() = next;
        self.send_input_region();
        // No commit here: a drawn frame's presentation commits input and pixels together, and a
        // frame that draws nothing commits it alone (`Overlay::unchanged_frame`).
        true
    }

    /// Sends the input region the surface last settled on; it waits for a commit.
    fn send_input_region(&self) {
        match &*self.regions.borrow() {
            Some(rects) => {
                let region = self.compositor.create_region(&self.queue.handle(), ());
                for &[x, y, w, h] in rects { region.add(x, y, w, h); }
                self.surface.set_input_region(Some(&region));
                region.destroy();
            }
            None => self.surface.set_input_region(None),
        }
    }

    pub fn set_input(&mut self, input: OverlayInput) {
        self.options.input = input;
        self.state.want_pointer = input != OverlayInput::None;
        if self.seat.is_none() && self.state.want_pointer && let Some(globals) = &self.globals {
            self.seat = self.seat_global.map(|(name, version)| globals.registry().bind(name, version, &self.queue.handle(), ()));
        }
        if let Some(seat) = &self.seat { self.state.update_seat(seat, &self.queue.handle()); }
    }

    /// Asks the compositor for the keys; false when the policy or the layer-shell version
    /// can't. The surface keeps asking until it hides.
    pub fn request_keys(&mut self) -> bool {
        if !keyboard::can_request_keys(self.options.keys, self.layer.version()) { return false; }
        self.interactivity = KeyboardInteractivity::OnDemand;
        self.layer.set_keyboard_interactivity(self.interactivity);
        self.commit();
        true
    }

    pub fn supports_keys(&self) -> bool {
        keyboard::supports_keys(self.options.keys, self.layer.version(), self.seat.is_some())
    }

    pub fn has_keyboard_focus(&self) -> bool {
        self.state.keyboard_focus.focused()
    }

    /// Synchronizes `zwp_text_input_v3` enablement and cursor rectangle with `step`.
    pub fn sync_ime(&mut self, step: crate::input::host_events::ImeStep) -> Option<crate::Ime> {
        if !self.is_live() { return None; }
        let (sent, ime) = self.state.sync_text_input(step);
        if sent { self.flush_checked(); }
        ime
    }

    /// Shows `want` while the pointer is over the surface, once per change. Without
    /// cursor-shape-v1 only `Cursor::Hidden` applies; others keep the cursor the pointer
    /// arrived with, such as a terminal's I-beam.
    pub fn set_cursor(&mut self, want: crate::el::Cursor) {
        let (Over::Inside { serial, cursor }, Some(pointer)) = (self.state.over, &self.state.pointer) else { return };
        if cursor == Some(want) { return; }
        match (want, &self.state.shape) {
            (crate::el::Cursor::Hidden, _) => pointer.set_cursor(serial, None, 0, 0),
            (want, Some(device)) => device.set_shape(serial, shape(want)),
            (_, None) => return,
        }
        self.state.over = Over::Inside { serial, cursor: Some(want) };
        self.flush_checked();
    }

    /// Requests a `wl_surface.frame` callback on this surface's private queue without committing,
    /// so it commits alongside the buffer in the immediately following `queue.present`.
    pub fn request_frame(&mut self) {
        if !self.is_live() { return; }
        for effect in frame_callback::step(&mut self.state.frame, frame_callback::Event::Request) {
            let frame_callback::Effect::Request(id) = effect;
            if let Some((_, prev)) = self.state.frame_callback.take() {
                self.conn.backend().destroy_object(&prev.id()).ok();
            }
            let cb = self.surface.frame(&self.queue.handle(), id);
            self.state.frame_callback = Some((id, cb));
        }
    }

    /// True when a frame may be acquired and presented: the compositor has configured the
    /// surface, and no `wl_surface.frame` callback is outstanding, so presenting will not block
    /// in Mesa's FIFO wait.
    pub fn frame_ready(&self) -> bool {
        self.state.mapping.drawable() && self.state.frame.ready()
    }

    /// True until the compositor closes the surface or never configures it, or the display
    /// connection dies.
    pub fn is_live(&self) -> bool {
        self.state.mapping.gone().is_none()
    }

    /// Whether the surface was made on the output asked for, rather than left to the compositor.
    pub fn has_output(&self) -> bool {
        self.output.is_some()
    }

    /// True when the surface is still live and libwayland's private event queue already holds
    /// unread events (for example from a mid-turn `wl_display` read during swapchain acquire,
    /// present, or a roundtrip). Checks the queue in memory without syscalls by immediately
    /// dropping any prepared-read guard.
    pub fn has_pending_events(&self) -> bool {
        self.is_live() && self.queue.prepare_read().is_none()
    }

    /// Next held-key repeat deadline for the overlay's wake timer, or `None` when idle.
    pub fn repeat_deadline(&self) -> Option<crate::time::Instant> {
        self.state.repeat.deadline()
    }

    /// Handles what the compositor sent since the last call (winit reads the socket) and fires
    /// any due held-key repeat.
    pub fn dispatch(&mut self) -> Changes {
        let armed = self.state.repeat.attempt();
        self.turn();
        let gone = self.state.mapping.gone();
        if gone.is_none() && let Some(attempt) = armed {
            self.state.fire_repeat(attempt, crate::time::now());
        }
        Changes {
            size: self.state.mapping.take_size(),
            input: std::mem::take(&mut self.state.input),
            scale: self.state.scale.take().filter(|_| gone.is_none()),
            gone,
        }
    }

    /// Handles what the compositor sent to a hidden surface and drops its input, keeping size
    /// and scale for the next `dispatch`; says why once the surface is gone. libwayland frees
    /// an undispatched event without closing its fd, so a queue left alone leaks every
    /// `wl_keyboard.keymap` (each virtual keyboard a paste tool makes sends a few).
    pub fn drain(&mut self) -> Option<Gone> {
        self.turn();
        self.state.step_repeat(repeat::Event::Stop);
        self.state.reset_compose();
        self.state.step_pointer(pointer::Event::Reset);
        if self.is_live() && self.state.drain_text_input() {
            self.flush_checked();
        }
        self.state.input.clear();
        self.state.mapping.gone()
    }

    /// Dispatches what winit read for the surface, lets a configure deadline pass, and sends
    /// what either asked for.
    fn turn(&mut self) {
        if !self.is_live() { return; }
        let waiting = self.state.mapping.attempt();
        let dispatch_err = self.queue.dispatch_pending(&mut self.state).err();
        // After the dispatch: a configure it just read disarms the deadline.
        if let Some(attempt) = waiting {
            self.state.step_mapping(mapping::Event::Timeout { attempt, now: crate::time::now() });
        }
        self.carry_out();
        let flush_err = self.queue.flush().err();
        let last_err = self.conn.backend().last_error();
        let (kind, reason) = classify_turn_error(dispatch_err, flush_err, last_err);
        match connection::turn_action(false, kind, !self.is_live()) {
            connection::TurnAction::Continue
            | connection::TurnAction::StopClosed
            | connection::TurnAction::AlreadyClosed => {}
            connection::TurnAction::FailAndClose => self.fail_connection(reason),
        }
    }

    fn flush_checked(&mut self) {
        if !self.is_live() { return; }
        let flush_err = self.queue.flush().err();
        let (kind, reason) = classify_turn_error(None, flush_err, self.conn.backend().last_error());
        if kind.is_some_and(connection::is_fatal) {
            self.fail_connection(reason);
        }
    }

    fn fail_connection(&mut self, reason: Option<String>) {
        let was_closed = !self.is_live();
        self.state.step_mapping(mapping::Event::Closed);
        self.state.drop_frame_callback(&self.conn);
        self.state.step_repeat(repeat::Event::Stop);
        self.state.reset_text_input_gone();
        self.state.input.clear();
        if !was_closed {
            match reason {
                Some(reason) => eprintln!("rune: Wayland layer surface '{}' closed: the display connection failed: {reason}", self.namespace),
                None => eprintln!("rune: Wayland layer surface '{}' closed: the display connection failed", self.namespace),
            }
        }
    }

    pub fn raw_handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        let surface = NonNull::new(self.surface.id().as_ptr().cast()).expect("live wl_surface");
        (
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(self.display)),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
        )
    }
}

impl Drop for LayerSurface {
    fn drop(&mut self) {
        self.state.want_pointer = false;
        self.state.want_keys = false;
        self.state.surface = None;
        self.state.reset_text_input_gone();
        // Drain pending events before destroying proxies so any queued `wl_keyboard.keymap` fd is
        // received and closed via `OwnedFd` rather than dropped open by libwayland.
        drain_event_queue(&mut self.queue, &mut self.state);
        let backend = self.conn.backend();
        if let Some((_, cb)) = self.state.frame_callback.take() { backend.destroy_object(&cb.id()).ok(); }
        if let Some(fence) = self.state.fence.take() { backend.destroy_object(&fence.id()).ok(); }
        if let Some(ti) = self.state.text_input.take() { ti.destroy(); }
        if let Some(tim) = self.state.text_input_manager.take() { tim.destroy(); }
        if let Some(shape) = self.state.shape.take() { shape.destroy(); }
        if let Some(shapes) = self.state.shapes.take() { shapes.destroy(); }
        if let Some(pointer) = self.state.pointer.take() {
            if pointer.version() >= 3 { pointer.release(); } else { backend.destroy_object(&pointer.id()).ok(); }
        }
        if let Some(touch) = self.state.touch.take() {
            if touch.version() >= 3 { touch.release(); } else { backend.destroy_object(&touch.id()).ok(); }
        }
        if let Some(keyboard) = self.state.keyboard.take() {
            if keyboard.version() >= 3 { keyboard.release(); } else { backend.destroy_object(&keyboard.id()).ok(); }
        }
        if let Some(seat) = self.seat.take() {
            if seat.version() >= 5 { seat.release(); } else { backend.destroy_object(&seat.id()).ok(); }
        }
        if let Some(output) = self.output.take() {
            if output.version() >= 3 { output.release(); } else { backend.destroy_object(&output.id()).ok(); }
        }
        self.layer.destroy();
        self.surface.destroy();
        if self.shell.version() >= 3 { self.shell.destroy(); } else { backend.destroy_object(&self.shell.id()).ok(); }
        backend.destroy_object(&self.compositor.id()).ok();
        if let Some(globals) = self.globals.take() {
            destroy_globals(&self.conn, globals);
        }
        // Drain again after destroying every proxy so `EventQueueInner` holds no queued events
        // (each retains `QueueHandle` in a cycle) and no proxy can enqueue more.
        drain_event_queue(&mut self.queue, &mut self.state);
        self.queue.flush().ok();
    }
}

#[cfg(test)]
#[path = "layer_shell_tests.rs"]
mod tests;
