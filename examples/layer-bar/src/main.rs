//! A desktop panel on Wayland layer-shell: on every display, a bar docked along the top edge on
//! the `Top` layer that reserves its height, so the compositor tiles windows below it, and a
//! popup it toggles. Bars follow the displays: plugging one in adds its bar, unplugging it
//! removes it. The app has no window of its own.
//!
//! The popup is a launcher's prompt: it takes every key while shown (`KeyPolicy::Exclusive`),
//! so typing reaches its field with no click, Escape closes it, and so does a press anywhere
//! outside it. It is prewarmed, so closing it unmaps its surface and opening it maps the same
//! one again.
//!
//! ```sh
//! cargo run -p layer-bar                 # stdin: toggle | status | quit
//! cargo run -p layer-bar -- --bottom     # dock along the bottom edge
//! ```
//!
//! On a wlroots-style compositor, `hyprctl layers` (or the compositor's equivalent) lists
//! `layer-bar` on each display's top layer, and each display's reserved area grows by the
//! bar's height. Stderr says when a bar is added or removed.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::tok::{self, radius, space, type_};
use rune::{
    Align, App, BarEdge, ConfiguredOverlay, Cx, DisplayId, DisplayTarget, El, ExclusiveZone, Key, KeyEvent, KeyPolicy, OverlayHandle,
    OverlayInput, OverlayOptions, OverlayPlacement, OverlaySpec, ScreenEdge, TextEdit, Tx, WaylandLayer, col, el, row, text, text_input,
};

const THICKNESS: f32 = 32.0;

/// How long after a press outside the popup closed it a press on the bar's own button leaves
/// it closed: that same press reaches the button next, and would open it again.
const SAME_PRESS: Duration = Duration::from_millis(300);

struct Bar {
    /// The display's connector name, such as `DP-2`.
    display: String,
    popup: OverlayHandle<Popup>,
    open: bool,
    /// When a press outside the popup last closed it.
    pressed_away: Option<Instant>,
}

#[derive(Default)]
struct Popup {
    query: TextEdit,
    /// The popup's own handle and its bar's, to close itself and say so; set once both exist.
    me: Option<OverlayHandle<Popup>>,
    bar: Option<OverlayHandle<Bar>>,
}

/// One display's bar and the popup it toggles.
struct Panel {
    display: String,
    id: DisplayId,
    bar: OverlayHandle<Bar>,
    popup: OverlayHandle<Popup>,
}

fn set_open(bar: &mut Bar, open: bool) {
    bar.open = open;
    bar.pressed_away = None;
    if open {
        // The field takes the keys the popup is given as it shows.
        bar.popup.update(|s, _| {
            s.query.clear();
            s.query.focused = true;
        });
        bar.popup.show();
    } else {
        bar.popup.hide();
    }
}

/// A press on the bar's button.
fn press(bar: &mut Bar) {
    if !bar.open && bar.pressed_away.take().is_some_and(|at| at.elapsed() < SAME_PRESS) { return; }
    set_open(bar, !bar.open);
}

/// The popup closes itself and tells its bar. `pressed_away`: a press outside it did.
fn dismiss(s: &mut Popup, pressed_away: bool) {
    if let Some(me) = &s.me { me.hide(); }
    if let Some(bar) = &s.bar {
        bar.update(move |bar, _| {
            bar.open = false;
            bar.pressed_away = pressed_away.then(Instant::now);
        });
    }
}

/// Escape closes the popup: as the field's `on_input_key` while it has the focus (the field
/// alone would only blur), and as the overlay's `on_key` when nothing has.
fn popup_key(s: &mut Popup, event: &KeyEvent, _: &mut Tx) -> bool {
    if event.key != Key::Escape { return false; }
    dismiss(s, false);
    true
}

/// Focus left the popup: the keyboard went elsewhere, or the user pressed outside it and the
/// compositor ended the focus grab that guards a `KeyPolicy::Exclusive` overlay.
fn popup_focus(s: &mut Popup, focused: bool, _: &mut Tx) {
    if !focused { dismiss(s, true); }
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
                .on_press(press)
                .child(text(if s.open { "Close" } else { "Open" }).font(type_::LABEL).color(t.text)),
        )
}

fn popup(s: &Popup, cx: &mut Cx<Popup>) -> El {
    let t = tok::theme(cx.system_dark());
    col().key("popup").w_full().h_full().px(space::S6).py(space::S6).gap(space::S3).radius(radius::LG).fill(t.card).border(1.0, t.card_border)
        // A press on the popup's own padding would blur the field: the keys stay with it.
        .on_press(|s: &mut Popup| s.query.focused = true)
        .child(text("Run").key("heading").font(type_::TITLE).color(t.text))
        .child(
            el().key("query-field").w_full().h(32.0).px(space::S2).radius(radius::MD).fill(t.bg).border(1.0, t.card_border).center_y()
                .child(
                    text_input(&s.query, type_::BODY)
                        .key("query")
                        .label("Command")
                        .single_line()
                        .placeholder("Type a command", t.text_secondary)
                        .color(t.text)
                        .on_edit(|s: &mut Popup| &mut s.query)
                        .on_input_key(popup_key),
                ),
        )
        .child(text("Escape or a press outside closes").key("hint").font(type_::LABEL).color(t.text_secondary))
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
    OverlaySpec::new("layer-bar-popup", (280.0, 140.0), popup)
        .options(OverlayOptions {
            placement: Some(OverlayPlacement::Edge { edge, margin: 8.0, align: Align::End, inset: 8.0 }),
            display,
            input: OverlayInput::All,
            keys: KeyPolicy::Exclusive,
            prewarm: true,
            ..Default::default()
        })
        .on_key(popup_key)
        .on_focus(popup_focus)
}

fn main() {
    let bottom = std::env::args().any(|a| a == "--bottom");
    let (edge, popup_edge) = if bottom { (BarEdge::Bottom, ScreenEdge::Bottom) } else { (BarEdge::Top, ScreenEdge::Top) };
    // The bar stdin's commands reach: the primary display's, or the first's.
    let first: Arc<Mutex<Option<OverlayHandle<Bar>>>> = Arc::default();
    let reached = first.clone();
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
            let bar = cx.overlay(
                bar_spec(edge, DisplayTarget::Id(id.clone())),
                Bar { display: name.clone(), popup: popup.clone(), open: false, pressed_away: None },
            );
            let (me, its_bar) = (popup.clone(), bar.clone());
            popup.update(move |s, _| {
                s.me = Some(me);
                s.bar = Some(its_bar);
            });
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
            let command = line.trim();
            if command == "quit" { quit.quit(); continue; }
            if command.is_empty() { continue; }
            let Some(bar) = reached.lock().unwrap().clone() else {
                eprintln!("layer-bar: no display with a bar");
                continue;
            };
            match command {
                "toggle" => bar.update(|s, _| set_open(s, !s.open)),
                // Nothing the views read changes: neither overlay redraws for it.
                "status" => bar.update_if(|s, _| {
                    let open = s.open;
                    s.popup.update_if(move |popup, _| {
                        eprintln!("layer-bar: popup {}, query {:?}", if open { "open" } else { "closed" }, popup.query.text);
                        false
                    });
                    false
                }),
                other => eprintln!("layer-bar: unknown command {other:?} (toggle | status | quit)"),
            }
        }
    });
    app.run();
}
