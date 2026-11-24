use super::*;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android")))]
mod wayland {
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use winit::raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};

    use super::super::fake_compositor::{self, Command, Fake, Grant};
    use super::super::{LockState, SessionLock, Shared, turn_lock};
    use crate::el::Align;
    use crate::overlay::{Placement, ScreenEdge};
    use crate::platform::layer_shell::{Ask, Input, LayerSurface};
    use crate::{KeyPolicy, KeyboardState, OverlayInput, OverlayOptions};

    const AT: Placement = Placement { size: (100.0, 100.0), edge: ScreenEdge::Top, margin: 0.0, align: Align::Start, inset: 0.0 };

    /// Dropped in this order: the lock's wire lives on the host's display, so it goes first
    /// (the event loop's registry never drops a live one at all).
    struct Rig {
        lock: SessionLock,
        heard: Arc<Mutex<Vec<LockState>>>,
        host: Connection,
        fake: Fake,
    }

    fn rig(grant: Grant) -> Rig {
        let (fake, stream) = fake_compositor::start(grant);
        let host = Connection::from_socket(stream).expect("a connection to the fake compositor");
        let heard: Arc<Mutex<Vec<LockState>>> = Arc::default();
        let sink = heard.clone();
        let lock = SessionLock::detached(move |state| sink.lock().unwrap().push(state), || {});
        Rig { lock, heard, host, fake }
    }

    fn display(host: &Connection) -> NonNull<std::ffi::c_void> {
        NonNull::new(host.display().id().as_ptr().cast()).expect("a wl_display")
    }

    fn options() -> OverlayOptions {
        OverlayOptions { keys: KeyPolicy::Exclusive, input: OverlayInput::All, ..Default::default() }
    }

    fn lock_surface(rig: &Rig, output: Option<u32>) -> Option<LayerSurface> {
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display(&rig.host)));
        LayerSurface::make(raw, "lock", AT, true, &options(), output, Ask::Lock(&rig.lock))
    }

    struct Globals;
    impl Dispatch<WlRegistry, GlobalListContents> for Globals {
        fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }

    /// The registry names of the compositor's outputs, in the order it made them.
    fn outputs(host: &Connection) -> Vec<u32> {
        let (globals, _queue) = registry_queue_init::<Globals>(host).expect("the registry");
        globals.contents().with_list(|list| list.iter().filter(|g| g.interface == "wl_output").map(|g| g.name).collect())
    }

    /// One read of the socket, as winit's loop does for every queue on the connection.
    fn read_once(host: &Connection) {
        let guard = host.prepare_read().or_else(|| {
            host.backend().dispatch_inner_queue().ok();
            host.prepare_read()
        });
        if let Some(guard) = guard {
            let _ = guard.read();
        }
    }

    /// Turns the loop (the lock, then each surface) until `done`; the fake answers from its
    /// own thread. Returns the input the surfaces heard, and any size they were given.
    fn turn_until(rig: &Rig, surfaces: &mut [&mut LayerSurface], done: impl Fn(&Rig, &[&mut LayerSurface]) -> bool) -> (Vec<Input>, Vec<(u32, u32)>) {
        let (mut input, mut sizes) = (Vec::new(), Vec::new());
        for _ in 0..1000 {
            read_once(&rig.host);
            turn_lock(&rig.lock.shared, Some(display(&rig.host)));
            for surface in surfaces.iter_mut() {
                let changes = surface.dispatch();
                input.extend(changes.input);
                sizes.extend(changes.size);
            }
            if done(rig, surfaces) {
                return (input, sizes);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the fake compositor never answered; the lock is {:?}", rig.lock.state());
    }

    /// What the fake saw since the last call, once it has read everything sent so far.
    fn requests(rig: &Rig) -> Vec<String> {
        rig.host.roundtrip().expect("a roundtrip with the fake compositor");
        rig.fake.take()
    }

    fn heard(rig: &Rig) -> Vec<LockState> {
        rig.heard.lock().unwrap().clone()
    }

    #[test]
    fn every_output_gets_a_lock_surface_including_one_added_while_locked() {
        let rig = rig(Grant::Later);
        let first_output = outputs(&rig.host)[0];
        // Making the first lock surface sends the lock; the compositor configures the surface
        // unasked, and nothing is committed before the ack.
        let mut first = lock_surface(&rig, Some(first_output)).expect("a lock surface");
        let (_, sizes) = turn_until(&rig, &mut [&mut first], |_, s| s[0].frame_ready());
        assert_eq!(sizes, [(640, 480)], "sized to its output");
        assert_eq!(requests(&rig), ["lock", "get_lock_surface FAKE-1", "ack 1 FAKE-1"]);
        assert_eq!(rig.lock.state(), LockState::Pending);
        first.presented();
        first.commit();
        assert_eq!(requests(&rig), ["commit FAKE-1"], "mapped, role state may be committed");

        rig.fake.send(Command::Locked);
        turn_until(&rig, &mut [&mut first], |rig, _| rig.lock.state() == LockState::Locked);
        assert_eq!(heard(&rig), [LockState::Locked]);

        // A display plugged in while locked gets its own surface on the same lock.
        rig.fake.send(Command::AddOutput("FAKE-2", 800, 600));
        let mut all = outputs(&rig.host);
        for _ in 0..500 {
            if all.len() == 2 { break; }
            std::thread::sleep(Duration::from_millis(2));
            all = outputs(&rig.host);
        }
        let mut second = lock_surface(&rig, Some(all[1])).expect("a lock surface on the new output");
        let (_, sizes) = turn_until(&rig, &mut [&mut first, &mut second], |_, s| s[1].frame_ready());
        assert_eq!(sizes, [(800, 600)]);
        assert_eq!(requests(&rig), ["get_lock_surface FAKE-2", "ack 2 FAKE-2"]);

        // Surfaces going leave the session locked: only `unlock` unlocks it.
        drop(second);
        drop(first);
        assert_eq!(requests(&rig), ["lock surface destroyed FAKE-2", "lock surface destroyed FAKE-1"]);
        assert_eq!(rig.lock.state(), LockState::Locked);
    }

    #[test]
    fn only_an_explicit_unlock_unlocks_and_the_lock_surfaces_close_with_it() {
        let rig = rig(Grant::Now);
        let output = outputs(&rig.host)[0];
        let mut surface = lock_surface(&rig, Some(output)).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |rig, s| s[0].frame_ready() && rig.lock.state() == LockState::Locked);
        surface.presented();
        requests(&rig);

        // A clone dropped, even in a panicking thread, unlocks nothing.
        let clone = rig.lock.clone();
        let panicked = std::thread::spawn(move || {
            let _held = clone;
            panic!("a lock screen's thread panics");
        })
        .join();
        assert!(panicked.is_err());
        turn_until(&rig, &mut [&mut surface], |_, _| true);
        assert!(requests(&rig).is_empty(), "nothing was sent");
        assert_eq!(rig.lock.state(), LockState::Locked);

        rig.lock.unlock();
        let mut over = false;
        for _ in 0..1000 {
            read_once(&rig.host);
            over = turn_lock(&rig.lock.shared, Some(display(&rig.host))).1;
            if over { break; }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(over, "the wire goes once the compositor has read the unlock");
        assert_eq!(heard(&rig), [LockState::Locked, LockState::Unlocked]);
        assert_eq!(surface.dispatch().gone, Some(crate::platform::layer_shell::Gone::Closed), "the lock's surfaces close with it");
        drop(surface);
        assert_eq!(requests(&rig), ["unlock_and_destroy", "lock surface destroyed FAKE-1"]);
        assert!(lock_surface(&rig, Some(output)).is_none(), "an ended lock makes no new surface");
    }

    #[test]
    fn dropping_every_handle_of_a_locked_lock_sends_nothing() {
        let rig = rig(Grant::Now);
        let output = outputs(&rig.host)[0];
        let mut surface = lock_surface(&rig, Some(output)).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |rig, _| rig.lock.state() == LockState::Locked);
        requests(&rig);
        // The event loop's registry keeps what the app lets go of.
        let kept: Arc<Shared> = rig.lock.shared.clone();
        let Rig { fake, host, lock, .. } = rig;
        drop(surface);
        drop(lock);
        host.roundtrip().expect("a roundtrip");
        assert_eq!(fake.take(), ["lock surface destroyed FAKE-1"], "no unlock_and_destroy, no destroy");
        assert_eq!(super::super::guard(&kept.inner).machine.public(), LockState::Locked);
        // The registry's own drop, while the display is still open.
        drop(kept);
        drop(host);
    }

    #[test]
    fn an_unlock_before_locked_waits_for_it() {
        let rig = rig(Grant::Later);
        let mut surface = lock_surface(&rig, Some(outputs(&rig.host)[0])).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |_, s| s[0].frame_ready());
        rig.lock.unlock();
        turn_until(&rig, &mut [&mut surface], |_, _| true);
        assert_eq!(rig.lock.state(), LockState::Pending);
        assert_eq!(requests(&rig), ["lock", "get_lock_surface FAKE-1", "ack 1 FAKE-1"], "no unlock before locked");
        rig.fake.send(Command::Locked);
        turn_until(&rig, &mut [&mut surface], |rig, _| rig.lock.state() == LockState::Unlocked);
        assert_eq!(heard(&rig), [LockState::Unlocked]);
        assert_eq!(requests(&rig), ["unlock_and_destroy"]);
    }

    #[test]
    fn a_refused_lock_is_destroyed_and_its_surfaces_close() {
        let rig = rig(Grant::Refuse);
        let output = outputs(&rig.host)[0];
        let mut surface = lock_surface(&rig, Some(output)).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |rig, _| rig.lock.state() == LockState::Finished);
        assert_eq!(heard(&rig), [LockState::Finished]);
        assert_eq!(surface.dispatch().gone, Some(crate::platform::layer_shell::Gone::Closed));
        drop(surface);
        let seen = requests(&rig);
        assert!(seen.contains(&"destroy".to_owned()), "{seen:?}");
        assert!(!seen.iter().any(|r| r.starts_with("violation") || r.starts_with("unlock")), "{seen:?}");
        assert!(lock_surface(&rig, Some(output)).is_none());
    }

    #[test]
    fn a_lock_the_compositor_ends_after_locking_goes_with_unlock_and_destroy() {
        let rig = rig(Grant::Now);
        let mut surface = lock_surface(&rig, Some(outputs(&rig.host)[0])).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |rig, _| rig.lock.state() == LockState::Locked);
        rig.fake.send(Command::Finished);
        turn_until(&rig, &mut [&mut surface], |rig, _| rig.lock.state() == LockState::Finished);
        assert_eq!(heard(&rig), [LockState::Locked, LockState::Finished]);
        assert_eq!(surface.dispatch().gone, Some(crate::platform::layer_shell::Gone::Closed));
        let seen = requests(&rig);
        assert_eq!(seen.last().map(String::as_str), Some("unlock_and_destroy"), "after locked, destroy is a protocol error: {seen:?}");
    }

    #[test]
    fn a_lock_surface_hears_caps_lock_and_the_layout() {
        let rig = rig(Grant::Now);
        let mut surface = lock_surface(&rig, Some(outputs(&rig.host)[0])).expect("a lock surface");
        turn_until(&rig, &mut [&mut surface], |_, s| s[0].frame_ready());
        surface.presented();
        rig.fake.send(Command::Focus { caps_lock: true });
        let keyboard = |input: &[Input]| input.iter().filter_map(|i| if let Input::Keyboard(state) = i { Some(state.clone()) } else { None }).collect::<Vec<_>>();
        let (input, _) = turn_until(&rig, &mut [&mut surface], |_, s| s[0].has_keyboard_focus());
        let mut states = keyboard(&input);
        if states.is_empty() {
            states = keyboard(&turn_until(&rig, &mut [&mut surface], |_, _| true).0);
        }
        assert_eq!(states, [KeyboardState::new(true, false, Some("English (US)".into()))]);
        rig.fake.send(Command::Focus { caps_lock: false });
        let mut seen = Vec::new();
        for _ in 0..500 {
            seen.extend(keyboard(&turn_until(&rig, &mut [&mut surface], |_, _| true).0));
            if !seen.is_empty() { break; }
        }
        assert_eq!(seen, [KeyboardState::new(false, false, Some("English (US)".into()))]);
    }

    #[test]
    fn a_lock_surface_needs_an_output_of_its_own_and_sends_nothing_without_one() {
        let rig = rig(Grant::Now);
        assert!(lock_surface(&rig, None).is_none());
        assert!(requests(&rig).is_empty(), "not even the lock");
        assert_eq!(rig.lock.state(), LockState::Pending);
    }
}

#[test]
fn a_lock_state_has_ended_once_it_cant_come_back() {
    assert!(!LockState::Pending.ended());
    assert!(!LockState::Locked.ended());
    assert!(LockState::Unlocked.ended() && LockState::Finished.ended() && LockState::Unsupported.ended());
}

#[test]
fn unlock_wakes_the_loop_and_does_nothing_once_ended() {
    let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = wakes.clone();
    let lock = SessionLock::detached(|_| {}, move || { counted.fetch_add(1, Ordering::Relaxed); });
    lock.unlock();
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(lock.shared.unlock_asked.load(Ordering::Acquire));
    *guard(&lock.shared.public) = LockState::Finished;
    lock.unlock();
    assert_eq!(wakes.load(Ordering::Relaxed), 1, "an ended lock is left alone");
    assert!(lock.same(&lock.clone()));
    assert_eq!(format!("{lock:?}"), "SessionLock { state: Finished }");
}
