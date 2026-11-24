//! One capture's conversation with the compositor, in whichever protocol captures its target.
//!
//! - `ext-image-copy-capture-v1`: a session per target says what buffers it takes (size and
//!   formats, sent again when they change); each frame attaches the session's one buffer and
//!   asks for a capture, which the compositor answers once the content has changed since the
//!   last frame (at once for the session's first), copying only what changed.
//! - `wlr-screencopy-unstable-v1` (outputs) and `hyprland-toplevel-export-v1` (windows): a new
//!   frame object per frame offers its buffer layouts, then copies into the buffer it is
//!   given, at once for a session's first frame and after the next change for later ones.

use std::time::{Duration, Instant};

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1};
use wayland_protocols_hyprland::toplevel_export::v1::client::hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};

use super::client::{Client, Job};
use super::convert::{self, Layout, Turn};
use super::registry::Globals;
use super::shm::ShmBuffer;
use super::{Backend, CaptureError, Still, WindowAddress};

/// How often in a row the compositor may refuse the buffer before the frame fails.
const REFUSALS: u8 = 3;

/// What became of the frame asked for.
pub(super) enum Outcome {
    /// `arrived`: when the compositor said it was ready; `copy`: reading and converting it since.
    Frame { still: Still, arrived: Instant, copy: Duration },
    Failed(CaptureError),
    Stopped,
}

pub(super) enum Session {
    Ext(Ext),
    Legacy(Legacy),
}

impl Session {
    pub(super) fn output(g: &Globals, backend: Backend, output: &WlOutput, cursor: bool, qh: &QueueHandle<Client>, id: u64) -> Result<Session, CaptureError> {
        match (backend, &g.ext_copy, &g.ext_outputs) {
            (Backend::ExtImageCopyCapture, Some(copy), Some(sources)) => {
                let source = sources.create_source(output, qh, ());
                let session = copy.create_session(&source, options(cursor), qh, id);
                source.destroy();
                Ok(Session::Ext(Ext::new(session, false)))
            }
            (Backend::WlrScreencopy, ..) => Ok(Session::Legacy(Legacy::new(LegacySource::Output(output.clone()), cursor))),
            _ => Err(CaptureError::Unsupported),
        }
    }

    /// `handle`: the listed window at `address`, where the list named it.
    pub(super) fn window(
        g: &Globals,
        backend: Backend,
        handle: Option<&ExtForeignToplevelHandleV1>,
        address: WindowAddress,
        cursor: bool,
        qh: &QueueHandle<Client>,
        id: u64,
    ) -> Result<Session, CaptureError> {
        match (backend, &g.ext_copy, &g.ext_toplevels, handle) {
            (Backend::ExtImageCopyCapture, Some(copy), Some(sources), Some(handle)) => {
                let source = sources.create_source(handle, qh, ());
                let session = copy.create_session(&source, options(cursor), qh, id);
                source.destroy();
                Ok(Session::Ext(Ext::new(session, true)))
            }
            // The protocol names a window by its address's low 32 bits.
            (Backend::HyprlandToplevelExport, ..) => Ok(Session::Legacy(Legacy::new(LegacySource::Window(address.0 as u32), cursor))),
            _ => Err(CaptureError::Unsupported),
        }
    }

    /// Asks for a frame, unless one is asked for already. Some when it failed at once.
    pub(super) fn request(&mut self, g: &Globals, qh: &QueueHandle<Client>, id: u64) -> Option<Outcome> {
        match self {
            Session::Ext(ext) => ext.request(g, qh, id),
            Session::Legacy(legacy) => legacy.request(g, qh, id),
        }
    }

