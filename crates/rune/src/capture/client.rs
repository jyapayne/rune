//! The capture thread: its own Wayland connection and queue, the commands the handles send it,
//! and the captures (jobs) it runs. It sleeps in `poll` on the connection, a wake socket the
//! handles write to, and the earliest stream's frame-rate hold.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use super::registry::{Globals, Output, Toplevel};
use super::session::{Outcome, Session};
use super::stream_machine::{self, Effect, Event};
use super::{Capturer, CaptureError, Done, Shared, Source, StreamStats, Target, WindowAddress, WindowInfo, lock};
use crate::NativeSurface;
use crate::machine::{Effects, Seq};
use crate::time::{self, Instant};

pub(crate) enum Command {
    Shot { target: Target, done: Done },
    Stream { id: u64, target: Target, surface: NativeSurface, interval: Duration, stats: Arc<Mutex<StreamStats>> },
    Show { id: u64, shown: bool },
    Interval { id: u64, interval: Duration },
    Stop { id: u64 },
}

/// The handles' way to the capture thread. Dropping the last one closes the wake socket's
/// writing end, which wakes the thread to find the command channel closed and end.
pub(crate) struct Link {
    tx: Sender<Command>,
    wake: UnixStream,
    ids: AtomicU64,
    /// Set by the thread as it ends, under the lock it takes to refuse what was sent before:
    /// a command is either run or refused, never lost.
    closed: Arc<Mutex<bool>>,
}

impl Link {
    /// Sends `command`; it comes back when the capture thread has ended.
    pub(super) fn send(&self, command: Command) -> Result<(), Command> {
        let closed = lock(&self.closed);
        if *closed {
            return Err(command);
        }
        self.tx.send(command).map_err(|unsent| unsent.0)?;
        drop(closed);
        // Full means the thread has a wake pending already.
        let _ = (&self.wake).write(&[1]);
        Ok(())
    }

    pub(super) fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }
}

/// Shots count down from the top, streams up from one: one key space for the jobs.
const FIRST_SHOT: u64 = u64::MAX;

pub(super) fn spawn(ext: bool) -> Result<Capturer, CaptureError> {
    spawn_on(Connection::connect_to_env().map_err(|e| CaptureError::Connect(e.to_string()))?, ext)
}

/// A capture thread on `conn`. `ext`: use `ext-image-copy-capture-v1` where it is offered.
pub(super) fn spawn_on(conn: Connection, ext: bool) -> Result<Capturer, CaptureError> {
    let io = |e: std::io::Error| CaptureError::Connect(e.to_string());
    let (wake, wake_rx) = UnixStream::pair().map_err(io)?;
    wake.set_nonblocking(true).map_err(io)?;
    wake_rx.set_nonblocking(true).map_err(io)?;
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let closed = Arc::new(Mutex::new(false));
    let gate = closed.clone();
    std::thread::Builder::new().name("rune-capture".into()).spawn(move || run(conn, ext, rx, wake_rx, ready_tx, &gate)).map_err(io)?;
    let shared = ready_rx.recv().map_err(|_| CaptureError::Closed)??;
    Ok(Capturer { link: Arc::new(Link { tx, wake, ids: AtomicU64::new(1), closed }), shared })
}

fn run(conn: Connection, ext: bool, rx: Receiver<Command>, wake: UnixStream, ready: SyncSender<Result<Arc<Shared>, CaptureError>>, closed: &Mutex<bool>) {
    let mut queue: EventQueue<Client> = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());
    let mut client = Client::new(qh, display, ext);
    // The globals; then the outputs' names and the window list; then the windows' addresses.
    for _ in 0..3 {
        if let Err(e) = queue.roundtrip(&mut client) {
            let _ = ready.send(Err(CaptureError::Connect(e.to_string())));
            return;
        }
    }
    client.shared = Arc::new(Shared { protocols: client.globals.protocols(), ..Shared::default() });
    client.publish_outputs();
    client.publish_windows();
    if ready.send(Ok(client.shared.clone())).is_err() {
        return;
    }
    loop {
        if queue.dispatch_pending(&mut client).is_err() || !client.commands(&rx) {
            break;
        }
        client.timers(time::now());
        if queue.flush().is_err() {
            break;
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let timeout = client.next_due().map(|due| due.saturating_duration_since(time::now()));
        let timeout = timeout.map(|left| Timespec { tv_sec: left.as_secs() as i64, tv_nsec: i64::from(left.subsec_nanos()) });
        let (readable, woken) = {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN), PollFd::new(&wake, PollFlags::IN)];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => (!fds[0].revents().is_empty(), !fds[1].revents().is_empty()),
                Err(rustix::io::Errno::INTR) => (false, false),
                Err(_) => break,
            }
        };
        if readable {
            if let Err(e) = guard.read()
                && !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
            {
                break;
            }
        } else {
            drop(guard);
        }
        if woken {
            let mut drain = [0u8; 64];
            while matches!((&wake).read(&mut drain), Ok(n) if n > 0) {}
        }
    }
    // Refuse what was sent and not run, outside the lock: a refused shot's callback may send.
    let refused: Vec<Command> = {
        let mut closed = lock(closed);
        *closed = true;
        rx.try_iter().collect()
    };
    for command in refused {
        match command {
            Command::Shot { done, .. } => done(Err(CaptureError::Closed)),
            Command::Stream { stats, .. } => lock(&stats).ended = Some(CaptureError::Closed),
            Command::Show { .. } | Command::Interval { .. } | Command::Stop { .. } => {}
        }
    }
    let ids: Vec<u64> = client.jobs.keys().copied().collect();
    for id in ids {
        client.fail(id, CaptureError::Closed);
    }
}

