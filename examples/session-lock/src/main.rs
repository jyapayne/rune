//! A lock screen on Wayland's `ext-session-lock-v1`: it locks the session at once, with a lock
//! surface on every display, including any plugged in while locked, and unlocks when the
//! passphrase is typed and Enter pressed. A lock surface shows Caps Lock and the keyboard
//! layout as the compositor reports them.
//!
//! ```sh
//! cargo run -p session-lock                                  # passphrase: rune
//! cargo run -p session-lock -- --passphrase hunter2
//! cargo run -p session-lock -- --unlock-after 10             # unlocks itself after 10 s too
//! ```
//!
//! Try it in a nested compositor first. Only the passphrase or `--unlock-after` unlocks the
//! session: killing this process leaves it locked, and Hyprland then shows its "lock dead"
//! screen until another lock client takes over (`misc:allow_session_lock_restore`). Stderr
//! says what the lock does and when a surface is added or removed.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::tok::type_;
use rune::{
    App, Color, Cx, DisplayId, DisplayTarget, El, Key, KeyEvent, KeyboardState, LockState, OverlayHandle, OverlayInput, OverlayOptions,
    OverlaySpec, SessionLock, Tx, col, el, row, text,
};

/// One display's lock surface.
struct Surface {
    lock: SessionLock,
    passphrase: Arc<str>,
    typed: String,
    wrong: bool,
    keyboard: KeyboardState,
}

fn on_key(s: &mut Surface, event: &KeyEvent, _: &mut Tx) -> bool {
    match &event.key {
        Key::Enter => {
            s.wrong = s.typed != *s.passphrase;
            if !s.wrong {
                eprintln!("session-lock: passphrase accepted, unlocking");
                s.lock.unlock();
            }
            s.typed.clear();
        }
        Key::Backspace => {
            s.typed.pop();
        }
        Key::Escape => s.typed.clear(),
        Key::Text(typed) => {
            s.typed.push_str(typed);
            s.wrong = false;
        }
        _ => {}
    }
    true
}

fn on_keyboard_state(s: &mut Surface, state: &KeyboardState, _: &mut Tx) {
    s.keyboard = state.clone();
}

fn view(s: &Surface, _: &mut Cx<Surface>) -> El {
    let dots = row().gap(10.0).children((0..s.typed.chars().count()).map(|_| el().size(12.0, 12.0).radius(6.0).fill(Color::WHITE)));
    let mut hints = Vec::new();
    if s.wrong { hints.push("Wrong passphrase".to_owned()); }
    if s.keyboard.caps_lock { hints.push("Caps Lock is on".to_owned()); }
    if let Some(layout) = &s.keyboard.layout { hints.push(layout.clone()); }
    el().w_full().h_full().center().fill(rune::oklch(0.16, 0.02, 260.0)).child(
        col()
            .gap(18.0)
            .items_center()
            .child(text("Locked").font(type_::TITLE).color(Color::WHITE))
            .child(el().h(24.0).center().child(dots))
            .child(text(hints.join("  ·  ")).font(type_::BODY).color(rune::oklch(0.82, 0.02, 260.0))),
    )
}

fn spec(lock: &SessionLock, display: DisplayTarget) -> rune::ConfiguredOverlay<Surface> {
    // The size is the compositor's: each lock surface covers its display.
    OverlaySpec::new("session-lock", (640.0, 480.0), view)
        .options(OverlayOptions { display, input: OverlayInput::All, ..Default::default() })
        .session_lock(lock)
        .on_key(on_key)
        .on_keyboard_state(on_keyboard_state)
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|at| args.get(at + 1).cloned())
}

fn main() {
    let passphrase: Arc<str> = arg("--passphrase").unwrap_or_else(|| "rune".to_owned()).into();
    let unlock_after = arg("--unlock-after").and_then(|s| s.parse::<u64>().ok());
    let lock_slot: Arc<Mutex<Option<SessionLock>>> = Arc::default();
    let slot = lock_slot.clone();
    let mut surfaces: Vec<(DisplayId, String, OverlayHandle<Surface>)> = Vec::new();
    let app = App::new(rune::AppSpec::<()>::new("session-lock", (1.0, 1.0), || (), |_, _| el())).windowless().on_displays_changed(move |displays, cx| {
        let Some(lock) = slot.lock().unwrap().clone() else { return };
        if lock.state().ended() { return; }
        surfaces.retain(|(id, name, surface)| {
            let stays = displays.iter().any(|d| d.id.as_ref() == Some(id));
            if !stays {
                surface.remove();
                eprintln!("session-lock: removed the lock surface on {name}");
            }
            stays
        });
        for (i, display) in displays.iter().enumerate() {
            let Some(id) = display.id.clone() else { continue };
            if surfaces.iter().any(|(have, ..)| *have == id) { continue; }
            let name = display.name.clone().unwrap_or_else(|| format!("display {}", i + 1));
            let state = Surface { lock: lock.clone(), passphrase: passphrase.clone(), typed: String::new(), wrong: false, keyboard: KeyboardState::default() };
            let surface = cx.overlay(spec(&lock, DisplayTarget::Id(id.clone())), state);
            surface.show();
            eprintln!("session-lock: added a lock surface on {name}");
            surfaces.push((id, name, surface));
        }
    });
    let handle = app.handle();
    let quit = handle.clone();
    let lock = handle.lock_session(move |state| {
        eprintln!("session-lock: {state:?}");
        if state.ended() {
            // Let the compositor read the unlock before the connection closes.
            let quit = quit.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                quit.quit();
            });
        }
    });
    *lock_slot.lock().unwrap() = Some(lock.clone());
    if let Some(seconds) = unlock_after {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            eprintln!("session-lock: --unlock-after reached, unlocking");
            lock.unlock();
        });
    }
    app.run();
    if matches!(lock_slot.lock().unwrap().as_ref().map(SessionLock::state), Some(LockState::Locked | LockState::Pending)) {
        eprintln!("session-lock: exiting while locked; the session stays locked");
    }
}