    /// Gives up the frame asked for.
    pub(super) fn cancel(&mut self) {
        match self {
            Session::Ext(ext) => {
                ext.wanted = false;
                // The compositor may or may not have copied into the buffer: copy it all next time.
                ext.fresh = true;
                if let Some(frame) = ext.frame.take() {
                    frame.proxy.destroy();
                }
            }
            Session::Legacy(legacy) => {
                legacy.first = true;
                if let Some(frame) = legacy.frame.take() {
                    frame.proxy.destroy();
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
        if let Session::Ext(ext) = self {
            ext.session.destroy();
        }
    }
}

fn options(cursor: bool) -> Options {
    if cursor { Options::PaintCursors } else { Options::empty() }
}

/// What a session takes, as the compositor last said.
#[derive(Clone, Debug, Default)]
struct Constraints {
    size: Option<[u32; 2]>,
    shm: Vec<Format>,
}

pub(super) struct Ext {
    session: ExtImageCopyCaptureSessionV1,
    /// The picture keeps its alpha (a window's).
    alpha: bool,
    /// Constraints being sent, until their `done`.
    pending: Constraints,
    constraints: Option<Constraints>,
    buffer: Option<ShmBuffer>,
    /// The buffer holds no frame the compositor knows of: damage all of it.
    fresh: bool,
    frame: Option<ExtFrame>,
    /// A frame is wanted once the constraints come.
    wanted: bool,
    refused: u8,
}

struct ExtFrame {
    proxy: ExtImageCopyCaptureFrameV1,
    turn: Turn,
    presented: Option<Duration>,
}

impl Ext {
    fn new(session: ExtImageCopyCaptureSessionV1, alpha: bool) -> Ext {
        Ext { session, alpha, pending: Constraints::default(), constraints: None, buffer: None, fresh: true, frame: None, wanted: false, refused: 0 }
    }

    fn request(&mut self, g: &Globals, qh: &QueueHandle<Client>, id: u64) -> Option<Outcome> {
        if self.frame.is_some() {
            return None;
        }
        let Some(constraints) = &self.constraints else {
            self.wanted = true;
            return None;
        };
        self.wanted = false;
        let (Some(size), Some(format)) = (constraints.size, convert::pick(&constraints.shm, self.alpha)) else {
            return Some(Outcome::Failed(CaptureError::Format));
        };
        let buffer = match buffer_for(&mut self.buffer, g, qh, size, size[0] * 4, format) {
            Ok((buffer, new)) => {
                self.fresh |= new;
                buffer
            }
            Err(why) => return Some(Outcome::Failed(why)),
        };
        let proxy = self.session.create_frame(qh, id);
        proxy.attach_buffer(&buffer.buffer);
        if self.fresh {
            proxy.damage_buffer(0, 0, size[0] as i32, size[1] as i32);
        }
        proxy.capture();
        self.frame = Some(ExtFrame { proxy, turn: Turn::Normal, presented: None });
        None
    }

    fn session_event(&mut self, event: ext_image_copy_capture_session_v1::Event, g: &Globals, qh: &QueueHandle<Client>, id: u64) -> Option<Outcome> {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => self.pending.size = Some([width, height]),
            ext_image_copy_capture_session_v1::Event::ShmFormat { format: WEnum::Value(format) } => self.pending.shm.push(format),
            ext_image_copy_capture_session_v1::Event::Done => {
                self.constraints = Some(std::mem::take(&mut self.pending));
                if self.wanted {
                    return self.request(g, qh, id);
                }
            }
            ext_image_copy_capture_session_v1::Event::Stopped => return Some(Outcome::Stopped),
            _ => {}
        }
        None
    }

    fn frame_event(&mut self, event: ext_image_copy_capture_frame_v1::Event, g: &Globals, qh: &QueueHandle<Client>, id: u64) -> Option<Outcome> {
        let frame = self.frame.as_mut()?;
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform: WEnum::Value(transform) } => frame.turn = Turn::of(transform),
            ext_image_copy_capture_frame_v1::Event::PresentationTime { tv_sec_hi, tv_sec_lo, tv_nsec } => {
                frame.presented = Some(timestamp(tv_sec_hi, tv_sec_lo, tv_nsec));
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                let frame = self.frame.take()?;
                frame.proxy.destroy();
                self.fresh = false;
                self.refused = 0;
                return Some(develop(self.buffer.as_ref()?, frame.turn, false, frame.presented));
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                self.frame.take()?.proxy.destroy();
                self.fresh = true;
                return match reason {
                    // New constraints came before this: a buffer that meets them is made.
                    WEnum::Value(FailureReason::BufferConstraints) if self.refused < REFUSALS => {
                        self.refused += 1;
                        self.request(g, qh, id)
                    }
                    WEnum::Value(FailureReason::Stopped) => Some(Outcome::Stopped),
                    _ => Some(Outcome::Failed(CaptureError::Failed("the compositor failed the frame".into()))),
                };
            }
            _ => {}
        }
        None
    }
}

#[derive(Clone)]
pub(super) enum LegacySource {
    Output(WlOutput),
    /// A window address's low 32 bits.
    Window(u32),
}

pub(super) struct Legacy {
    source: LegacySource,
    cursor: bool,
    frame: Option<LegacyFrame>,
    buffer: Option<ShmBuffer>,
    /// The next frame is copied at once rather than after the next change.
    first: bool,
}

enum LegacyProxy {
    Wlr(ZwlrScreencopyFrameV1),
    Hyprland(HyprlandToplevelExportFrameV1),
}

impl LegacyProxy {
    fn destroy(&self) {
        match self {
            LegacyProxy::Wlr(frame) => frame.destroy(),
            LegacyProxy::Hyprland(frame) => frame.destroy(),
        }
    }