/// A continuous capture's state on the thread.
pub(super) struct StreamJob {
    surface: NativeSurface,
    stats: Arc<Mutex<StreamStats>>,
    state: stream_machine::State,
    interval: Duration,
    seq: Seq,
    frames: u64,
}

pub(super) enum Kind {
    Shot(Option<Done>),
    Stream(StreamJob),
}

pub(super) struct Job {
    target: Target,
    kind: Kind,
    /// None until the target is found (a window the list hasn't named yet).
    pub(super) session: Option<Session>,
    /// How many round trips were spent looking for the target's window.
    looked: u8,
}

impl Job {
    /// Whether the job wants a frame now.
    fn wants(&self) -> bool {
        match &self.kind {
            Kind::Shot(_) => true,
            Kind::Stream(stream) => stream.state == stream_machine::State::Capturing,
        }
    }
}

pub(crate) struct Client {
    pub(super) qh: QueueHandle<Client>,
    display: WlDisplay,
    pub(super) globals: Globals,
    pub(super) outputs: Vec<Output>,
    pub(super) toplevels: Vec<Toplevel>,
    pub(super) jobs: HashMap<u64, Job>,
    next_shot: u64,
    shared: Arc<Shared>,
}

impl Client {
    fn new(qh: QueueHandle<Client>, display: WlDisplay, ext: bool) -> Client {
        Client {
            qh,
            display,
            globals: Globals::new(ext),
            outputs: Vec::new(),
            toplevels: Vec::new(),
            jobs: HashMap::new(),
            next_shot: FIRST_SHOT,
            shared: Arc::default(),
        }
    }

    pub(super) fn publish_outputs(&self) {
        *lock(&self.shared.outputs) = self.outputs.iter().filter_map(|output| output.name.clone()).collect();
    }

    pub(super) fn publish_windows(&self) {
        let windows = self.toplevels.iter().filter_map(|toplevel| {
            Some(WindowInfo { address: toplevel.address?, title: toplevel.title.clone(), app_id: toplevel.app_id.clone() })
        });
        *lock(&self.shared.windows) = windows.collect();
    }

    /// Runs every command sent so far. False when every handle is gone.
    fn commands(&mut self, rx: &Receiver<Command>) -> bool {
        loop {
            match rx.try_recv() {
                Ok(command) => self.command(command),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Shot { target, done } => {
                let id = self.next_shot;
                self.next_shot -= 1;
                self.start(id, target, Kind::Shot(Some(done)));
            }
            Command::Stream { id, target, surface, interval, stats } => {
                let stream = StreamJob { surface, stats, state: stream_machine::State::default(), interval, seq: Seq::default(), frames: 0 };
                self.start(id, target, Kind::Stream(stream));
            }
            Command::Show { id, shown } => {
                let event = if shown { Event::Show { now: time::now() } } else { Event::Hide };
                self.step(id, event);
            }
            Command::Interval { id, interval } => {
                if let Some(Job { kind: Kind::Stream(stream), .. }) = self.jobs.get_mut(&id) {
                    stream.interval = interval;
                }
                self.step(id, Event::Retimed { now: time::now() });
            }
            Command::Stop { id } => {
                self.jobs.remove(&id);
            }
        }
    }

    fn start(&mut self, id: u64, target: Target, kind: Kind) {
        self.jobs.insert(id, Job { target, kind, session: None, looked: 0 });
        self.open(id);
    }

    /// Finds the job's target and opens its session, and asks for a frame if one is wanted.
    pub(super) fn open(&mut self, id: u64) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Some(backend) = self.shared.protocols.backend(&job.target) else { return self.fail(id, CaptureError::Unsupported) };
        let cursor = job.target.cursor;
        let session = match &job.target.source {
            Source::Output(name) => {
                let Some(output) = self.outputs.iter().find(|output| output.name.as_ref() == Some(name)) else {
                    let name = name.clone();
                    return self.fail(id, CaptureError::NoOutput(name));
                };
                Session::output(&self.globals, backend, &output.wl, cursor, &self.qh, id)
            }
            Source::Window(address) => {
                match self.toplevels.iter().find(|toplevel| toplevel.address == Some(*address)) {
                    Some(toplevel) => Session::window(&self.globals, backend, Some(&toplevel.handle), *address, cursor, &self.qh, id),
                    // Nothing names the windows: the compositor's answer says whether it exists.
                    None if !(self.shared.protocols.ext_toplevel_list && self.shared.protocols.hyprland_toplevel_mapping) => {
                        Session::window(&self.globals, backend, None, *address, cursor, &self.qh, id)
                    }
                    // The list may not have named the window yet: a round trip has the compositor
                    // send every toplevel it made before it, and a second the addresses asked for
                    // on their arrival.
                    None if job.looked < 2 => {
                        job.looked += 1;
                        self.display.sync(&self.qh, id);
                        return;
                    }
                    None => {
                        let address = *address;
                        return self.fail(id, CaptureError::NoWindow(address));
                    }
                }
            }
        };
        match session {
            Ok(session) => job.session = Some(session),
            Err(why) => return self.fail(id, why),
        }
        if job.wants() {
            self.capture(id);
        }
    }

