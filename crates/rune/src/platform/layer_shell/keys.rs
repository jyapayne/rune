//! The layer surface's keyboard: `wl_keyboard` dispatch, the XKB keymap and compose state, and
//! the translation of a key into Rune's physical and logical key events. Focus, repeat and
//! compose decisions live in `layer_shell_machine`.

use std::os::fd::OwnedFd;

use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::super::layer_shell_machine::{keyboard, repeat};
use super::{Input, State};

impl State {
    fn emit_key(&mut self, key: u32, pressed: bool, repeat: bool) {
        use winit::platform::scancode::PhysicalKeyExtScancode;
        let Some(xkb) = &self.xkb else { return };
        let physical = match crate::PhysicalKeyCode::from_scancode(key) {
            winit::keyboard::PhysicalKey::Code(code) => Some(code),
            _ => None,
        };
        let mods = key_mods(xkb);
        let compose_action = keyboard::compose_action(pressed, repeat, mods);
        let fallback = key_event(xkb, self.compose.for_key(compose_action), key, physical, pressed, repeat);
        if let Some(code) = physical {
            self.input.push(Input::Physical(
                crate::PhysicalInput::Key { key: code, mods, pressed, repeat },
                fallback,
            ));
        } else if let Some(key) = fallback {
            self.input.push(match (pressed, repeat) {
                (true, true) => Input::KeyRepeat(key),
                (true, false) => Input::Key(key),
                (false, _) => Input::KeyUp(key),
            });
        }
    }

