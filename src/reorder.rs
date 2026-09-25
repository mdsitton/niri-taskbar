use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use waybar_cffi::gtk::{
    self as gtk, StateFlags,
    gdk::{EventMask, EventType, ModifierType},
    glib::{Propagation, object::Cast},
    prelude::{BoxExt, ContainerExt, StyleContextExt, WidgetExt, WidgetExtManual},
};

use crate::{button::Button, state::State};

/// Where a window's button sits, as far as dragging it around is concerned.
pub struct Placement {
    pub button: gtk::Button,
    pub workspace: u64,
    /// The window's 1-based column in the scrolling layout, or `None` if it's floating, in which
    /// case there's no column to move.
    pub column: Option<usize>,
    /// The window's position in Niri's own order, so the row can be put back if a drag goes
    /// nowhere.
    pub order: usize,
}

/// Lets buttons be dragged left and right along the row to move their window's column in Niri.
///
/// The row is rearranged live while dragging, and Niri is asked to move the column every time it
/// passes another one, so the windows follow along in real time.
#[derive(Clone)]
pub struct Reorder(Rc<Inner>);

struct Inner {
    state: State,
    placements: RefCell<HashMap<u64, Placement>>,
    drag: RefCell<Option<Drag>>,
    /// The window whose button is being dragged, kept apart from `drag` so it can be checked from
    /// signal handlers that might fire while `drag` is borrowed.
    dragging: Cell<Option<u64>>,
}

struct Drag {
    window: u64,
    workspace: u64,
    /// Where the press happened, relative to the button.
    start: (f64, f64),
    /// Whether the pointer has moved far enough for this to count as a drag rather than a click.
    active: bool,
    /// The column index we last asked Niri to move the column to.
    requested: usize,
    /// Whether talking to Niri went wrong, after which we stop trying until the next drag.
    failed: bool,
}

impl Reorder {
    pub fn new(state: State) -> Self {
        Self(Rc::new(Inner {
            state,
            placements: Default::default(),
            drag: Default::default(),
            dragging: Default::default(),
        }))
    }

    /// Makes the given button draggable, if reordering is enabled.
    pub fn attach(&self, button: &Button, window_id: u64) {
        if !self.0.state.config().drag_reorder() {
            return;
        }

        let widget = button.widget();
        widget.add_events(EventMask::BUTTON1_MOTION_MASK);

        // The handlers only hold weak references, since the placements hold the buttons.
        let blocker = button.click_blocker();
        let inner = Rc::downgrade(&self.0);
        widget.connect_button_press_event(move |_, event| {
            // Focusing on press rather than on the click that comes with the release means the
            // window is already active by the time a drag gets going, so the click can go.
            let focused = event.button() == 1
                && event.event_type() == EventType::ButtonPress
                && with(&inner, |inner| inner.press(window_id, event.position()));
            blocker.set(focused);
            Propagation::Proceed
        });

        let inner = Rc::downgrade(&self.0);
        widget.connect_motion_notify_event(move |button, event| {
            if !event.state().contains(ModifierType::BUTTON1_MASK) {
                return Propagation::Proceed;
            }
            inner.upgrade().map_or(Propagation::Proceed, |inner| {
                inner.motion(button, event.position())
            })
        });

        let inner = Rc::downgrade(&self.0);
        widget.connect_button_release_event(move |button, event| {
            if event.button() == 1 {
                with(&inner, |inner| inner.release(button));
            }
            Propagation::Proceed
        });

        // Buttons shuffling about under the pointer would otherwise light up with hover styles (and
        // the hover pill) as they pass by, so keep them all unhovered, the dragged one included.
        let inner = Rc::downgrade(&self.0);
        widget.connect_state_flags_changed(move |button, _| {
            let dragging = with(&inner, |inner| inner.dragging.get());
            if dragging.is_some() && button.state_flags().contains(StateFlags::PRELIGHT) {
                button.unset_state_flags(StateFlags::PRELIGHT);
            }
        });

        let inner = Rc::downgrade(&self.0);
        widget.connect_grab_broken_event(move |button, _| {
            with(&inner, |inner| inner.cancel(button));
            Propagation::Proceed
        });
    }

    /// Replaces the placements with those from the latest snapshot.
    pub fn set_placements(&self, placements: HashMap<u64, Placement>) {
        *self.0.placements.borrow_mut() = placements;
    }

    /// Returns true while a button is being dragged, during which the row order is ours.
    pub fn is_dragging(&self) -> bool {
        self.0.dragging.get().is_some()
    }
}

fn with<T: Default>(inner: &Weak<Inner>, f: impl FnOnce(&Inner) -> T) -> T {
    inner.upgrade().map(|inner| f(&inner)).unwrap_or_default()
}

