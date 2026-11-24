# Layer bar

A desktop panel with no app window: on every display, a bar docked along the top edge on the
layer-shell `Top` layer, reserving its height so the compositor tiles windows below it, and a
popup it toggles. Plugging a display in adds its bar; unplugging it removes it.

```sh
cargo run -p layer-bar             # stdin: toggle | quit
cargo run -p layer-bar -- --bottom
```

`toggle` opens and closes the popup on the primary display's bar, or the first display's where
none is primary (Wayland has no primary display).

```rust
let mut panels: Vec<Panel> = Vec::new();
app.on_displays_changed(move |displays, cx| {
    panels.retain(|panel| {
        let stays = displays.iter().any(|d| d.id.as_ref() == Some(&panel.id));
        if !stays { panel.bar.remove(); }
        stays
    });
    for display in displays {
        let Some(id) = display.id.clone() else { continue };
        if panels.iter().any(|panel| panel.id == id) { continue; }
        let bar = cx.overlay(OverlaySpec::new("layer-bar", (640.0, 32.0), bar).options(OverlayOptions {
            placement: Some(OverlayPlacement::Bar { edge: BarEdge::Top, thickness: 32.0, margin: 0.0 }),
            display: DisplayTarget::Id(id.clone()),
            wayland_layer: WaylandLayer::Top,
            exclusive_zone: ExclusiveZone::Reserve(32.0),
            input: OverlayInput::Regions,
            ..Default::default()
        }), state);
        bar.show();
        panels.push(Panel { id, bar });
    }
});
```

The handler runs once at startup with every display, and on Wayland again whenever one comes,
goes or changes. A display's `name` is its connector's (`DP-2`); one that is unplugged and
plugged in again keeps its name and gets a new `id`, so bars are kept by `id`. Stderr says
`layer-bar: added bar on DP-2` and `layer-bar: removed bar on DP-2`.

The reserved distance is in points, whatever the display's scale. On Hyprland,
`hyprctl layers` lists `layer-bar` on each display's top layer and `hyprctl monitors` shows
each display's reserved area grown by 32; `hyprctl output create headless` and
`hyprctl output remove HEADLESS-2` add and remove a display to try it with.
