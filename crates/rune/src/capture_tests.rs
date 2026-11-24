use std::time::Duration;

use super::{Backend, Protocols, Still, Target, WindowAddress, interval};
use crate::native_surface::Format;

#[test]
fn a_window_address_reads_and_writes_as_hyprctl_shows_it() {
    assert_eq!(WindowAddress::parse("0x55d1c0de4ab0"), Some(WindowAddress(0x55d1_c0de_4ab0)));
    assert_eq!(WindowAddress::parse("55d1c0de4ab0"), Some(WindowAddress(0x55d1_c0de_4ab0)));
    assert_eq!(WindowAddress::parse(" 0XFF\n"), Some(WindowAddress(255)));
    assert_eq!(WindowAddress::parse("0xnope"), None);
    assert_eq!(WindowAddress::parse(""), None);
    assert_eq!(WindowAddress(0x55d1_c0de_4ab0).to_string(), "0x55d1c0de4ab0");
}

#[test]
fn ext_image_copy_capture_is_preferred_for_both_kinds_of_target() {
    let all = Protocols {
        ext_image_copy_capture: true,
        ext_output_source: true,
        ext_toplevel_source: true,
        ext_toplevel_list: true,
        hyprland_toplevel_mapping: true,
        wlr_screencopy: true,
        hyprland_toplevel_export: true,
    };
    assert_eq!(all.outputs(), Some(Backend::ExtImageCopyCapture));
    assert_eq!(all.windows(), Some(Backend::ExtImageCopyCapture));
    assert_eq!(all.backend(&Target::output("DP-2")), Some(Backend::ExtImageCopyCapture));
}

#[test]
fn each_kind_of_target_falls_back_on_its_own_protocol() {
    let legacy = Protocols { wlr_screencopy: true, hyprland_toplevel_export: true, ..Protocols::default() };
    assert_eq!(legacy.outputs(), Some(Backend::WlrScreencopy));
    assert_eq!(legacy.windows(), Some(Backend::HyprlandToplevelExport));
    // ext capture of windows needs the list and the addresses too.
    let no_mapping = Protocols { ext_image_copy_capture: true, ext_toplevel_source: true, ext_toplevel_list: true, hyprland_toplevel_export: true, ..Protocols::default() };
    assert_eq!(no_mapping.windows(), Some(Backend::HyprlandToplevelExport));
    let no_source = Protocols { ext_image_copy_capture: true, wlr_screencopy: true, ..Protocols::default() };
    assert_eq!(no_source.outputs(), Some(Backend::WlrScreencopy));
    assert_eq!(Protocols::default().outputs(), None);
    assert_eq!(Protocols::default().backend(&Target::window(WindowAddress(1))), None);
}

fn still_3x2() -> Still {
    // Pixel (x, y) is B=x, G=y, R=9, A=255.
    let bgra = (0..2u8).flat_map(|y| (0..3u8).flat_map(move |x| [x, y, 9, 255])).collect();
    Still { width: 3, height: 2, bgra, presented: Some(Duration::from_secs(5)) }
}

#[test]
fn a_crop_takes_the_pixels_inside_it_and_refuses_one_reaching_outside() {
    let still = still_3x2();
    let crop = still.crop(1, 1, 2, 1).unwrap();
    assert_eq!((crop.width(), crop.height()), (2, 1));
    assert_eq!(crop.bgra(), [1, 1, 9, 255, 2, 1, 9, 255]);
    assert_eq!(crop.presented(), still.presented());
    assert_eq!(still.crop(0, 0, 3, 2).unwrap(), still);
    assert_eq!(still.crop(2, 0, 2, 1), None);
    assert_eq!(still.crop(0, 0, 0, 1), None);
    assert_eq!(still.crop(u32::MAX, 0, 2, 1), None, "no overflow");
}

#[test]
fn a_still_hands_its_pixels_over_as_rgba_or_as_a_surface_frame() {
    let still = still_3x2();
    assert_eq!(&still.to_rgba()[..4], &[9, 0, 0, 255]);
    let frame = still.into_byte_frame();
    assert_eq!((frame.width, frame.height, frame.format), (3, 2, Format::Bgra));
    assert_eq!(frame.planes[0].len(), 24);
    assert!(frame.planes[1].is_empty());
    crate::NativeSurface::new().push_bytes(frame, 1).expect("a NativeSurface takes a still's frame");
}

#[test]
fn a_frame_rate_cap_is_an_interval_and_none_is_zero() {
    assert_eq!(interval(4.0), Duration::from_millis(250));
    assert_eq!(interval(0.0), Duration::ZERO);
    assert_eq!(interval(f32::NAN), Duration::ZERO);
    assert_eq!(interval(-3.0), Duration::ZERO);
}

#[test]
fn the_cursor_is_left_out_unless_asked_for() {
    assert!(!Target::output("DP-2").cursor);
    assert!(Target::window(WindowAddress(7)).with_cursor().cursor);
}
