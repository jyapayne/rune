//! A desktop panel on Wayland layer-shell: a bar docked along the top edge on the `Top` layer
//! that reserves its height, so the compositor tiles windows below it, and a popup it toggles.
//! The app has no window of its own.
//!
//! ```sh
//! cargo run -p layer-bar                 # stdin: toggle | reserve N | quit
//! cargo run -p layer-bar -- --bottom     # dock along the bottom edge
//! ```
//!
//! On a wlroots-style compositor, `hyprctl layers` (or the compositor's equivalent) lists
//! `layer-bar` on the top layer, and the display's reserved area grows by the bar's height.

#![forbid(unsafe_code)]

use std::io::BufRead;

use rune::tok::{self, radius, space, type_};
use rune::{Align, App, BarEdge, Cx, El, ExclusiveZone, KeyPolicy, OverlayHandle, OverlayInput, OverlayOptions, OverlayPlacement, OverlaySpec, ScreenEdge, WaylandLayer, col, el, row, text};

const THICKNESS: f32 = 32.0;

struct Bar {
    popup: OverlayHandle<Popup>,
    open: bool,
}

#[derive(Default)]
struct Popup {
    presses: u32,
}

fn toggle(bar: &mut Bar) {
    bar.open = !bar.open;
    if bar.open { bar.popup.show() } else { bar.popup.hide() }
}

fn bar(s: &Bar, cx: &mut Cx<Bar>) -> El {
    let t = tok::theme(cx.system_dark());
    let accent = tok::accent(0, cx.system_dark());
    row()
        .key("bar")
        .w_full()
        .h_full()
        .px(space::S3)
        .gap(space::S3)
        .center_y()
        .fill(t.bg)
        .child(el().key("mark").size(10.0, 10.0).radius(5.0).fill(accent))
        .child(text("layer-bar").key("title").font(type_::LABEL).color(t.text))
        .child(el().key("spring").grow(1.0))
        .child(
            row()
                .key("toggle")
                .role("button")
                .label("Toggle popup")
                .hit()
                .h(22.0)
                .px(space::S3)
                .center()
                .radius(radius::MD)
                .fill(if s.open { accent } else { t.control_off })
                .hover(t.hover)
                .on_press(toggle)
                .child(text(if s.open { "Close" } else { "Open" }).font(type_::LABEL).color(t.text)),
        )
}

fn popup(s: &Popup, cx: &mut Cx<Popup>) -> El {
    let t = tok::theme(cx.system_dark());
    col().key("popup").w_full().h_full().px(space::S6).py(space::S6).gap(space::S3).radius(radius::LG).fill(t.card).border(1.0, t.card_border)
        .child(text("Popup").key("heading").font(type_::TITLE).color(t.text))
        .child(text(format!("Pressed {} times", s.presses)).key("presses").font(type_::BODY).color(t.text_secondary))
        .child(
            row().key("press").role("button").label("Press").h(28.0).px(space::S3).center().radius(radius::MD).fill(t.control_off).hover(t.hover)
                .on_press(|s: &mut Popup| s.presses += 1)
                .child(text("Press").font(type_::LABEL).color(t.text)),
        )
}

fn main() {
    let bottom = std::env::args().any(|a| a == "--bottom");
    let edge = if bottom { BarEdge::Bottom } else { BarEdge::Top };
    let mut app = App::new(rune::AppSpec::<()>::new("layer-bar", (1.0, 1.0), || (), |_, _| el())).windowless();
    let popup = app.overlay(
        OverlaySpec::new("layer-bar-popup", (280.0, 140.0), popup).options(OverlayOptions {
            placement: Some(OverlayPlacement::Edge { edge: if bottom { ScreenEdge::Bottom } else { ScreenEdge::Top }, margin: 8.0, align: Align::End, inset: 8.0 }),
            input: OverlayInput::All,
            keys: KeyPolicy::OnDemand,
            prewarm: true,
            ..Default::default()
        }),
        Popup::default(),
    );
    let bar = app.overlay(
        OverlaySpec::new("layer-bar", (640.0, THICKNESS), bar).options(OverlayOptions {
            placement: Some(OverlayPlacement::Bar { edge, thickness: THICKNESS, margin: 0.0 }),
            wayland_layer: WaylandLayer::Top,
            exclusive_zone: ExclusiveZone::Reserve(THICKNESS),
            input: OverlayInput::Regions,
            ..Default::default()
        }),
        Bar { popup: popup.clone(), open: false },
    );
    bar.show();
    let quit = app.handle();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            match line.trim() {
                "toggle" => bar.update(|s, _| toggle(s)),
                "quit" => quit.quit(),
                "" => {}
                other => eprintln!("layer-bar: unknown command {other:?} (toggle | quit)"),
            }
        }
    });
    app.run();
}
