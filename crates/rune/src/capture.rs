//! Screen and window capture on Wayland, for previews of windows (a dock's, an overview's) and a
//! frozen screen to select a region of.
//!
//! A [`Capturer`] keeps its own Wayland connection on a thread of its own, so it needs no window
//! and never blocks the UI thread. It captures an output by its connector name (`DP-2`) or a
//! window by its Hyprland address (`hyprctl clients -j`'s `address`), with
//! `ext-image-copy-capture-v1` where the compositor has it, and otherwise `wlr-screencopy-unstable-v1`
//! for outputs and `hyprland-toplevel-export-v1` for windows ([`Protocols`]). Frames arrive in
//! `wl_shm` buffers and are handed over as packed B, G, R, A bytes with straight alpha.
//!
//! - [`Capturer::shot`]: one picture, now ([`Still`]).
//! - [`Capturer::stream`]: a [`NativeSurface`] kept showing the source. The compositor sends a
//!   frame only when the content changes, and never faster than the stream's cap; a stream not
//!   shown asks for nothing.
//!
//! ```no_run
//! use rune::capture::{Capturer, Target, WindowAddress};
//! use std::time::Duration;
//!
//! let capturer = Capturer::connect()?;
//! let screen = capturer.shot_blocking(Target::output("DP-2"), Duration::from_secs(1))?;
//! assert_eq!(screen.bgra().len(), (screen.width() * screen.height() * 4) as usize);
//!
//! let preview = rune::NativeSurface::new();
//! let window = WindowAddress::parse("0x55d1c0de4ab0").unwrap();
//! let stream = capturer.stream(Target::window(window), preview.clone(), 4.0);
//! stream.set_shown(true); // while the dock's preview is on screen
//! // el().native_surface(&preview, SurfaceFit::Fit) shows it.
//! # Ok::<(), rune::capture::CaptureError>(())
//! ```

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

use crate::NativeSurface;
use crate::native_surface::{ByteFrame, ColorInfo, Format};

mod client;
mod convert;
mod registry;
mod session;
mod shm;
mod stream_machine;

/// A Hyprland window's address, as `hyprctl clients -j` writes it (`0x55d1c0de4ab0`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowAddress(pub u64);

impl WindowAddress {
    /// From `hyprctl`'s hexadecimal form, with or without its `0x`.
    pub fn parse(text: &str) -> Option<WindowAddress> {
        let digits = text.trim();
        let digits = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits);
        u64::from_str_radix(digits, 16).ok().map(WindowAddress)
    }
}

impl fmt::Debug for WindowAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WindowAddress({self})")
    }
}

impl fmt::Display for WindowAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:x}", self.0)
    }
}

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    source: Source,
    cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Output(String),
    Window(WindowAddress),
}

impl Target {
    /// The output with this connector name (`DP-2`, `HEADLESS-1`).
    pub fn output(name: impl Into<String>) -> Target {
        Target { source: Source::Output(name.into()), cursor: false }
    }

    /// A window, by its Hyprland address.
    pub fn window(address: WindowAddress) -> Target {
        Target { source: Source::Window(address), cursor: false }
    }

    /// With the pointer drawn in, where the compositor can.
    pub fn with_cursor(mut self) -> Target {
        self.cursor = true;
        self
    }
}

/// The protocol a kind of target is captured with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// `ext-image-copy-capture-v1`, with `ext-output-image-capture-source-manager-v1` for outputs
    /// and `ext-foreign-toplevel-image-capture-source-manager-v1` for windows.
    ExtImageCopyCapture,
    /// `wlr-screencopy-unstable-v1`, outputs only.
    WlrScreencopy,
    /// `hyprland-toplevel-export-v1`, windows only.
    HyprlandToplevelExport,
}

/// The capture protocols the compositor offers (the ones a [`Capturer`] uses).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Protocols {
    pub ext_image_copy_capture: bool,
    pub ext_output_source: bool,
    pub ext_toplevel_source: bool,
    /// `ext-foreign-toplevel-list-v1`, the windows `ext_toplevel_source` captures.
    pub ext_toplevel_list: bool,
    /// `hyprland-toplevel-mapping-v1`, which names a listed window's Hyprland address.
    pub hyprland_toplevel_mapping: bool,
    pub wlr_screencopy: bool,
    pub hyprland_toplevel_export: bool,
}

