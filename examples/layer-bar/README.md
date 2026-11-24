# Layer bar

A desktop panel with no app window: a bar docked along the top edge on the layer-shell `Top`
layer, reserving its height so the compositor tiles windows below it, and a popup it toggles.

```sh
cargo run -p layer-bar             # stdin: toggle | quit
cargo run -p layer-bar -- --bottom
```

```rust
app.overlay(OverlaySpec::new("layer-bar", (640.0, 32.0), bar).options(OverlayOptions {
    placement: Some(OverlayPlacement::Bar { edge: BarEdge::Top, thickness: 32.0, margin: 0.0 }),
    wayland_layer: WaylandLayer::Top,
    exclusive_zone: ExclusiveZone::Reserve(32.0),
    input: OverlayInput::Regions,
    ..Default::default()
}), state);
```

The reserved distance is in points, whatever the display's scale. On Hyprland,
`hyprctl layers` lists `layer-bar` on the top layer and `hyprctl monitors` shows the display's
reserved area grown by 32.
