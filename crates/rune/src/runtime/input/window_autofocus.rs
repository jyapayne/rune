//! `El::autofocus` outside a modal, as HTML's `autofocus` on a page: until anything in the window
//! has had focus, the first shown, focusable `El::autofocus` control takes it as it shows (a save
//! dialog's name field, a search window's field), without a ring. Once anything has had focus,
//! from the user, the app or this, it never runs again, as a page's autofocus runs once. A modal's
//! own `El::autofocus` is `modal_focus`'s, each time one opens.
use super::*;

/// Whether the window's autofocus is still to come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WindowAutofocus {
    /// Nothing in the window has had focus yet.
    #[default]
    Waiting,
    /// Something has.
    Done,
}

impl RuntimeCore {
    /// Gives focus to the window's first shown `El::autofocus` control while nothing has had it,
    /// after this commit's requested focus (`Tx::focus`, a field's `TextEdit::focused`) and its
    /// modal have had their turn.
    pub(in crate::runtime) fn sync_window_autofocus(&mut self, app: &mut dyn AppDriver) {
        if self.kbd.window_autofocus == WindowAutofocus::Done {
            return;
        }
        if self.kbd.has_focus() {
            self.kbd.window_autofocus = WindowAutofocus::Done;
            return;
        }
        // An open modal focuses inside itself; nothing behind it may take focus.
        if self.active_modal().is_some() {
            return;
        }
        let Some(j) = (0..self.tree.nodes.len()).find(|&j| self.tree.nodes[j].focus.autofocus && self.can_focus(j)) else { return };
        let path = self.tree.nodes[j].path.clone();
        self.set_focus(app, Some(path), false);
        self.kbd.window_autofocus = WindowAutofocus::Done;
    }
}

#[cfg(test)]
#[path = "window_autofocus_tests.rs"]
mod tests;