    /// Whether the compositor ends its buffer offers with `buffer_done`.
    fn ends_offers(&self) -> bool {
        match self {
            LegacyProxy::Wlr(frame) => frame.version() >= 3,
            LegacyProxy::Hyprland(_) => true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Offer {
    format: Format,
    size: [u32; 2],
    stride: u32,
}

struct LegacyFrame {
    proxy: LegacyProxy,
    offers: Vec<Offer>,
    copying: bool,
    y_invert: bool,
}

/// A legacy frame's events, from either protocol.
enum LegacyEvent {
    Buffer(Offer),
    BufferDone,
    YInvert(bool),
    Ready(Duration),
    Failed,
}

impl Legacy {
    fn new(source: LegacySource, cursor: bool) -> Legacy {
        Legacy { source, cursor, frame: None, buffer: None, first: true }
    }

    fn request(&mut self, g: &Globals, qh: &QueueHandle<Client>, id: u64) -> Option<Outcome> {
        if self.frame.is_some() {
            return None;
        }
        let proxy = match (&self.source, &g.wlr, &g.hyprland_export) {
            (LegacySource::Output(output), Some(wlr), _) => LegacyProxy::Wlr(wlr.capture_output(i32::from(self.cursor), output, qh, id)),
            (LegacySource::Window(handle), _, Some(export)) => LegacyProxy::Hyprland(export.capture_toplevel(i32::from(self.cursor), *handle, qh, id)),
            _ => return Some(Outcome::Failed(CaptureError::Unsupported)),
        };
        self.frame = Some(LegacyFrame { proxy, offers: Vec::new(), copying: false, y_invert: false });
        None
    }

    fn event(&mut self, event: LegacyEvent, g: &Globals, qh: &QueueHandle<Client>) -> Option<Outcome> {
        let frame = self.frame.as_mut()?;
        match event {
            LegacyEvent::Buffer(offer) => {
                frame.offers.push(offer);
                if !frame.proxy.ends_offers() {
                    return self.copy(g, qh);
                }
            }
            LegacyEvent::BufferDone => return self.copy(g, qh),
            LegacyEvent::YInvert(y_invert) => frame.y_invert = y_invert,
            LegacyEvent::Ready(presented) => {
                let frame = self.frame.take()?;
                frame.proxy.destroy();
                self.first = false;
                return Some(develop(self.buffer.as_ref()?, Turn::Normal, frame.y_invert, Some(presented)));
            }
            LegacyEvent::Failed => {
                self.frame.take()?.proxy.destroy();
                return Some(Outcome::Failed(CaptureError::Failed("the compositor failed the frame".into())));
            }
        }
        None
    }

    /// The offers are in: copy into a buffer of the best one.
    fn copy(&mut self, g: &Globals, qh: &QueueHandle<Client>) -> Option<Outcome> {
        let frame = self.frame.as_mut()?;
        if frame.copying {
            return None;
        }
        let formats: Vec<Format> = frame.offers.iter().map(|offer| offer.format).collect();
        let alpha = matches!(self.source, LegacySource::Window(_));
        let Some(offer) = convert::pick(&formats, alpha).and_then(|format| frame.offers.iter().find(|offer| offer.format == format).copied()) else {
            self.frame.take()?.proxy.destroy();
            return Some(Outcome::Failed(CaptureError::Format));
        };
        let buffer = match buffer_for(&mut self.buffer, g, qh, offer.size, offer.stride, offer.format) {
            Ok((buffer, _)) => buffer,
            Err(why) => {
                self.frame.take()?.proxy.destroy();
                return Some(Outcome::Failed(why));
            }
        };
        frame.copying = true;
        match &frame.proxy {
            LegacyProxy::Wlr(proxy) if self.first || proxy.version() < 2 => proxy.copy(&buffer.buffer),
            LegacyProxy::Wlr(proxy) => proxy.copy_with_damage(&buffer.buffer),
            LegacyProxy::Hyprland(proxy) => proxy.copy(&buffer.buffer, i32::from(self.first)),
        }
        None
    }
}

/// `slot`'s buffer if it fits, otherwise a new one in its place (true).
fn buffer_for<'a>(slot: &'a mut Option<ShmBuffer>, g: &Globals, qh: &QueueHandle<Client>, size: [u32; 2], stride: u32, format: Format) -> Result<(&'a ShmBuffer, bool), CaptureError> {
    let new = !slot.as_ref().is_some_and(|buffer| buffer.fits(size, stride, format));
    if new {
        let shm = g.shm.as_ref().ok_or(CaptureError::Unsupported)?;
        // The old buffer goes before the new one is made.
        *slot = None;
        *slot = Some(ShmBuffer::new(shm, qh, size, stride, format).map_err(|e| CaptureError::Failed(e.to_string()))?);
    }
    slot.as_ref().map(|buffer| (buffer, new)).ok_or(CaptureError::Unsupported)
}

/// The upright picture in `buffer`.
fn develop(buffer: &ShmBuffer, turn: Turn, y_invert: bool, presented: Option<Duration>) -> Outcome {
    let start = crate::time::now();
    let Some(layout) = Layout::of(buffer.format) else { return Outcome::Failed(CaptureError::Format) };
    let source = convert::Source { width: buffer.width, height: buffer.height, stride: buffer.stride, layout, turn, y_invert };
    let bytes = match buffer.read() {
        Ok(bytes) => bytes,
        Err(e) => return Outcome::Failed(CaptureError::Failed(e.to_string())),
    };
    let Some(bgra) = convert::upright(&source, bytes) else { return Outcome::Failed(CaptureError::Failed("the buffer is shorter than its size".into())) };
    let [width, height] = source.upright_size();
    Outcome::Frame { still: Still { width, height, bgra, presented }, arrived: start, copy: start.elapsed() }
}

fn timestamp(sec_hi: u32, sec_lo: u32, nsec: u32) -> Duration {
    Duration::new(u64::from(sec_hi) << 32 | u64::from(sec_lo), nsec)
}

/// The job's session, its globals and queue, borrowed apart from the rest of the client.
fn session_of(state: &mut Client, id: u64) -> Option<(&mut Session, &Globals, &QueueHandle<Client>)> {
    let Client { jobs, globals, qh, .. } = state;
    match jobs.get_mut(&id) {
        Some(Job { session: Some(session), .. }) => Some((session, globals, qh)),
        _ => None,
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for Client {
    fn event(state: &mut Self, _: &ExtImageCopyCaptureSessionV1, event: ext_image_copy_capture_session_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        let Some((Session::Ext(ext), g, qh)) = session_of(state, *id) else { return };
        if let Some(outcome) = ext.session_event(event, g, qh, *id) {
            state.outcome(*id, outcome);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for Client {
    fn event(state: &mut Self, frame: &ExtImageCopyCaptureFrameV1, event: ext_image_copy_capture_frame_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        let Some((Session::Ext(ext), g, qh)) = session_of(state, *id) else { return };
        if ext.frame.as_ref().is_none_or(|current| &current.proxy != frame) {
            return;
        }
        if let Some(outcome) = ext.frame_event(event, g, qh, *id) {
            state.outcome(*id, outcome);
        }
    }
}

fn legacy_event(state: &mut Client, id: u64, frame: &wayland_client::backend::ObjectId, event: LegacyEvent) {
    let Some((Session::Legacy(legacy), g, qh)) = session_of(state, id) else { return };
    let current = legacy.frame.as_ref().map(|current| match &current.proxy {
        LegacyProxy::Wlr(proxy) => proxy.id(),
        LegacyProxy::Hyprland(proxy) => proxy.id(),
    });
    if current.as_ref() != Some(frame) {
        return;
    }
    if let Some(outcome) = legacy.event(event, g, qh) {
        state.outcome(id, outcome);
    }
}

fn offer(format: WEnum<Format>, width: u32, height: u32, stride: u32) -> Option<LegacyEvent> {
    match format {
        WEnum::Value(format) => Some(LegacyEvent::Buffer(Offer { format, size: [width, height], stride })),
        WEnum::Unknown(_) => None,
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, u64> for Client {
    fn event(state: &mut Self, frame: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event as E;
        let event = match event {
            E::Buffer { format, width, height, stride } => offer(format, width, height, stride),
            E::BufferDone => Some(LegacyEvent::BufferDone),
            E::Flags { flags } => Some(LegacyEvent::YInvert(matches!(flags, WEnum::Value(flags) if flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert)))),
            E::Ready { tv_sec_hi, tv_sec_lo, tv_nsec } => Some(LegacyEvent::Ready(timestamp(tv_sec_hi, tv_sec_lo, tv_nsec))),
            E::Failed => Some(LegacyEvent::Failed),
            _ => None,
        };
        if let Some(event) = event {
            legacy_event(state, *id, &frame.id(), event);
        }
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, u64> for Client {
    fn event(state: &mut Self, frame: &HyprlandToplevelExportFrameV1, event: hyprland_toplevel_export_frame_v1::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        use hyprland_toplevel_export_frame_v1::Event as E;
        let event = match event {
            E::Buffer { format, width, height, stride } => offer(format, width, height, stride),
            E::BufferDone => Some(LegacyEvent::BufferDone),
            E::Flags { flags } => Some(LegacyEvent::YInvert(matches!(flags, WEnum::Value(flags) if flags.contains(hyprland_toplevel_export_frame_v1::Flags::YInvert)))),
            E::Ready { tv_sec_hi, tv_sec_lo, tv_nsec } => Some(LegacyEvent::Ready(timestamp(tv_sec_hi, tv_sec_lo, tv_nsec))),
            E::Failed => Some(LegacyEvent::Failed),
            _ => None,
        };
        if let Some(event) = event {
            legacy_event(state, *id, &frame.id(), event);
        }
    }
}

delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
