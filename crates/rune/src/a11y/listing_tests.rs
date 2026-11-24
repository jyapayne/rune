use super::{Listing, listing};

#[test]
fn a_hidden_overlay_leaves_the_accessibility_bus_where_hidden_surfaces_would_be_listed() {
    // A prewarmed overlay is made hidden: it has no adapter, and gets none until it shows.
    assert_eq!(listing(true, false, false), Listing::Keep);
    assert_eq!(listing(true, true, false), Listing::Join, "shown, it joins");
    assert_eq!(listing(true, true, true), Listing::Keep, "one adapter for one show");
    assert_eq!(listing(true, false, true), Listing::Leave, "hidden, it leaves");
    assert_eq!(listing(true, false, false), Listing::Keep, "and stays off while hidden");
}

#[test]
fn an_overlay_keeps_its_adapter_where_the_platform_already_drops_hidden_windows() {
    for shown in [false, true] {
        for listed in [false, true] {
            assert_eq!(listing(false, shown, listed), Listing::Keep, "shown {shown}, listed {listed}");
        }
    }
}
