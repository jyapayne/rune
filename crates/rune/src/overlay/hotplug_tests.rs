use std::sync::atomic::AtomicUsize;

use super::*;
use crate::tooling::AppSpec;
use crate::{Cx, Display, FontSmoothing, col};

fn view<T>(_: &T, _: &mut Cx<T>) -> El { col() }

fn app() -> AppSpec<()> { AppSpec::new("hotplug", (120.0, 80.0), || (), view) }

fn seed() -> AppSeed { AppSeed::of(&app(), FontSmoothing::default()) }

fn spec<T>() -> OverlaySpec<T> { OverlaySpec::new("hotplug-overlay", (120.0, 32.0), view) }

fn display(name: &str) -> Display {
    Display { id: None, name: Some(name.to_owned()), position: (0, 0), size: (1920, 1080), scale: 1.0, primary: false }
}

/// Counts the wakes a handle sends its host.
fn count_wakes<T>(handle: &OverlayHandle<T>) -> Arc<AtomicUsize> {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counted = wakes.clone();
    *handle.shared.core.wake.lock().unwrap() = Some(Arc::new(move || { counted.fetch_add(1, Ordering::AcqRel); }));
    wakes
}

#[test]
fn a_removed_overlay_is_finished_and_wakes_its_host_once() {
    let (host, handle) = OverlayHost::new(spec::<()>(), (), &app(), FontSmoothing::default());
    let wakes = count_wakes(&handle);
    assert!(!host.finished());

    handle.remove();
    assert!(host.finished(), "the loop drops a finished host at the end of the turn");
    assert_eq!(wakes.load(Ordering::Acquire), 1);

    // A second remove, from this handle or a clone, asks nothing more.
    handle.clone().remove();
    assert_eq!(wakes.load(Ordering::Acquire), 1);
}

#[test]
fn a_removed_overlay_hears_nothing_more() {
    let (_host, handle) = OverlayHost::new(spec::<u32>(), 0, &app(), FontSmoothing::default());
    handle.update(|n, _| *n += 1);
    assert_eq!(handle.shared.updates.lock().unwrap().len(), 1);
    let wakes = count_wakes(&handle);

    handle.remove();
    assert!(handle.shared.updates.lock().unwrap().is_empty(), "updates still queued don't run");

    handle.show();
    handle.resize((10.0, 10.0));
    handle.update(|n, _| *n += 1);
    let requests = handle.shared.core.requests.lock().unwrap();
    assert_eq!((requests.visible(), requests.size), (None, None));
    assert!(handle.shared.updates.lock().unwrap().is_empty());
    assert_eq!(wakes.load(Ordering::Acquire), 1, "only the remove woke the host");
}

#[test]
fn a_dropped_host_leaves_its_handle_unavailable_with_nothing_to_wake() {
    let (host, handle) = OverlayHost::new(spec::<()>(), (), &app(), FontSmoothing::default());
    let wakes = count_wakes(&handle);
    handle.remove();
    drop(host);
    assert_eq!(handle.availability(), Availability::Unavailable);
    assert!(handle.shared.core.wake.lock().unwrap().is_none());
    handle.shared.core.wake();
    assert_eq!(wakes.load(Ordering::Acquire), 1);
}

#[test]
fn an_overlay_asked_for_from_another_thread_is_built_once_behind_its_handle() {
    let shared: Arc<crate::app::SharedCore> = Arc::default();
    let link = AppLink::new(shared.clone(), seed(), None);
    assert!(!link.listens());

    let asked = shared.clone();
    let handle = std::thread::spawn(move || {
        let (handle, make) = deferred(spec::<u32>, 7);
        // Asked before its host exists: the request waits behind the handle.
        handle.show();
        asked.add_overlay(make);
        handle
    })
    .join()
    .unwrap();

    let mut added = Vec::new();
    link.take_added(&mut added);
    assert_eq!(added.len(), 1);
    assert_eq!(handle.shared.core.requests.lock().unwrap().visible(), Some(true));
    assert!(!added[0].finished());

    // The host is the handle's: removing through the handle finishes it.
    handle.remove();
    assert!(added[0].finished());

    link.take_added(&mut added);
    assert_eq!(added.len(), 1, "an overlay is built once");
}

#[test]
fn the_displays_handler_adds_an_overlay_per_display_and_removes_the_ones_that_went() {
    let mut bars: Vec<(String, OverlayHandle<()>)> = Vec::new();
    let handler: OnDisplays = Box::new(move |displays, cx| {
        bars.retain(|(name, bar)| {
            let stays = displays.iter().any(|d| d.name.as_deref() == Some(name));
            if !stays { bar.remove(); }
            stays
        });
        for display in displays {
            let name = display.name.clone().unwrap();
            if bars.iter().all(|(have, _)| *have != name) {
                bars.push((name, cx.overlay(spec::<()>(), ())));
            }
        }
    });
    let mut link = AppLink::new(Arc::default(), seed(), Some(handler));
    assert!(link.listens());

    let mut hosts = Vec::new();
    link.displays_changed(&[display("DP-1")], &mut hosts);
    assert_eq!(hosts.len(), 1);

    link.displays_changed(&[display("DP-1"), display("HDMI-A-1")], &mut hosts);
    assert_eq!(hosts.len(), 2, "the display that stayed keeps its overlay");

    link.displays_changed(&[display("DP-1")], &mut hosts);
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts.iter().map(|host| host.finished()).collect::<Vec<_>>(), [false, true]);
}

#[test]
fn an_overlay_added_while_running_takes_the_apps_poll_and_font_smoothing_like_one_added_before() {
    fn poll(_: &mut u32, _: f64, _: &mut Tx) -> bool { false }
    let mut app = AppSpec::new("hotplug-poll", (120.0, 80.0), || 0u32, view);
    app.poll = Some(poll);
    let seed = AppSeed::of(&app, FontSmoothing::Off);

    let same = OverlayHost::seeded(spec::<u32>().into(), 0, &seed, Arc::default());
    assert!(same.app.has_poll(), "an overlay of the app's state type polls with the app's poll");
    assert_eq!(same.app.rt().font_smoothing(), FontSmoothing::Off);

    let other = OverlayHost::seeded(spec::<()>().into(), (), &seed, Arc::default());
    assert!(!other.app.has_poll());

    let own = OverlayHost::seeded(spec::<()>().options(Default::default()).font_smoothing(FontSmoothing::On), (), &seed, Arc::default());
    assert_eq!(own.app.rt().font_smoothing(), FontSmoothing::On);
}