impl Protocols {
    /// How outputs are captured: `ext-image-copy-capture-v1` first.
    pub fn outputs(&self) -> Option<Backend> {
        if self.ext_image_copy_capture && self.ext_output_source {
            Some(Backend::ExtImageCopyCapture)
        } else if self.wlr_screencopy {
            Some(Backend::WlrScreencopy)
        } else {
            None
        }
    }

    /// How windows are captured: `ext-image-copy-capture-v1` first, which needs the window
    /// list and its mapping to addresses too.
    pub fn windows(&self) -> Option<Backend> {
        if self.ext_image_copy_capture && self.ext_toplevel_source && self.ext_toplevel_list && self.hyprland_toplevel_mapping {
            Some(Backend::ExtImageCopyCapture)
        } else if self.hyprland_toplevel_export {
            Some(Backend::HyprlandToplevelExport)
        } else {
            None
        }
    }

    fn backend(&self, target: &Target) -> Option<Backend> {
        match target.source {
            Source::Output(_) => self.outputs(),
            Source::Window(_) => self.windows(),
        }
    }
}

/// Why a capture gave no picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// No Wayland connection (not a Wayland session, or the compositor refused it).
    Connect(String),
    /// The compositor offers no protocol for this kind of target.
    Unsupported,
    NoOutput(String),
    NoWindow(WindowAddress),
    /// The compositor offers no buffer format this reads.
    Format,
    /// The compositor couldn't copy the frame.
    Failed(String),
    /// The source went away (the window closed, the output was unplugged).
    Stopped,
    /// The capture thread ended: the connection was lost.
    Closed,
    TimedOut,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Connect(why) => write!(f, "no Wayland connection: {why}"),
            CaptureError::Unsupported => f.write_str("the compositor offers no capture protocol for this target"),
            CaptureError::NoOutput(name) => write!(f, "no output named {name}"),
            CaptureError::NoWindow(address) => write!(f, "no window at {address}"),
            CaptureError::Format => f.write_str("the compositor offers no buffer format this reads"),
            CaptureError::Failed(why) => write!(f, "the compositor couldn't copy the frame: {why}"),
            CaptureError::Stopped => f.write_str("the capture source went away"),
            CaptureError::Closed => f.write_str("the capture connection was lost"),
            CaptureError::TimedOut => f.write_str("no frame came in time"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// One captured picture: packed B, G, R, A rows with straight alpha, top row first.
#[derive(Clone, PartialEq, Eq)]
pub struct Still {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    presented: Option<Duration>,
}

impl fmt::Debug for Still {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Still").field("width", &self.width).field("height", &self.height).field("presented", &self.presented).finish()
    }
}

impl Still {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    pub fn into_bgra(self) -> Vec<u8> {
        self.bgra
    }

    /// R, G, B, A bytes, for a PNG encoder or an [`crate::Image`].
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut rgba = self.bgra.clone();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        rgba
    }

    /// When the compositor presented this content, on the system's monotonic clock
    /// (`CLOCK_MONOTONIC`), where it says.
    pub fn presented(&self) -> Option<Duration> {
        self.presented
    }

    /// The `width`×`height` part whose top-left pixel is `(x, y)`; None when it isn't all inside.
    pub fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> Option<Still> {
        let fits = |at: u32, len: u32, of: u32| at.checked_add(len).is_some_and(|end| end <= of);
        if width == 0 || height == 0 || !fits(x, width, self.width) || !fits(y, height, self.height) {
            return None;
        }
        let row = self.width as usize * 4;
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for r in y..y + height {
            let start = r as usize * row + x as usize * 4;
            bgra.extend_from_slice(&self.bgra[start..start + width as usize * 4]);
        }
        Some(Still { width, height, bgra, presented: self.presented })
    }

    /// For [`NativeSurface::push_bytes`].
    pub fn into_byte_frame(self) -> ByteFrame {
        ByteFrame { width: self.width, height: self.height, format: Format::Bgra, color: ColorInfo::default(), planes: [self.bgra, Vec::new()] }
    }
}

/// What a stream has delivered so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamStats {
    /// Frames pushed to the surface.
    pub frames: u64,
    /// From the compositor's presentation of the last frame's content to its push, where the
    /// compositor said when it presented.
    pub latency: Option<Duration>,
    /// Reading and converting the last frame's buffer, on the capture thread.
    pub copy: Option<Duration>,
    /// Why the stream ended, once it has.
    pub ended: Option<CaptureError>,
}

/// A listed window with its Hyprland address.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WindowInfo {
    pub address: WindowAddress,
    pub title: String,
    pub app_id: String,
}

