use super::*;

#[test]
fn a_wayland_identifier_names_its_exported_handle() {
    assert_eq!(wayland_handle("wayland:9b1c2d3e-handle"), Some("9b1c2d3e-handle"));
}

#[test]
fn other_identifiers_name_no_wayland_handle() {
    assert_eq!(wayland_handle("x11:4a00007"), None);
    assert_eq!(wayland_handle(""), None, "no parent at all");
    assert_eq!(wayland_handle("wayland:"), None, "an empty handle");
    assert_eq!(wayland_handle("handle-without-kind"), None);
}

#[test]
fn the_first_parent_set_is_kept() {
    set_parent_window("wayland:first", true);
    set_parent_window("wayland:second", false);
    assert_eq!(parent(), Some(&Parent { identifier: "wayland:first".to_owned(), modal: true }));
}
