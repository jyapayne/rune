use crate::{Cx, El, Env, Key, KeyEvent, Mods, Runtime, TextEdit, TextStyle, col, el, text_input};

#[derive(Default)]
struct Form {
    name: TextEdit,
    /// The field shows (content still loading hides it).
    shown: bool,
    /// A modal is open over the window.
    modal: bool,
}

fn button(key: &str) -> El {
    el().key(key).size(60.0, 30.0).focusable(true).role("button").label(key)
}

fn view(s: &Form, _: &mut Cx<Form>) -> El {
    let mut root = col().key("root").size(300.0, 200.0).child(button("list"));
    if s.shown {
        root = root.child(text_input(&s.name, TextStyle::new(14.0, 400.0)).key("name").size(200.0, 40.0).autofocus().on_edit(|s: &mut Form| &mut s.name));
    }
    if s.modal {
        root = root.child(col().key("dialog").absolute(0.0, 0.0).size(200.0, 100.0).role("dialog").label("Dialog").modal(true, |s: &mut Form| s.modal = false).child(button("cancel")));
    }
    root
}

fn window(form: Form) -> Runtime<Form> {
    let mut rt = Runtime::new(form, view, (300.0, 200.0), Env::new(true, 1.0));
    rt.frame(0.0);
    rt
}

fn key(rt: &mut Runtime<Form>, key: Key) {
    rt.key(KeyEvent { key, mods: Mods::default() });
    rt.frame(0.0);
}

fn focused(rt: &Runtime<Form>) -> Option<String> {
    rt.elements().into_iter().find(|e| e.focused).and_then(|e| e.key)
}

#[test]
fn a_window_opens_with_the_keyboard_in_its_autofocus_field() {
    let mut rt = window(Form { shown: true, ..Form::default() });
    assert_eq!(focused(&rt).as_deref(), Some("name"));
    key(&mut rt, Key::Text("a".into()));
    assert_eq!(rt.state.name.text, "a");
    assert!(!rt.kbd.focus.shown(), "no ring: autofocus isn't the keys moving focus");
}

#[test]
fn an_autofocus_field_that_shows_later_takes_focus_while_nothing_has_had_it() {
    let mut rt = window(Form::default());
    assert_eq!(focused(&rt), None);
    rt.update(|s, _| s.shown = true);
    rt.frame(0.0);
    assert_eq!(focused(&rt).as_deref(), Some("name"));
}

#[test]
fn once_anything_has_had_focus_autofocus_never_takes_it() {
    let mut rt = window(Form::default());
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("list"));
    rt.update(|s, _| s.shown = true);
    rt.frame(0.0);
    assert_eq!(focused(&rt).as_deref(), Some("list"));
}

#[test]
fn a_focus_the_app_asks_for_wins_over_autofocus() {
    let mut rt = Runtime::new(Form { shown: true, ..Form::default() }, view, (300.0, 200.0), Env::new(true, 1.0));
    rt.update(|_, tx| tx.focus("list", false));
    rt.frame(0.0);
    assert_eq!(focused(&rt).as_deref(), Some("list"));
}

#[test]
fn an_autofocus_control_behind_a_modal_waits_for_the_modal() {
    let mut rt = window(Form { shown: true, modal: true, ..Form::default() });
    assert_eq!(focused(&rt).as_deref(), Some("cancel"), "the modal focuses inside itself");
    key(&mut rt, Key::Escape);
    assert!(!rt.state.modal);
    assert_ne!(focused(&rt).as_deref(), Some("name"), "focus has been had: it returns, autofocus doesn't run");
}
