//! State for things that appear directly in the UI

use crate::tui::{
    input::InputTarget,
    view::{ProfileListPane, RecipeListPane, RequestPane, ResponsePane},
};
use chrono::{DateTime, Duration, Utc};
use ratatui::widgets::*;
use std::{cell::RefCell, fmt::Display, ops::DerefMut};
use strum::{EnumIter, IntoEnumIterator};

/// A notification is an ephemeral informational message generated by some async
/// action. It doesn't grab focus, but will be useful to the user nonetheless.
/// It should be shown for a short period of time, then disappear on its own.
#[derive(Debug)]
pub struct Notification {
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    /// Amount of time a notification stays on screen before disappearing
    const NOTIFICATION_DECAY: Duration = Duration::milliseconds(5000);

    /// Has this notification overstayed its welcome?
    pub fn expired(&self) -> bool {
        Utc::now() - self.timestamp >= Self::NOTIFICATION_DECAY
    }
}

/// A list of items in the UI
#[derive(Debug)]
pub struct StatefulList<T> {
    /// Use interior mutability because this needs to be modified during the
    /// draw phase, by [Frame::render_stateful_widget]. This means we don't
    /// have to pass a mutable reference to [AppState] everywhere during
    /// the draw phase just so list state can be modified.
    state: RefCell<ListState>,
    pub items: Vec<T>,
}

impl<T> StatefulList<T> {
    pub fn with_items(items: Vec<T>) -> StatefulList<T> {
        let mut state = ListState::default();
        // Pre-select the first item if possible
        if !items.is_empty() {
            state.select(Some(0));
        }
        StatefulList {
            state: RefCell::new(state),
            items,
        }
    }

    /// Get the currently selected item (if any)
    pub fn selected(&self) -> Option<&T> {
        self.items.get(self.state.borrow().selected()?)
    }

    /// Get a mutable reference to state. This uses `RefCell` underneath so it
    /// will panic if aliased. Only call this during the draw phase!
    pub fn state_mut(&self) -> impl DerefMut<Target = ListState> + '_ {
        self.state.borrow_mut()
    }

    /// Select the previous item in the list. This should only be called during
    /// the message phase, so we can take `&mut self`.
    pub fn previous(&mut self) {
        let state = self.state.get_mut();
        let i = match state.selected() {
            Some(i) => {
                // Avoid underflow here
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        state.select(Some(i));
    }

    /// Select the next item in the list. This should only be called during the
    /// message phase, so we can take `&mut self`.
    pub fn next(&mut self) {
        let state = self.state.get_mut();
        let i = match state.selected() {
            Some(i) => (i + 1) % self.items.len(),
            None => 0,
        };
        state.select(Some(i));
    }
}

/// A fixed-size collection of selectable items, e.g. panes or tabs. User can
/// cycle between them.
#[derive(Debug)]
pub struct StatefulSelect<T: FixedSelect> {
    values: Vec<T>,
    selected: usize,
}

/// Friendly little trait indicating a type can be cycled through, e.g. a set
/// of panes or tabs
pub trait FixedSelect: Display + IntoEnumIterator + PartialEq {
    /// Initial item to select
    const DEFAULT_INDEX: usize = 0;
}

impl<T: FixedSelect> StatefulSelect<T> {
    pub fn new() -> Self {
        let values: Vec<T> = T::iter().collect();
        if values.is_empty() {
            panic!("Cannot create StatefulSelect from empty values");
        }
        Self {
            values,
            selected: T::DEFAULT_INDEX,
        }
    }

    /// Get the index of the selected element
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Get the selected element
    pub fn selected(&self) -> &T {
        &self.values[self.selected]
    }

    /// Is the given item selected?
    pub fn is_selected(&self, item: &T) -> bool {
        self.selected() == item
    }

    /// Select previous item
    pub fn previous(&mut self) {
        // Prevent underflow
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.values.len() - 1);
    }

    /// Select next item
    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.values.len();
    }
}

impl<T: FixedSelect> Default for StatefulSelect<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Copy, Clone, Debug, derive_more::Display, EnumIter, PartialEq)]
pub enum PrimaryPane {
    #[display(fmt = "Profiles")]
    ProfileList,
    #[display(fmt = "Recipes")]
    RecipeList,
    Request,
    Response,
}

impl PrimaryPane {
    /// Get a trait object that should handle contextual input for this pane
    pub fn input_handler(self) -> Box<dyn InputTarget> {
        match self {
            Self::ProfileList => Box::new(ProfileListPane),
            Self::RecipeList => Box::new(RecipeListPane),
            Self::Request => Box::new(RequestPane),
            Self::Response => Box::new(ResponsePane),
        }
    }
}

impl FixedSelect for PrimaryPane {
    const DEFAULT_INDEX: usize = 1;
}

#[derive(Copy, Clone, Debug, derive_more::Display, EnumIter, PartialEq)]
pub enum RequestTab {
    Body,
    Query,
    Headers,
}

impl FixedSelect for RequestTab {}

#[derive(Copy, Clone, Debug, derive_more::Display, EnumIter, PartialEq)]
pub enum ResponseTab {
    Body,
    Headers,
}

impl FixedSelect for ResponseTab {}