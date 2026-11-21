//! Single-pane navigation for narrow (phone) screens, Telegram style: the chat list is the
//! home screen, a tapped chat fills the screen, and a back button (or the browser's back
//! gesture) returns to the list. Wide windows keep the sidebar and chat side by side.

use crate::history;

/// Below this width (logical points) only one pane is visible at a time.
pub const NARROW_BREAKPOINT: f32 = 600.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Sidebar and chat side by side.
    Wide,
    /// Narrow: the chat list fills the screen.
    List,
    /// Narrow: the open chat fills the screen.
    Chat,
}

#[derive(Default)]
pub struct NarrowNav {
    narrow: bool,
    chat_open: bool,
    /// In-chat search results (shown in the list pane) are on screen, not the chat itself.
    results_open: bool,
    search_was_active: bool,
}

impl NarrowNav {
    /// Folds this frame's inputs into the navigation state. Returns true when the chat pane was
    /// just left through browser navigation, so the caller can also drop chat-local state.
    pub fn begin_frame(&mut self, width: f32, search_active: bool) -> bool {
        self.narrow = width < NARROW_BREAKPOINT;
        if search_active && !self.search_was_active {
            self.results_open = true;
        }
        self.search_was_active = search_active;
        let popped = history::take_popped();
        let left_chat = popped && self.chat_open;
        if popped {
            self.chat_open = false;
        }
        left_chat
    }

    pub fn mode(&self, has_chat: bool, search_active: bool) -> Mode {
        if !self.narrow {
            return Mode::Wide;
        }
        let showing_results = search_active && self.results_open;
        if self.chat_open && has_chat && !showing_results {
            Mode::Chat
        } else {
            Mode::List
        }
    }

    /// A chat was picked in the list.
    pub fn open_chat(&mut self) {
        self.results_open = false;
        if !self.narrow || self.chat_open {
            return;
        }
        self.chat_open = true;
        history::push_chat();
    }

    /// A search hit was picked: leave the results and show the chat at that message.
    pub fn show_chat_after_search(&mut self) {
        self.results_open = false;
    }

    /// The in-app back button. In the browser the history entry is popped and the UI follows
    /// on the `popstate` event; without history the pane closes right away.
    pub fn back(&mut self) {
        if history::back() {
            return;
        }
        self.chat_open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrow_nav() -> NarrowNav {
        let mut nav = NarrowNav::default();
        nav.begin_frame(NARROW_BREAKPOINT - 1.0, false);
        nav
    }

    #[test]
    fn wide_screens_show_both_panes() {
        let mut nav = NarrowNav::default();
        nav.begin_frame(NARROW_BREAKPOINT, false);
        assert_eq!(nav.mode(true, false), Mode::Wide);
    }

    #[test]
    fn narrow_starts_on_the_list_even_with_a_restored_chat() {
        assert_eq!(narrow_nav().mode(true, false), Mode::List);
    }

    #[test]
    fn opening_and_leaving_a_chat() {
        let mut nav = narrow_nav();
        nav.open_chat();
        assert_eq!(nav.mode(true, false), Mode::Chat);
        nav.back();
        assert_eq!(nav.mode(true, false), Mode::List);
    }

    #[test]
    fn search_results_replace_the_chat_until_a_hit_is_picked() {
        let mut nav = narrow_nav();
        nav.open_chat();
        nav.begin_frame(NARROW_BREAKPOINT - 1.0, true);
        assert_eq!(nav.mode(true, true), Mode::List);
        nav.show_chat_after_search();
        assert_eq!(nav.mode(true, true), Mode::Chat);
    }
}