impl Inner {
    /// Focuses the window and gets ready for a possible drag, returning true if the window was
    /// focused.
    fn press(&self, window: u64, position: (f64, f64)) -> bool {
        // This also gets the window ready to be moved, since Niri can only move the focused
        // column.
        let focused = match self.state.niri().activate_window(window) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(%e, id = window, "error trying to activate window");
                false
            }
        };

        let placements = self.placements.borrow();
        let drag = placements.get(&window).and_then(|placement| {
            let column = placement.column?;
            Some(Drag {
                window,
                workspace: placement.workspace,
                start: position,
                active: false,
                requested: column,
                failed: !focused,
            })
        });

        *self.drag.borrow_mut() = drag;
        focused
    }

    fn motion(&self, button: &gtk::Button, position: (f64, f64)) -> Propagation {
        let mut drag = self.drag.borrow_mut();
        let Some(drag) = drag.as_mut() else {
            return Propagation::Proceed;
        };

        if !drag.active {
            let (start_x, start_y) = drag.start;
            if !button.drag_check_threshold(
                start_x as i32,
                start_y as i32,
                position.0 as i32,
                position.1 as i32,
            ) {
                return Propagation::Proceed;
            }

            drag.active = true;
            self.dragging.set(Some(drag.window));
            button.style_context().add_class("dragging");

            // Whatever was hovered when the drag started doesn't get a state change to trip the
            // handler above, so clear it here.
            if let Some(row) = row_of(button) {
                for child in row.children() {
                    child.unset_state_flags(StateFlags::PRELIGHT);
                }
            }
        }

        let Some(row) = row_of(button) else {
            return Propagation::Stop;
        };

        // Event positions are relative to the button, which moves as we rearrange the row, so
        // work in the same space as the sibling allocations instead.
        let x = f64::from(button.allocation().x()) + position.0;
        let Some(target) = self.arrange(&row, drag, x) else {
            return Propagation::Stop;
        };

        if target != drag.requested && !drag.failed {
            match self.state.niri().move_column_to_index(target) {
                Ok(()) => drag.requested = target,
                Err(e) => {
                    tracing::warn!(%e, id = drag.window, target, "error trying to move column");
                    drag.failed = true;
                }
            }
        }

        Propagation::Stop
    }

    fn release(&self, button: &gtk::Button) {
        let Some(drag) = self.drag.borrow_mut().take() else {
            return;
        };
        if !drag.active {
            return;
        }

        self.dragging.set(None);
        button.style_context().remove_class("dragging");

        // If Niri has already told us about the last move, line the row up with it now. Otherwise
        // the snapshot that's on its way will do that once it arrives.
        let settled = self
            .placements
            .borrow()
            .get(&drag.window)
            .is_some_and(|placement| placement.column == Some(drag.requested));
        if drag.failed || settled {
            if let Some(row) = row_of(button) {
                self.restore(&row);
            }
        }
    }

    fn cancel(&self, button: &gtk::Button) {
        let Some(drag) = self.drag.borrow_mut().take() else {
            return;
        };
        if !drag.active {
            return;
        }

        self.dragging.set(None);
        button.style_context().remove_class("dragging");
        if let Some(row) = row_of(button) {
            self.restore(&row);
        }
    }

    /// Moves the dragged column's buttons to wherever the pointer is among the other columns on
    /// the same workspace, and returns the column index that corresponds to.
    fn arrange(&self, row: &gtk::Box, drag: &Drag, x: f64) -> Option<usize> {
        struct Span {
            column: usize,
            first: usize,
            last: usize,
            left: f64,
            right: f64,
        }

        let placements = self.placements.borrow();
        let children = row.children();

        // Columns get renumbered as Niri moves them, so look up where the dragged window is now
        // rather than where it started: that way it always agrees with the other placements.
        let dragged_column = placements.get(&drag.window)?.column?;

        // Split the row into the dragged column's buttons and everything else, noting where each
        // of the other columns on the workspace sits as we go.
        let mut dragged = Vec::new();
        let mut rest = Vec::new();
        let mut spans: Vec<Span> = Vec::new();
        for child in children.iter() {
            let column = placements
                .values()
                .find(|placement| placement.button.upcast_ref::<gtk::Widget>() == child)
                .filter(|placement| placement.workspace == drag.workspace)
                .and_then(|placement| placement.column);

            match column {
                Some(column) if column == dragged_column => dragged.push(child.clone()),
                Some(column) => {
                    let alloc = child.allocation();
                    let left = f64::from(alloc.x());
                    let right = f64::from(alloc.x() + alloc.width());
                    match spans.last_mut() {
                        Some(span) if span.column == column => {
                            span.last = rest.len();
                            span.right = right;
                        }
                        _ => spans.push(Span {
                            column,
                            first: rest.len(),
                            last: rest.len(),
                            left,
                            right,
                        }),
                    }
                    rest.push(child.clone());
                }
                None => rest.push(child.clone()),
            }
        }

        if dragged.is_empty() || spans.is_empty() {
            return Some(dragged_column);
        }

        let before = spans
            .iter()
            .filter(|span| (span.left + span.right) / 2.0 < x)
            .count();
        let at = match spans.get(before) {
            Some(span) => span.first,
            None => spans[before - 1].last + 1,
        };
        rest.splice(at..at, dragged);

        if rest != children {
            reorder(row, &rest);
        }

        // Columns are numbered contiguously from 1, so landing after `before` of the others puts
        // the dragged column straight after them.
        Some(before + 1)
    }

    /// Puts the row back into Niri's order.
    fn restore(&self, row: &gtk::Box) {
        let placements = self.placements.borrow();
        let mut children = row.children();
        children.sort_by_key(|child| {
            placements
                .values()
                .find(|placement| placement.button.upcast_ref::<gtk::Widget>() == child)
                .map_or(usize::MAX, |placement| placement.order)
        });
        reorder(row, &children);
    }
}

fn row_of(button: &gtk::Button) -> Option<gtk::Box> {
    button.parent()?.downcast().ok()
}

fn reorder(row: &gtk::Box, children: &[gtk::Widget]) {
    for (position, child) in children.iter().enumerate() {
        row.reorder_child(child, position as i32);
    }
}
