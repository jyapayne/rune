//! The capture thread against `fake_compositor`: real protocol objects, real shm buffers.

use std::time::Duration;

use wayland_client::Connection;

use super::fake_compositor::{self, Command, Fake, Key, OUTPUT, Offers};
use super::{CaptureError, Capturer, Still, Target, WindowAddress, client};
use crate::NativeSurface;

const WAIT: Duration = Duration::from_secs(5);

fn connect(offers: Offers, windows: &[(u64, [u32; 2])], ext: bool) -> (Fake, Capturer) {
    let (fake, stream) = fake_compositor::start(offers, windows);
    let capturer = client::spawn_on(Connection::from_socket(stream).expect("a connection"), ext).expect("the capture thread starts");
    (fake, capturer)
}

/// Waits for `done`, which the threads make true soon.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let start = crate::time::now();
    while !done() {
        assert!(start.elapsed() < WAIT, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Every pixel holds its own (x, y) and the content's generation, as the fake paints them.
fn assert_painted(still: &Still, generation: u8) {
    for y in 0..still.height() {
        for x in 0..still.width() {
            let at = ((y * still.width() + x) * 4) as usize;
            assert_eq!(&still.bgra()[at..at + 4], &[x as u8, y as u8, generation, 255], "pixel ({x}, {y})");
        }
    }
}

#[test]
fn an_output_shot_arrives_with_every_pixel_where_the_compositor_put_it() {
    let (_fake, capturer) = connect(Offers::ALL, &[], true);
    assert_eq!(capturer.outputs(), [OUTPUT]);
    assert_eq!(capturer.protocols().outputs(), Some(super::Backend::ExtImageCopyCapture));
    let still = capturer.shot_blocking(Target::output(OUTPUT), WAIT).unwrap();
    assert_eq!((still.width(), still.height()), (6, 4));
    assert_painted(&still, 0);
    assert_eq!(still.presented(), Some(Duration::from_secs(1)), "the frame's presentation time");
}

#[test]
fn the_wlr_fallback_packs_the_compositors_padded_upside_down_rows_top_first() {
    let (_fake, capturer) = connect(Offers::ALL, &[], false);
    assert_eq!(capturer.protocols().outputs(), Some(super::Backend::WlrScreencopy));
    let still = capturer.shot_blocking(Target::output(OUTPUT), WAIT).unwrap();
    assert_eq!((still.width(), still.height()), (6, 4));
    assert_painted(&still, 0);
}

#[test]
fn an_unknown_output_or_window_is_refused_rather_than_waited_for() {
    let (_fake, capturer) = connect(Offers::ALL, &[], true);
    assert_eq!(capturer.shot_blocking(Target::output("DP-9"), WAIT), Err(CaptureError::NoOutput("DP-9".into())));
    assert_eq!(capturer.shot_blocking(Target::window(WindowAddress(7)), WAIT), Err(CaptureError::NoWindow(WindowAddress(7))));
    // The Hyprland export never answers for a window that doesn't exist; the list does.
    let (_fake, legacy) = connect(Offers::ALL, &[], false);
    assert_eq!(legacy.shot_blocking(Target::window(WindowAddress(7)), WAIT), Err(CaptureError::NoWindow(WindowAddress(7))));
}

#[test]
fn a_window_is_found_by_its_hyprland_address_even_when_it_opened_just_before_the_shot() {
    let (fake, capturer) = connect(Offers::ALL, &[], true);
    let address = WindowAddress(0x55d1_0000_0042);
    fake.send(Command::Open(address.0, [3, 2]));
    let still = capturer.shot_blocking(Target::window(address), WAIT).unwrap();
    assert_eq!((still.width(), still.height()), (3, 2));
    assert_painted(&still, 0);
    let windows = capturer.windows();
    assert_eq!(windows.len(), 1);
    assert_eq!((windows[0].address, windows[0].app_id.as_str()), (address, "fake"));
}

#[test]
fn the_hyprland_export_fallback_names_a_window_by_its_addresss_low_32_bits() {
    let address = WindowAddress(0x55d1_9abc_def0);
    let (_fake, capturer) = connect(Offers { ext: false, ..Offers::ALL }, &[(address.0, [2, 3])], true);
    assert_eq!(capturer.protocols().windows(), Some(super::Backend::HyprlandToplevelExport));
    let still = capturer.shot_blocking(Target::window(address), WAIT).unwrap();
    assert_eq!((still.width(), still.height()), (2, 3));
    assert_painted(&still, 0);
}

#[test]
fn a_stream_asks_for_nothing_until_it_is_shown() {
    let (fake, capturer) = connect(Offers::ALL, &[], true);
    let surface = NativeSurface::new();
    let stream = capturer.stream(Target::output(OUTPUT), surface.clone(), 0.0);
    // The session opens; no frame is asked of it.
    let _ = capturer.shot_blocking(Target::output(OUTPUT), WAIT).unwrap();
    assert_eq!(fake.log().asked, 1, "only the shot's");
    assert_eq!(stream.stats().frames, 0);
    assert!(surface.latest().is_none());
    stream.set_shown(true);
    eventually("the shown stream's first frame", || stream.stats().frames == 1);
    assert_eq!(surface.latest().unwrap().size, [6, 4]);
}

#[test]
fn a_stream_gets_a_frame_only_when_the_content_changes_and_hiding_gives_up_the_wait() {
    let (fake, capturer) = connect(Offers::ALL, &[], true);
    let surface = NativeSurface::new();
    let stream = capturer.stream(Target::output(OUTPUT), surface, 0.0);
    stream.set_shown(true);
    eventually("the first frame, at once", || stream.stats().frames == 1);
    eventually("the next frame asked for", || fake.log().asked == 2);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(stream.stats().frames, 1, "nothing changed, so nothing came");
    fake.send(Command::Change(Key::Output));
    eventually("a frame of the changed content", || stream.stats().frames == 2);
    eventually("the next frame asked for", || fake.log().asked == 3);
    stream.set_shown(false);
    eventually("the waiting frame given up", || fake.log().given_up == 1);
    fake.send(Command::Change(Key::Output));
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(stream.stats().frames, 2, "a hidden stream takes no frame");
    stream.set_shown(true);
    eventually("the change made while hidden, once shown", || stream.stats().frames == 3);
}

#[test]
fn a_resized_window_gets_a_buffer_of_its_new_size_and_keeps_streaming() {
    let address = WindowAddress(0x77);
    let (fake, capturer) = connect(Offers::ALL, &[(address.0, [4, 4])], true);
    let surface = NativeSurface::new();
    let stream = capturer.stream(Target::window(address), surface.clone(), 0.0);
    stream.set_shown(true);
    eventually("the first frame", || stream.stats().frames == 1);
    assert_eq!(surface.latest().unwrap().size, [4, 4]);
    eventually("the next frame asked for", || fake.log().asked == 2);
    fake.send(Command::Resize(Key::Window(address.0), [5, 3]));
    fake.send(Command::Change(Key::Window(address.0)));
    eventually("a frame of the new size", || stream.stats().frames == 2);
    assert_eq!(surface.latest().unwrap().size, [5, 3]);
    assert_eq!(stream.stats().ended, None);
}

#[test]
fn closing_the_window_ends_its_stream_whichever_protocol_captures_it() {
    for ext in [true, false] {
        let address = WindowAddress(0x5_0000_0001);
        let (fake, capturer) = connect(Offers::ALL, &[(address.0, [2, 2])], ext);
        let stream = capturer.stream(Target::window(address), NativeSurface::new(), 0.0);
        stream.set_shown(true);
        eventually("the first frame", || stream.stats().frames == 1);
        fake.send(Command::Close(address.0));
        eventually("the stream ended", || stream.stats().ended == Some(CaptureError::Stopped));
        assert!(capturer.windows().is_empty(), "ext: {ext}");
    }
}

#[test]
fn a_capturer_with_no_compositor_says_why() {
    let (fake, capturer) = connect(Offers::ALL, &[], true);
    drop(fake);
    assert_eq!(capturer.shot_blocking(Target::output(OUTPUT), WAIT).unwrap_err(), CaptureError::Closed);
    let stream = capturer.stream(Target::output(OUTPUT), NativeSurface::new(), 4.0);
    assert_eq!(stream.stats().ended, Some(CaptureError::Closed));
}
