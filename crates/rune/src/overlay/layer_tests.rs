use super::press_on_surface;

/// Under a focus grab Hyprland keeps the pointer on the grabbed surface and reports where it
/// is from that surface's corner: over a bar above a popup it says `y = -32`, and the press
/// there is sent to the popup before the grab ends.
#[test]
fn a_press_the_compositor_reports_beyond_the_surface_is_not_on_it() {
    let size = (280.0, 140.0);
    assert!(press_on_surface(size, 1.0, (0.0, 0.0)));
    assert!(press_on_surface(size, 1.0, (279.5, 139.5)));
    assert!(!press_on_surface(size, 1.0, (253.0, -32.0)), "over the bar above it");
    assert!(!press_on_surface(size, 1.0, (-0.5, 70.0)));
    assert!(!press_on_surface(size, 1.0, (280.0, 70.0)), "the far edge is the first point outside");
    assert!(!press_on_surface(size, 1.0, (140.0, 140.0)));
    // The runtime's pointer is in the zoomed view's points: at 2x the surface is 140 x 70 of them.
    assert!(press_on_surface(size, 2.0, (139.0, 69.0)));
    assert!(!press_on_surface(size, 2.0, (141.0, 10.0)));
    assert!(!press_on_surface(size, 2.0, (10.0, 71.0)));
}
