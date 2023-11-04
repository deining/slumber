//! State types for the view.

use crate::http::{RequestBuildError, RequestError, RequestId, RequestRecord};
use chrono::{DateTime, Duration, Utc};
use itertools::Itertools;
use ratatui::widgets::*;
use std::{
    cell::{Ref, RefCell},
    fmt::Display,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};
use strum::IntoEnumIterator;

/// An internally mutable cell for UI state. Certain state needs to be updated
/// during the draw phase, typically because it's derived from parent data
/// passed via props. This is safe to use in the render phase, because rendering
/// is entirely synchronous.
///
/// In addition to storing the state value, this stores a state key as well. The
/// key is used to determine when to update the state. The key should be
/// something cheaply comparable. If the value itself is cheaply comparable,
/// you can just use that as the key.
#[derive(Debug)]
pub struct StateCell<K, V> {
    state: RefCell<Option<(K, V)>>,
}

impl<K, V> StateCell<K, V> {
    /// Get the current state value, or a new value if the state is stale. State
    /// will be stale if it is uninitialized OR the key has changed. In either
    /// case, `init` will be called to create a new value.
    pub fn get_or_update(&self, key: K, init: impl FnOnce() -> V) -> Ref<'_, V>
    where
        K: PartialEq,
    {
        let mut state = self.state.borrow_mut();
        match state.deref() {
            Some(state) if state.0 == key => {}
            _ => {
                // (Re)create the state
                *state = Some((key, init()));
            }
        }
        drop(state);

        // Unwrap is safe because we just stored a value
        // It'd be nice to return an `impl Deref` here instead to prevent
        // leaking implementation details, but I was struggling with the
        // lifetimes on that
        Ref::map(self.state.borrow(), |state| &state.as_ref().unwrap().1)
    }

    /// Get a mutable reference to the V. This will never panic because
    /// `&mut self` guarantees exclusive access. Returns `None` iff the state
    /// cell is uninitialized.
    pub fn get_mut(&mut self) -> Option<&mut V> {
        self.state.get_mut().as_mut().map(|state| &mut state.1)
    }
}

/// Derive impl applies unnecessary bound on the generic parameter
impl<K, V> Default for StateCell<K, V> {
    fn default() -> Self {
        Self {
            state: RefCell::new(None),
        }
    }
}

/// State of an HTTP response, which can be in various states of
/// completion/failure. Each request *recipe* should have one request state
/// stored in the view at a time.
#[derive(Debug)]
pub enum RequestState {
    /// The request is being built. Typically this is very fast, but can be
    /// slow if a chain source takes a while.
    Building { id: RequestId },

    /// Something went wrong during the build :(
    BuildError { error: RequestBuildError },

    /// Request is in flight, or is *about* to be sent. There's no way to
    /// initiate a request that doesn't immediately launch it, so Loading is
    /// the initial state.
    Loading {
        id: RequestId,
        start_time: DateTime<Utc>,
    },

    /// A resolved HTTP response, with all content loaded and ready to be
    /// displayed. This does *not necessarily* have a 2xx/3xx status code, any
    /// received response is considered a "success".
    Response {
        record: RequestRecord,
        pretty_body: Option<String>,
    },

    /// Error occurred sending the request or receiving the response.
    RequestError { error: RequestError },
}

impl RequestState {
    /// Unique ID for this request, which will be retained throughout its life
    /// cycle
    pub fn id(&self) -> RequestId {
        match self {
            Self::Building { id } | Self::Loading { id, .. } => *id,
            Self::BuildError { error } => error.id,
            Self::RequestError { error } => error.request.id,
            Self::Response { record, .. } => record.id,
        }
    }

    /// Is the initial stage in a request life cycle?
    pub fn is_initial(&self) -> bool {
        matches!(self, Self::Building { .. })
    }

    /// When was the request launched? Returns `None` if the request hasn't
    /// been launched yet.
    pub fn start_time(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Building { .. } | Self::BuildError { .. } => None,
            Self::Loading { start_time, .. } => Some(*start_time),
            Self::Response { record, .. } => Some(record.start_time),
            Self::RequestError { error } => Some(error.start_time),
        }
    }

    /// Elapsed time for the active request. If pending, this is a running
    /// total. Otherwise end time - start time.  Returns `None` if the request
    /// hasn't been launched yet.
    pub fn duration(&self) -> Option<Duration> {
        match self {
            Self::Building { .. } | Self::BuildError { .. } => None,
            Self::Loading { start_time, .. } => Some(Utc::now() - start_time),
            Self::Response { record, .. } => Some(record.duration()),
            Self::RequestError { error } => {
                Some(error.end_time - error.start_time)
            }
        }
    }

    /// Initialize a new request in the `Building` state
    pub fn building(id: RequestId) -> Self {
        Self::Building { id }
    }

    /// Create a loading state with the current timestamp. This will generally
    /// be slightly off from when the request was actually launched, but it
    /// shouldn't matter. See [HttpEngine::send] for why it can't report a start
    /// time back to us.
    pub fn loading(id: RequestId) -> Self {
        Self::Loading {
            id,
            start_time: Utc::now(),
        }
    }

    /// Create a request state from a completed response. This is **expensive**,
    /// don't call it unless you need the value.
    pub fn response(record: RequestRecord) -> Self {
        // Prettification might get slow on large responses, maybe we
        // want to punt this into a separate task?
        let pretty_body = record.response.prettify_body().ok();
        Self::Response {
            record,
            pretty_body,
        }
    }
}

/// A notification is an ephemeral informational message generated by some async
/// action. It doesn't grab focus, but will be useful to the user nonetheless.
/// It should be shown for a short period of time, then disappear on its own.
#[derive(Debug)]
pub struct Notification {
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    pub fn new(message: String) -> Self {
        Self {
            message,
            timestamp: Utc::now(),
        }
    }
}

/// A list of items in the UI
#[derive(Debug)]
pub struct StatefulList<T> {
    /// Use interior mutability because this needs to be modified during the
    /// draw phase, by [Frame::render_stateful_widget]. This allows rendering
    /// without a mutable reference.
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

    /// Get the number of items in the list
    pub fn len(&self) -> usize {
        self.items.len()
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
    selected_index: usize,
    _phantom: PhantomData<T>,
}

/// Friendly marker trait indicating a type can be cycled through, e.g. a set
/// of panes or tabs
pub trait FixedSelect:
    Default + Display + IntoEnumIterator + PartialEq
{
}

impl<T: FixedSelect> StatefulSelect<T> {
    pub fn new() -> Self {
        Self {
            // Find the index of the select type's default value
            selected_index: T::iter()
                .find_position(|value| value == &T::default())
                .unwrap()
                .0,
            _phantom: PhantomData,
        }
    }

    /// Get the index of the selected element
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Get the selected element
    pub fn selected(&self) -> T {
        T::iter()
            .nth(self.selected_index)
            .expect("StatefulSelect index out of bounds")
    }

    /// Is the given item selected?
    pub fn is_selected(&self, item: &T) -> bool {
        &self.selected() == item
    }

    /// Select previous item, returning whether the selection changed
    pub fn previous(&mut self) -> bool {
        // Prevent underflow
        let old = self.selected_index;
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(T::iter().count() - 1);
        old != self.selected_index
    }

    /// Select next item, returning whether the selection changed
    pub fn next(&mut self) -> bool {
        let old = self.selected_index;
        self.selected_index = (self.selected_index + 1) % T::iter().count();
        old != self.selected_index
    }
}

impl<T: FixedSelect> Default for StatefulSelect<T> {
    fn default() -> Self {
        Self::new()
    }
}