    pub(super) fn fire_repeat(&mut self, attempt: crate::machine::Attempt, now: crate::time::Instant) {
        for effect in repeat::step(&mut self.repeat_seq, &mut self.repeat, repeat::Event::Due { attempt, now }) {
            match effect {
                repeat::Effect::Repeat { key } => {
                    if self.keyboard_focus.focused() {
                        self.emit_key(key, true, true);
                    }
                }
                repeat::Effect::WakeAt(_) => {}
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use xkbcommon::xkb;
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                state.step_repeat(repeat::Event::Stop);
                state.reset_compose();
                let focus_event = if state.surface.as_ref() == Some(&surface) {
                    keyboard::FocusEvent::EnterOurs
                } else {
                    keyboard::FocusEvent::EnterOther
                };
                state.step_keyboard_focus(focus_event);
            }
            wl_keyboard::Event::Leave { surface, .. } if state.surface.as_ref() == Some(&surface) => {
                state.step_repeat(repeat::Event::Stop);
                state.reset_compose();
                state.step_keyboard_focus(keyboard::FocusEvent::LeaveOurs);
            }
            wl_keyboard::Event::Keymap { format: wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                if let Some(map) = read_keymap_fd(fd, size) {
                    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                    state.xkb = xkb::Keymap::new_from_string(&context, map, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::COMPILE_NO_FLAGS).map(|map| {
                        let mut xkb_state = xkb::State::new(&map);
                        if let Some(m) = state.mod_state {
                            xkb_state.update_mask(m.depressed, m.latched, m.locked, 0, 0, m.group);
                        }
                        xkb_state
                    });
                    state.compose.on_keymap(state.xkb.is_some());
                    if let Some(held) = state.repeat.key()
                        && !state.xkb.as_ref().is_some_and(|xkb| xkb.get_keymap().key_repeats(xkb::Keycode::new(held + 8)))
                    {
                        state.step_repeat(repeat::Event::Stop);
                    }
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                state.mod_state = Some(keyboard::ModState { depressed: mods_depressed, latched: mods_latched, locked: mods_locked, group });
                if let Some(xkb) = &mut state.xkb {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    state.input.push(Input::Physical(crate::PhysicalInput::Modifiers(key_mods(xkb)), None));
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.step_repeat(repeat::Event::RepeatInfo { rate, delay_ms: delay });
            }
            wl_keyboard::Event::Key { key, state: wayland_client::WEnum::Value(key_state), .. } if state.keyboard_focus.focused() => {
                match key_state {
                    wl_keyboard::KeyState::Pressed => {
                        state.emit_key(key, true, false);
                        let repeats = state.xkb.as_ref().is_some_and(|xkb| xkb.get_keymap().key_repeats(xkb::Keycode::new(key + 8)));
                        state.step_repeat(repeat::Event::Press { key, repeats, now: crate::time::now() });
                    }
                    wl_keyboard::KeyState::Released => {
                        state.emit_key(key, false, false);
                        state.step_repeat(repeat::Event::Release { key });
                    }
                    wl_keyboard::KeyState::Repeated => {
                        state.emit_key(key, true, true);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Reads a Wayland `wl_keyboard.keymap` fd from offset `0` without touching the open file
/// description's file offset. Compositors such as Hyprland and niri share one open file
/// description across clients and binds, so sequential `read()` advances the shared offset and
/// leaves later readers at EOF.
pub(super) fn read_keymap_fd(fd: OwnedFd, size: u32) -> Option<String> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::from(fd);
    keyboard::decode_keymap(size, |buf, offset| file.read_exact_at(buf, offset))
}

pub(super) fn key_mods(state: &xkbcommon::xkb::State) -> crate::Mods {
    use xkbcommon::xkb;
    let active = |name: &str| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
    crate::input::key_table::mods_from_keys(active(xkb::MOD_NAME_SHIFT), active(xkb::MOD_NAME_CTRL), active(xkb::MOD_NAME_ALT), active(xkb::MOD_NAME_LOGO))
}

pub(super) fn feed_compose(compose: &mut xkbcommon::xkb::compose::State, sym: xkbcommon::xkb::Keysym) -> keyboard::ComposeOutcome {
    use xkbcommon::xkb::compose::{FeedResult, Status};
    match compose.feed(sym) {
        FeedResult::Ignored => keyboard::ComposeOutcome::Nothing,
        FeedResult::Accepted => match compose.status() {
            Status::Nothing => keyboard::ComposeOutcome::Nothing,
            Status::Composing => keyboard::ComposeOutcome::Composing,
            Status::Composed => {
                let text = compose.utf8();
                compose.reset();
                keyboard::ComposeOutcome::Composed(text)
            }
            Status::Cancelled => {
                compose.reset();
                keyboard::ComposeOutcome::Cancelled
            }
        },
    }
}

fn key_event(
    state: &xkbcommon::xkb::State,
    compose: Option<&mut xkbcommon::xkb::compose::State>,
    key: u32,
    physical: Option<crate::PhysicalKeyCode>,
    pressed: bool,
    repeat: bool,
) -> Option<crate::KeyEvent> {
    use xkbcommon::xkb;
    let code = xkb::Keycode::new(key + 8);
    let sym = state.key_get_one_sym(code);
    let mods = key_mods(state);
    let outcome = match (compose, keyboard::compose_action(pressed, repeat, mods)) {
        (Some(compose), keyboard::ComposeAction::Feed) => feed_compose(compose, sym),
        (Some(compose), keyboard::ComposeAction::ResetAndBypass) => {
            compose.reset();
            keyboard::ComposeOutcome::Nothing
        }
        (_, keyboard::ComposeAction::Bypass) | (None, _) => keyboard::ComposeOutcome::Nothing,
    };
    let text = state.key_get_utf8(code);
    keyboard::key_event_with_compose(outcome, named_keysym(sym.raw()), xkb::keysym_to_utf32(sym), text, mods, physical)
}

/// The named key an XKB keysym is, or None for one that types (or nothing Rune names).
pub(super) fn named_keysym(symbol: u32) -> Option<crate::Key> {
    Some(match symbol {
        0xff1b => crate::Key::Escape, 0xff0d | 0xff8d => crate::Key::Enter, 0xff08 => crate::Key::Backspace,
        0xffff | 0xff9f => crate::Key::Delete, 0xff09 | 0xfe20 | 0xff89 => crate::Key::Tab,
        0xff50 | 0xff95 => crate::Key::Home, 0xff57 | 0xff9c => crate::Key::End,
        0xff55 | 0xff9a => crate::Key::PageUp, 0xff56 | 0xff9b => crate::Key::PageDown,
        0xff51 | 0xff96 => crate::Key::Left, 0xff52 | 0xff97 => crate::Key::Up,
        0xff53 | 0xff98 => crate::Key::Right, 0xff54 | 0xff99 => crate::Key::Down,
        0xffbe..=0xffd5 => crate::Key::Function((symbol - 0xffbe + 1) as u8),
        0xff91..=0xff94 => crate::Key::Function((symbol - 0xff91 + 1) as u8),
        0xff67 => crate::Key::ContextMenu,
        0xff63 | 0xff9e => crate::Key::Insert, 0xff61 | 0xff15 => crate::Key::PrintScreen,
        0xff14 => crate::Key::ScrollLock, 0xff13 | 0xff6b => crate::Key::Pause,
        _ => return None,
    })
}