/// What the capture thread tells the handles.
#[derive(Default)]
struct Shared {
    protocols: Protocols,
    outputs: Mutex<Vec<String>>,
    windows: Mutex<Vec<WindowInfo>>,
}

type Done = Box<dyn FnOnce(Result<Still, CaptureError>) + Send>;

/// A connection to the compositor's capture protocols, kept on a thread of its own. Clones
/// share it; it closes when the last clone and the last [`Stream`] are dropped.
#[derive(Clone)]
pub struct Capturer {
    link: Arc<client::Link>,
    shared: Arc<Shared>,
}

impl fmt::Debug for Capturer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capturer").field("protocols", &self.shared.protocols).finish()
    }
}

impl Capturer {
    /// Connects to `WAYLAND_DISPLAY` and learns the compositor's outputs and windows (two round
    /// trips on the capture thread, waited for here).
    pub fn connect() -> Result<Capturer, CaptureError> {
        client::spawn(true)
    }

    /// As [`connect`](Self::connect), using `wlr-screencopy` and `hyprland-toplevel-export` even
    /// where `ext-image-copy-capture` is offered: for checking the fallbacks.
    #[doc(hidden)]
    pub fn connect_legacy() -> Result<Capturer, CaptureError> {
        client::spawn(false)
    }

    pub fn protocols(&self) -> Protocols {
        self.shared.protocols
    }

    /// The outputs' connector names.
    pub fn outputs(&self) -> Vec<String> {
        lock(&self.shared.outputs).clone()
    }

    /// The windows the compositor lists, with their addresses (empty without
    /// `ext-foreign-toplevel-list-v1` and `hyprland-toplevel-mapping-v1`).
    pub fn windows(&self) -> Vec<WindowInfo> {
        lock(&self.shared.windows).clone()
    }

    /// Captures `target` once and calls `done` with the picture, on the capture thread.
    pub fn shot(&self, target: Target, done: impl FnOnce(Result<Still, CaptureError>) + Send + 'static) {
        if let Err(client::Command::Shot { done, .. }) = self.link.send(client::Command::Shot { target, done: Box::new(done) }) {
            done(Err(CaptureError::Closed));
        }
    }

    /// Captures `target` once, waiting up to `timeout` for the picture.
    pub fn shot_blocking(&self, target: Target, timeout: Duration) -> Result<Still, CaptureError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.shot(target, move |still| {
            let _ = tx.send(still);
        });
        rx.recv_timeout(timeout).unwrap_or(Err(CaptureError::TimedOut))
    }

    /// Keeps `surface` showing `target`, at most `max_fps` frames a second (0: as fast as the
    /// content changes). It starts hidden: call [`Stream::set_shown`].
    pub fn stream(&self, target: Target, surface: NativeSurface, max_fps: f32) -> Stream {
        let id = self.link.next_id();
        let stats = Arc::new(Mutex::new(StreamStats::default()));
        let command = client::Command::Stream { id, target, surface, interval: interval(max_fps), stats: stats.clone() };
        if self.link.send(command).is_err() {
            lock(&stats).ended = Some(CaptureError::Closed);
        }
        Stream { id, link: self.link.clone(), stats }
    }
}

fn interval(max_fps: f32) -> Duration {
    if max_fps.is_finite() && max_fps > 0.0 { Duration::from_secs_f32(1.0 / max_fps) } else { Duration::ZERO }
}

/// A continuous capture into a [`NativeSurface`]; dropping it stops it.
pub struct Stream {
    id: u64,
    link: Arc<client::Link>,
    stats: Arc<Mutex<StreamStats>>,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream").field("id", &self.id).field("stats", &self.stats()).finish()
    }
}

impl Stream {
    /// Whether the surface is on screen: a stream that isn't asks the compositor for nothing.
    pub fn set_shown(&self, shown: bool) {
        let _ = self.link.send(client::Command::Show { id: self.id, shown });
    }

    pub fn set_max_fps(&self, max_fps: f32) {
        let _ = self.link.send(client::Command::Interval { id: self.id, interval: interval(max_fps) });
    }

    pub fn stats(&self) -> StreamStats {
        lock(&self.stats).clone()
    }
}

/// The capture thread and the handles share a little state behind locks; a panic elsewhere
/// leaves it readable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.link.send(client::Command::Stop { id: self.id });
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "capture/end_to_end_tests.rs"]
mod end_to_end_tests;
#[cfg(test)]
mod fake_compositor;