    /// Asks the job's session for a frame.
    fn capture(&mut self, id: u64) {
        let Some(Job { session: Some(session), .. }) = self.jobs.get_mut(&id) else { return };
        if let Some(outcome) = session.request(&self.globals, &self.qh, id) {
            self.outcome(id, outcome);
        }
    }

    /// What the session said of the frame asked for.
    pub(super) fn outcome(&mut self, id: u64, outcome: Outcome) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        match (&mut job.kind, outcome) {
            (Kind::Shot(done), Outcome::Frame { still, .. }) => {
                if let Some(done) = done.take() {
                    done(Ok(still));
                }
                self.jobs.remove(&id);
            }
            (Kind::Shot(_), Outcome::Failed(why)) => self.fail(id, why),
            (Kind::Shot(_), Outcome::Stopped) => self.fail(id, CaptureError::Stopped),
            (Kind::Stream(stream), Outcome::Frame { still, arrived, copy }) => {
                stream.frames += 1;
                let latency = still.presented().and_then(|presented| monotonic_now().checked_sub(presented));
                // The surface's waker runs inside the push, so the stats count the frame first.
                let mut stats = lock(&stream.stats);
                stats.frames += 1;
                stats.latency = latency;
                stats.copy = Some(copy);
                drop(stats);
                if let Err(why) = stream.surface.push_bytes(still.into_byte_frame(), stream.frames) {
                    return self.fail(id, CaptureError::Failed(why));
                }
                // The cap counts from the frame's arrival, not from the end of its copy.
                self.step(id, Event::Ready { now: arrived });
            }
            (Kind::Stream(_), Outcome::Failed(_)) => self.step(id, Event::Failed { now: time::now() }),
            (Kind::Stream(_), Outcome::Stopped) => self.step(id, Event::Stopped),
        }
    }

    /// A listed window closed: its captures end. `hyprland-toplevel-export-v1` never says so
    /// itself; it leaves a frame of a window that is gone unanswered.
    pub(super) fn window_closed(&mut self, address: WindowAddress) {
        let gone = Source::Window(address);
        let ids: Vec<u64> = self.jobs.iter().filter(|(_, job)| job.target.source == gone).map(|(id, _)| *id).collect();
        for id in ids {
            self.outcome(id, Outcome::Stopped);
        }
    }

    /// Ends the job with `why`.
    pub(super) fn fail(&mut self, id: u64, why: CaptureError) {
        let Some(job) = self.jobs.remove(&id) else { return };
        match job.kind {
            Kind::Shot(Some(done)) => done(Err(why)),
            Kind::Shot(None) => {}
            Kind::Stream(stream) => lock(&stream.stats).ended = Some(why),
        }
    }

    fn step(&mut self, id: u64, event: Event) {
        let Some(Job { kind: Kind::Stream(stream), .. }) = self.jobs.get_mut(&id) else { return };
        let effects: Effects<Effect> = stream_machine::step(&mut stream.seq, &mut stream.state, stream.interval, event);
        for effect in effects {
            match effect {
                Effect::Capture => self.capture(id),
                Effect::Cancel => {
                    if let Some(Job { session: Some(session), .. }) = self.jobs.get_mut(&id) {
                        session.cancel();
                    }
                }
                // `next_due` reads the hold from the state.
                Effect::WakeAt(_) => {}
                Effect::End => self.fail(id, CaptureError::Stopped),
            }
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.jobs.values().filter_map(|job| match &job.kind {
            Kind::Stream(stream) => stream.state.due().map(|(_, due)| due),
            Kind::Shot(_) => None,
        }).min()
    }

    fn timers(&mut self, now: Instant) {
        let due: Vec<(u64, crate::machine::Attempt)> = self.jobs.iter().filter_map(|(id, job)| match &job.kind {
            Kind::Stream(stream) => stream.state.due().filter(|(_, due)| *due <= now).map(|(attempt, _)| (*id, attempt)),
            Kind::Shot(_) => None,
        }).collect();
        for (id, attempt) in due {
            self.step(id, Event::Due { attempt, now });
        }
    }
}

/// The system's monotonic clock, which the compositor's presentation times are on.
pub(super) fn monotonic_now() -> Duration {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

/// A round trip a job spent looking for its window: look again.
impl Dispatch<WlCallback, u64> for Client {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.open(*id);
        }
    }
}
