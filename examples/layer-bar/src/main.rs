//! A desktop panel on Wayland layer-shell: on every display, a bar docked along the top edge on
//! the `Top` layer that reserves its height, so the compositor tiles windows below it, and a
//! popup it toggles. Bars follow the displays: plugging one in adds its bar, unplugging it
//! removes it. The app has no window of its own.
//!
//! ```sh
//! cargo run -p layer-bar                 # stdin: toggle | quit
//! cargo run -p layer-bar -- --bottom     # dock along the bottom edge
//! ```
//!
//! On a wlroots-style compositor, `hyprctl layers` (or the compositor's equivalent) lists
//! `layer-bar` on each display's top layer, and each display's reserved area grows by the
//! bar's height. Stderr says when a bar is added or removed.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::sync::{Arc, Mutex};

use rune::tok::{self, radius, space, type_};
use rune::{
    Align, App, BarEdge, ConfiguredOverlay, Cx, DisplayId, DisplayTarget, El, ExclusiveZone, KeyPolicy, OverlayHandle, OverlayInput,
    OverlayOptions, OverlayPlacement, OverlaySpec, ScreenEdge, WaylandLayer, col, el, row, text,
};

const THICKNESS: f32 = 32.0;

struct Bar {
    /// The display's connector name, such as `DP-2`.
    display: String,
    popup: OverlayHandle<Popup>,
    open: bool,
}

#[derive(Default)]
struct Popup {
    presses: u32,
}

/// One display's bar and the popup it toggles.
struct Panel {
    display: String,
    id: DisplayId,
    bar: OverlayHandle<Bar>,
    popup: OverlayHandle<Popup>,
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
        .child(text(format!("layer-bar on {}", s.display)).key("title").font(type_::LABEL).color(t.text))
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

fn bar_spec(edge: BarEdge, display: DisplayTarget) -> ConfiguredOverlay<Bar> {
    OverlaySpec::new("layer-bar", (640.0, THICKNESS), bar).options(OverlayOptions {
        placement: Some(OverlayPlacement::Bar { edge, thickness: THICKNESS, margin: 0.0 }),
        display,
        wayland_layer: WaylandLayer::Top,
        exclusive_zone: ExclusiveZone::Reserve(THICKNESS),
        input: OverlayInput::Regions,
        ..Default::default()
    })
}

fn popup_spec(edge: ScreenEdge, display: DisplayTarget) -> ConfiguredOverlay<Popup> {
    OverlaySpec::new("layer-bar-popup", (280.0, 140.0), popup).options(OverlayOptions {
        placement: Some(OverlayPlacement::Edge { edge, margin: 8.0, align: Align::End, inset: 8.0 }),
        display,
        input: OverlayInput::All,
        keys: KeyPolicy::OnDemand,
        prewarm: true,
        ..Default::default()
    })
}

fn main() {
    let bottom = std::env::args().any(|a| a == "--bottom");
    let (edge, popup_edge) = if bottom { (BarEdge::Bottom, ScreenEdge::Bottom) } else { (BarEdge::Top, ScreenEdge::Top) };
    // The bar stdin's `toggle` reaches: the primary display's, or the first's.
    let first: Arc<Mutex<Option<OverlayHandle<Bar>>>> = Arc::default();
    let toggled = first.clone();
    let mut panels: Vec<Panel> = Vec::new();
    let app = App::new(rune::AppSpec::<()>::new("layer-bar", (1.0, 1.0), || (), |_, _| el())).windowless().on_displays_changed(move |displays, cx| {
        // A display unplugged and plugged in again keeps its name and gets a new id: its old
        // bar's surface went with the old one.
        panels.retain(|panel| {
            let stays = displays.iter().any(|d| d.id.as_ref() == Some(&panel.id));
            if !stays {
                panel.bar.remove();
                panel.popup.remove();
                eprintln!("layer-bar: removed bar on {}", panel.display);
            }
            stays
        });
        for (i, display) in displays.iter().enumerate() {
            let Some(id) = display.id.clone() else { continue };
            if panels.iter().any(|panel| panel.id == id) { continue; }
            let name = display.name.clone().unwrap_or_else(|| format!("display {}", i + 1));
            let popup = cx.overlay(popup_spec(popup_edge, DisplayTarget::Id(id.clone())), Popup::default());
            let bar = cx.overlay(bar_spec(edge, DisplayTarget::Id(id.clone())), Bar { display: name.clone(), popup: popup.clone(), open: false });
            bar.show();
            eprintln!("layer-bar: added bar on {name}");
            panels.push(Panel { display: name, id, bar, popup });
        }
        let main = displays.iter().find(|d| d.primary).or(displays.first()).and_then(|d| d.id.as_ref());
        *first.lock().unwrap() = panels.iter().find(|panel| Some(&panel.id) == main).map(|panel| panel.bar.clone());
    });
    let quit = app.handle();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            match line.trim() {
                "toggle" => {
                    let bar = toggled.lock().unwrap().clone();
                    match bar {
                        Some(bar) => bar.update(|s, _| toggle(s)),
                        None => eprintln!("layer-bar: no display to toggle a popup on"),
                    }
                }
                "quit" => quit.quit(),
                "" => {}
                other => eprintln!("layer-bar: unknown command {other:?} (toggle | quit)"),
            }
        }
    });
    app.run();
}
