//! Collapsing the windows that share a Niri column down to a single button, which opens a flyout
//! holding the rest while the pointer is over it.
//!
//! The button that shows for a stack is its focused window, or failing that whichever of its
//! windows was focused last, so moving around a column with the keyboard changes which one is on
//! show. The stack's other buttons stay in the row, hidden, so the row keeps its width whatever the
//! stacks are doing: the flyout floats over the bar rather than opening up the row, which would
//! push the rest of the taskbar about (all of it, when the taskbar is centred in the bar).

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use waybar_cffi::gtk::{
    self as gtk, StateFlags, cairo,
    gdk::{self, CrossingMode, NotifyType},
    glib::{self, Propagation, SourceId, object::Cast, prelude::ObjectExt, value::ToValue},
    prelude::{
        BinExt, BoxExt, ButtonExt, ContainerExt, CssProviderExt, GtkWindowExt, StyleContextExt,
        WidgetExt, WidgetExtManual,
    },
};

use std::sync::{Arc, Mutex};

use crate::{
    ScrollState,
    button::Button,
    config::ColumnGrouping,
    indicator::{Indicator, Options as IndicatorOptions},
    niri::Window,
    reorder::Reorder,
    state::State,
};

/// The default look for the cards peeking out from behind a stack, the count on it, and the
/// flyout, which a user stylesheet can override through `.niri-taskbar .stack-card`,
/// `.niri-taskbar .stack-count` and `.stack-flyout`.
const DEFAULT_CSS: &[u8] = b"
.stack-card {
  background-color: rgba(255, 255, 255, 0.12);
  color: rgba(255, 255, 255, 0.28);
}

.stack-count {
  background-color: rgba(255, 255, 255, 0.9);
  color: #1e1e22;
  border-radius: 999px;
  padding: 0 2px;
  font-weight: bold;
}

.stack-flyout {
  background-color: rgba(30, 30, 34, 0.95);
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.25);
  border-radius: 8px;
}
";

thread_local! {
    static STACK_CSS_PROVIDER: gtk::CssProvider = {
        let css = gtk::CssProvider::new();
        if let Err(e) = css.load_from_data(DEFAULT_CSS) {
            tracing::error!(%e, "stack CSS parse error");
        }
        css
    };
}

/// How long the pointer has to rest on a stack before it opens, so sweeping across the bar
/// doesn't set every stack along the way flapping open and shut.
const OPEN_DELAY: Duration = Duration::from_millis(200);

/// How long a stack stays open after the pointer leaves it, so there's time to get from the stack
/// to its flyout.
const CLOSE_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct Grouping(Rc<Inner>);

struct Inner {
    enabled: bool,
    state: State,
    reorder: Reorder,
    scroll_state: Arc<Mutex<ScrollState>>,
    /// Every stack, under each of its windows.
    stacks: RefCell<HashMap<u64, Rc<Stack>>>,
    /// The flyout for the stack that's open, if one is.
    flyout: RefCell<Option<Flyout>>,
    /// The window whose button the pointer is over, in the row or the flyout.
    hovered: Cell<Option<u64>>,
    timer: RefCell<Option<SourceId>>,
    /// When each window was last focused, as a count of focus changes, for picking which window
    /// a stack shows.
    focused_at: RefCell<HashMap<u64, u64>>,
    focus_count: Cell<u64>,
    last_focused: Cell<Option<u64>>,
    /// Gives a stack's button the room its cards need, made on first use from the buttons'
    /// margins as the stylesheet has them.
    room: RefCell<Option<gtk::CssProvider>>,
}

struct Stack {
    /// The window whose button shows in the row.
    shown: u64,
    /// Every window in the stack with its button in the row, the shown one first.
    buttons: Vec<(u64, gtk::Button)>,
    /// Every window in the stack, in the same order, for building flyout buttons from.
    windows: Vec<Window>,
}

impl Stack {
    fn button(&self, id: u64) -> Option<&gtk::Button> {
        self.buttons
            .iter()
            .find(|(window, _)| *window == id)
            .map(|(_, button)| button)
    }

    /// How many cards peek out from behind the stack: one for a pair, two for anything bigger.
    fn cards(&self) -> usize {
        (self.buttons.len() - 1).min(2)
    }

    fn contains(&self, id: u64) -> bool {
        self.buttons.iter().any(|(window, _)| *window == id)
    }
}

/// An open stack's flyout.
struct Flyout {
    /// The shown window of the stack it belongs to.
    shown: u64,
    window: gtk::Window,
    unfold: Rc<Unfold>,
    buttons: Vec<(u64, Button)>,
    /// The same pills as the bar has, under the flyout's buttons.
    indicator: Option<Rc<Indicator>>,
}

impl Flyout {
    /// Points the focus pill at whichever of the flyout's windows is focused, if any.
    fn show_focus(&self, windows: &[Window], animate: bool) {
        let Some(indicator) = &self.indicator else {
            return;
        };
        let focused = windows
            .iter()
            .find(|window| window.is_focused)
            .and_then(|window| self.buttons.iter().find(|(id, _)| *id == window.id))
            .map(|(_, button)| button.widget().clone());
        indicator.set_focus(focused.as_ref(), animate);
        indicator.refresh();
    }
}

impl Grouping {
    pub fn new(state: &State, reorder: Reorder, scroll_state: Arc<Mutex<ScrollState>>) -> Self {
        Self(Rc::new(Inner {
            enabled: state.config().column_grouping() == ColumnGrouping::Collapse,
            state: state.clone(),
            reorder,
            scroll_state,
            stacks: Default::default(),
            flyout: Default::default(),
            hovered: Default::default(),
            timer: Default::default(),
            focused_at: Default::default(),
            focus_count: Default::default(),
            last_focused: Default::default(),
            room: Default::default(),
        }))
    }

    /// Opens and shuts stacks as the pointer comes and goes over the given button.
    pub fn attach(&self, button: &gtk::Button, window_id: u64) {
        if self.0.enabled {
            self.0.track_hover(button.upcast_ref(), window_id);
        }
    }

    /// Draws the cards that mark out the stacks in the row.
    pub fn watch_row(&self, row: &gtk::Box) {
        if !self.0.enabled {
            return;
        }

        STACK_CSS_PROVIDER.with(|provider| {
            row.style_context()
                .add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        });

        // The cards go before the buttons, so the shown button sits on top of them.
        let inner = Rc::downgrade(&self.0);
        row.connect_draw(move |row, cr| {
            if let Some(inner) = inner.upgrade() {
                inner.draw_cards(row, cr);
            }
            Propagation::Proceed
        });

        // And the count after, so it sits on top of the shown button. gtk-rs only exposes the
        // before variant of the draw signal, hence going through the generic signal machinery
        // here.
        let inner = Rc::downgrade(&self.0);
        row.connect_local("draw", true, move |values: &[glib::Value]| {
            let row = values.first().and_then(|v| v.get::<gtk::Box>().ok());
            let cr = values.get(1).and_then(|v| v.get::<cairo::Context>().ok());
            if let (Some(inner), Some(row), Some(cr)) = (inner.upgrade(), row, cr) {
                inner.draw_counts(&row, &cr);
            }
            Some(false.to_value())
        });
    }

    /// Puts the windows in the order their buttons should sit in, with each stack together and
    /// the window it shows first.
    pub fn order<'a>(&self, windows: Vec<&'a Window>) -> Vec<&'a Window> {
        if !self.0.enabled {
            return windows;
        }
        self.0.note_focus(&windows);

        let mut sizes: HashMap<(u64, usize), usize> = HashMap::new();
        for key in windows.iter().filter_map(|window| column(window)) {
            *sizes.entry(key).or_default() += 1;
        }

        let mut ordered = Vec::with_capacity(windows.len());
        let mut done = Vec::new();
        for window in windows.iter() {
            let Some(key) = column(window).filter(|key| sizes[key] > 1) else {
                ordered.push(*window);
                continue;
            };
            if done.contains(&key) {
                continue;
            }
            done.push(key);

            let members: Vec<_> = windows
                .iter()
                .filter(|other| column(other) == Some(key))
                .copied()
                .collect();
            let shown = self.0.pick_shown(&members);
            ordered.extend(members.iter().filter(|w| w.id == shown));
            ordered.extend(members.iter().filter(|w| w.id != shown));
        }
        ordered
    }

    /// Updates the stacks once the buttons are in place, given the windows in the order from
    /// [`Self::order`]. This has to come after anything that shows every button in the row.
    pub fn apply(&self, windows: &[&Window], button: impl Fn(u64) -> Option<gtk::Button>) {
        if !self.0.enabled {
            return;
        }
        let inner = &self.0;

        // The stacks come straight out of the order, since that keeps each one together with the
        // shown window first.
        let mut stacks: HashMap<u64, Rc<Stack>> = HashMap::new();
        let mut i = 0;
        while i < windows.len() {
            let key = column(windows[i]);
            let len = windows[i..]
                .iter()
                .take_while(|window| key.is_some() && column(window) == key)
                .count();
            if len > 1 {
                let members = &windows[i..i + len];
                let stack = Rc::new(Stack {
                    shown: members[0].id,
                    buttons: members
                        .iter()
                        .filter_map(|window| Some((window.id, button(window.id)?)))
                        .collect(),
                    windows: members.iter().map(|window| (*window).clone()).collect(),
                });
                for window in members {
                    stacks.insert(window.id, Rc::clone(&stack));
                }
            }
            i += len.max(1);
        }

        // Windows that have left a stack go back to being ordinary buttons.
        for (id, stack) in inner.stacks.borrow().iter() {
            if stacks.contains_key(id) {
                continue;
            }
            if let Some(button) = stack.button(*id) {
                let context = button.style_context();
                context.remove_class("stacked");
                context.remove_class("stack-collapsed");
                context.remove_class("stack-urgent");
                inner.make_room(button, 0);
                button.show();
            }
        }
        *inner.stacks.borrow_mut() = stacks;

        for stack in inner.unique_stacks() {
            for (id, button) in stack.buttons.iter() {
                let context = button.style_context();
                context.add_class("stacked");
                if *id == stack.shown {
                    context.add_class("stack-collapsed");
                    inner.make_room(button, stack.cards());
                } else {
                    context.remove_class("stack-collapsed");
                    context.remove_class("stack-urgent");
                    inner.make_room(button, 0);
                    button.hide();
                }
            }
        }

        inner.refresh_flyout();
        inner.update_urgency();
    }

    /// The window that scrolling through the taskbar stops at for the given one: the one a stack
    /// is showing, for any window in it, and otherwise the window itself.
    pub fn scroll_stop(&self, window: u64) -> u64 {
        self.0
            .stacks
            .borrow()
            .get(&window)
            .map_or(window, |stack| stack.shown)
    }

    /// Marks each stack as urgent if any of its hidden windows is, since their own urgency can't
    /// be seen. Urgency can change without a snapshot, so this is also called for that.
    pub fn update_urgency(&self) {
        if self.0.enabled {
            self.0.update_urgency();
        }
    }
}

/// The column a tiled window is in, told apart from same-numbered columns on other workspaces.
fn column(window: &Window) -> Option<(u64, usize)> {
    window
        .layout
        .pos_in_scrolling_layout
        .map(|(column, _)| (window.workspace().id, column))
}

impl Inner {
    /// Shrinks a button on its right and bottom by the reach of the given number of cards, so the
    /// cards peeking out from behind it fit inside the space it takes up rather than spilling out
    /// past it, where the bar would cut them off. Zero puts it back as it was.
    ///
    /// What the margins gain, the padding gives up, split either side, so the button asks for no
    /// more room than it did (which would otherwise grow the whole bar, since what's inside the
    /// button can't shrink), and its icon stays the same size, in the middle of the smaller box.
    fn make_room(&self, button: &gtk::Button, cards: usize) {
        let class = |cards: usize| format!("stack-room-{cards}");
        let context = button.style_context();
        let had = (1..=2).find(|n| context.has_class(&class(*n))).unwrap_or(0);
        if had == cards {
            return;
        }

        // Made from a button that hasn't been given any room yet, so it starts from the margins
        // the stylesheet gives, and added just above the stylesheet so it wins.
        if self.room.borrow().is_none() {
            if had != 0 {
                return;
            }
            let margin = context.margin(StateFlags::NORMAL);
            let padding = context.padding(StateFlags::NORMAL);
            let css: String = (1..=2)
                .map(|n| {
                    let reach = (CARD_STEP * n as f64).ceil() as i16;
                    let (before, after) = (reach / 2, reach - reach / 2);
                    format!(
                        "button.{} {{ margin-right: {}px; margin-bottom: {}px; \
                         padding: {}px {}px {}px {}px; }}\n",
                        class(n),
                        margin.right + reach,
                        margin.bottom + reach,
                        (padding.top - before).max(0),
                        (padding.right - after).max(0),
                        (padding.bottom - after).max(0),
                        (padding.left - before).max(0),
                    )
                })
                .collect();
            let provider = gtk::CssProvider::new();
            if let Err(e) = provider.load_from_data(css.as_bytes()) {
                tracing::error!(%e, "stack room CSS parse error");
                return;
            }
            *self.room.borrow_mut() = Some(provider);
        }
        if let Some(provider) = self.room.borrow().as_ref() {
            context.add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
        }

        if had != 0 {
            context.remove_class(&class(had));
        }
        if cards != 0 {
            context.add_class(&class(cards));
        }
    }

    fn unique_stacks(&self) -> Vec<Rc<Stack>> {
        let mut unique: Vec<Rc<Stack>> = Vec::new();
        for stack in self.stacks.borrow().values() {
            if !unique.iter().any(|seen| Rc::ptr_eq(seen, stack)) {
                unique.push(Rc::clone(stack));
            }
        }
        unique
    }

    fn note_focus(&self, windows: &[&Window]) {
        let Some(focused) = windows.iter().find(|window| window.is_focused) else {
            return;
        };
        if self.last_focused.replace(Some(focused.id)) == Some(focused.id) {
            return;
        }
        let count = self.focus_count.get() + 1;
        self.focus_count.set(count);
        self.focused_at.borrow_mut().insert(focused.id, count);
    }

    /// Picks the window a stack shows: the one already showing while its flyout is open, so the
    /// flyout doesn't lose its footing, and otherwise the focused one, or the one focused last.
    fn pick_shown(&self, members: &[&Window]) -> u64 {
        if let Some(open) = self.flyout.borrow().as_ref().map(|flyout| flyout.shown) {
            if members.iter().any(|window| window.id == open) {
                return open;
            }
        }

        let focused_at = self.focused_at.borrow();
        members
            .iter()
            .max_by_key(|window| {
                (
                    window.is_focused,
                    focused_at.get(&window.id).copied().unwrap_or(0),
                    // Otherwise the top of the column, which max_by_key needs to prefer.
                    std::cmp::Reverse(window.layout.pos_in_scrolling_layout),
                )
            })
            .map_or(0, |window| window.id)
    }

    /// Follows the pointer onto and off the given widget, which stands for the given window.
    fn track_hover(self: &Rc<Self>, widget: &gtk::Widget, window: u64) {
        // Crossings caused by grabs, like a menu opening, aren't the pointer coming or going, and
        // nor is the pointer moving between a widget and something inside it.
        let counts = |mode: CrossingMode, detail: NotifyType| {
            mode == CrossingMode::Normal && detail != NotifyType::Inferior
        };

        let inner = Rc::downgrade(self);
        widget.connect_enter_notify_event(move |_, event| {
            if counts(event.mode(), event.detail()) {
                if let Some(inner) = inner.upgrade() {
                    inner.enter(window);
                }
            }
            Propagation::Proceed
        });

        let inner = Rc::downgrade(self);
        widget.connect_leave_notify_event(move |_, event| {
            if counts(event.mode(), event.detail()) {
                if let Some(inner) = inner.upgrade() {
                    inner.leave(window);
                }
            }
            Propagation::Proceed
        });
    }

    /// The stack the open flyout belongs to.
    fn open_stack(&self) -> Option<Rc<Stack>> {
        let shown = self.flyout.borrow().as_ref()?.shown;
        self.stacks.borrow().get(&shown).cloned()
    }

    fn enter(self: &Rc<Self>, window: u64) {
        if self.reorder.is_dragging() {
            return;
        }
        self.hovered.set(Some(window));

        let stack = self.stacks.borrow().get(&window).cloned();
        self.set_scroll_scope(stack.as_deref());
        let open = self.open_stack();
        let in_open = match (&stack, &open) {
            (Some(stack), Some(open)) => Rc::ptr_eq(stack, open),
            _ => false,
        };

        if in_open {
            self.cancel_timer();
        } else if stack.is_some() {
            self.schedule(OPEN_DELAY, |inner| {
                let hovered = inner.hovered.get();
                let stack = hovered.and_then(|id| inner.stacks.borrow().get(&id).cloned());
                if let Some(stack) = stack {
                    inner.open(&stack);
                }
            });
        } else if open.is_some() {
            self.schedule(CLOSE_DELAY, |inner| inner.close());
        }
    }

    fn leave(self: &Rc<Self>, window: u64) {
        if self.hovered.get() == Some(window) {
            self.hovered.set(None);
            self.set_scroll_scope(None);
        }
        if self.flyout.borrow().is_some() {
            self.schedule(CLOSE_DELAY, |inner| {
                // Still on the open stack, in the row or its flyout, means it stays open.
                let open = inner.open_stack();
                let hovered = inner.hovered.get();
                let still_in = match (open, hovered) {
                    (Some(open), Some(hovered)) => open.contains(hovered),
                    _ => false,
                };
                if !still_in {
                    inner.close();
                }
            });
        } else {
            self.cancel_timer();
        }
    }

    /// Keeps scrolling to the given stack's windows, or lets it roam free again.
    ///
    /// It goes through them in the order they're shown: along the flyout if it's open, and
    /// otherwise in the order the flyout would open in, with the window on show first.
    fn set_scroll_scope(&self, stack: Option<&Stack>) {
        let scope = stack.map(|stack| {
            let flyout = self.flyout.borrow();
            match flyout.as_ref() {
                Some(flyout) if flyout.buttons.iter().any(|(id, _)| stack.contains(*id)) => {
                    flyout.buttons.iter().map(|(id, _)| *id).collect()
                }
                _ => stack.windows.iter().map(|window| window.id).collect(),
            }
        });
        self.scroll_state
            .lock()
            .expect("scroll state lock")
            .stack_scope = scope;
    }

    fn schedule(self: &Rc<Self>, delay: Duration, f: impl FnOnce(&Rc<Self>) + 'static) {
        self.cancel_timer();
        let inner: Weak<Self> = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(delay, move || {
            if let Some(inner) = inner.upgrade() {
                // The timer's done, so there's nothing left to cancel.
                inner.timer.borrow_mut().take();
                f(&inner);
            }
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn cancel_timer(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// Opens the flyout for a stack, shutting any other.
    fn open(self: &Rc<Self>, stack: &Rc<Stack>) {
        self.open_in_order(stack, &[]);
    }

    /// Opens the flyout for a stack with its windows in the given order, as far as it goes, and
    /// otherwise in the stack's own, which puts the window it's showing first.
    fn open_in_order(self: &Rc<Self>, stack: &Rc<Stack>, order: &[u64]) {
        if self
            .flyout
            .borrow()
            .as_ref()
            .is_some_and(|flyout| flyout.shown == stack.shown)
        {
            return;
        }
        self.close();

        let Some(shown) = stack.button(stack.shown) else {
            return;
        };
        let Some(toplevel) = shown
            .toplevel()
            .and_then(|toplevel| toplevel.downcast::<gtk::Window>().ok())
        else {
            return;
        };

        // A popup window placed over the stack's button, the way Gtk places tooltips. Unlike a
        // popover, which has to fit inside the bar's own surface along with room for an arrow on
        // every side, this is a surface of its own, so it can sit over the bar at just the
        // button's height, and hang off the end of it if need be.
        let window = gtk::Window::new(gtk::WindowType::Popup);
        window.set_transient_for(Some(&toplevel));
        if let Some(visual) = WidgetExt::screen(&window).and_then(|screen| screen.rgba_visual()) {
            window.set_visual(Some(&visual));
        }
        // Gtk leaves the background to us: the panel is drawn by hand so it can unfold.
        window.set_app_paintable(true);
        let context = window.style_context();
        context.add_class("stack-flyout");
        STACK_CSS_PROVIDER.with(|provider| {
            context.add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        });

        // Tagged like the taskbar, so the buttons pick up the same styling as the taskbar's.
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.style_context().add_class("niri-taskbar");
        window.add(&row);

        // The whole stack, with the shown window first, sized to match the button in the bar so
        // the first of them lands squarely on top of it.
        let (width, height) = (shown.allocated_width(), shown.allocated_height());
        let mut buttons = Vec::new();
        let mut windows: Vec<&Window> = stack.windows.iter().collect();
        windows.sort_by_key(|window| {
            order
                .iter()
                .position(|id| *id == window.id)
                .unwrap_or(usize::MAX)
        });
        for window in windows {
            let button = Button::new(&self.state, window);
            button.set_focus(window.is_focused);
            button.set_niri_urgent(window.is_urgent);
            button.set_title(window.title.as_deref());
            button.widget().set_size_request(width, height);
            self.track_hover(button.widget().upcast_ref(), window.id);

            // Picking a window from the flyout is the end of it: the window's focused, and it
            // becomes the one the stack shows.
            let inner = Rc::downgrade(self);
            button.widget().connect_clicked(move |_| {
                if let Some(inner) = inner.upgrade() {
                    inner.close();
                }
            });

            buttons.push((window.id, button));
        }

        // They fan out from under the first, so that has to be drawn on top of the rest, which
        // means it has to come last: packing from the right in reverse puts them in that order
        // while still laying them out left to right.
        for (_, button) in buttons.iter().rev() {
            row.pack_end(button.widget(), false, false, 0);
        }

        // The same pills as the bar, drawn on the flyout's row, which the buttons are moved about
        // in, so the pills follow them as the stack fans out.
        let options = IndicatorOptions::from_config(self.state.config());
        let indicator = options
            .any()
            .then(|| Rc::new(Indicator::new(&row, options)));
        if let Some(indicator) = &indicator {
            for (_, button) in buttons.iter() {
                indicator.watch(button.widget());
            }
        }

        // Anywhere on the flyout that isn't a button counts as the stack itself.
        self.track_hover(window.upcast_ref(), stack.shown);

        // The flyout's a window of its own, so scrolling over it doesn't reach the bar unless it's
        // passed along.
        if self.state.config().scroll_windows() {
            crate::connect_scroll_handler(&window, self.state.clone(), self.scroll_state.clone());
        }

        // The stack's own button makes room for its cards, which moves its icon a little from where
        // an ordinary button's sits. While the stack is folded up, the flyout's buttons are nudged
        // by the difference, so its icons sit exactly on the stack's as it opens and closes.
        let content = |widget: &gtk::Widget| {
            let context = widget.style_context();
            let (margin, border, padding) = (
                context.margin(StateFlags::NORMAL),
                context.border(StateFlags::NORMAL),
                context.padding(StateFlags::NORMAL),
            );
            (
                f64::from(margin.left + border.left + padding.left),
                f64::from(margin.top + border.top + padding.top),
            )
        };
        let (stack_x, stack_y) = content(shown.upcast_ref());
        let (flyout_x, flyout_y) = buttons.first().map_or((stack_x, stack_y), |(_, button)| {
            content(button.widget().upcast_ref())
        });
        let unfold = Unfold::new(
            &window,
            &row,
            shown,
            (stack_x - flyout_x, stack_y - flyout_y),
        );

        // Its top left corner on the button's, so its first button covers it exactly.
        let (x, y) = shown
            .translate_coordinates(&toplevel, 0, 0)
            .unwrap_or_default();
        row.show_all();
        window.realize();
        if let Some(gdk_window) = window.window() {
            gdk_window.move_to_rect(
                &gdk::Rectangle::new(x, y, width, height),
                gdk::Gravity::West,
                gdk::Gravity::West,
                gdk::AnchorHints::SLIDE_X,
                0,
                0,
            );
        }
        window.show();
        unfold.run(1.0, OPEN, None);
        shown.style_context().remove_class("stack-collapsed");

        // The cards and the count go while it's open, and they're drawn with the row.
        if let Some(row) = row_of(shown) {
            row.queue_draw();
        }

        let flyout = Flyout {
            shown: stack.shown,
            window,
            unfold,
            buttons,
            indicator,
        };
        flyout.show_focus(&stack.windows, false);
        *self.flyout.borrow_mut() = Some(flyout);

        // Scrolling over the stack goes along the flyout from now on.
        let hovered = self.hovered.get();
        if hovered.is_some_and(|id| stack.contains(id)) {
            self.set_scroll_scope(Some(stack));
        }
    }

    /// Shuts the open flyout, if there is one.
    fn close(&self) {
        self.cancel_timer();
        let Some(flyout) = self.flyout.borrow_mut().take() else {
            return;
        };

        // It folds back up before it goes, letting the pointer through to the bar meanwhile, so
        // it doesn't get in the way of whatever comes next.
        flyout
            .window
            .input_shape_combine_region(Some(&cairo::Region::create()));
        let window = flyout.window.clone();
        flyout.unfold.run(
            0.0,
            CLOSE,
            Some(Box::new(move || {
                // SAFETY: nothing else holds on to the flyout once it's closed; it was built just
                // for this stack.
                unsafe { window.destroy() };
            })),
        );

        let stack = self.stacks.borrow().get(&flyout.shown).cloned();
        if let Some(stack) = stack {
            self.reshow(&stack);
        }
    }

    /// Settles which window a stack shows once its flyout has shut. While it was open the stack
    /// kept showing the same one, so nothing moved under the pointer, but focus may well have
    /// moved on since, and there may not be anything else coming from Niri to catch it up.
    fn reshow(&self, stack: &Rc<Stack>) {
        let members: Vec<&Window> = stack.windows.iter().collect();
        let best = self.pick_shown(&members);
        let old = stack.shown;

        let stack = if best != old {
            let (Some(old_button), Some(new_button)) = (stack.button(old), stack.button(best))
            else {
                return;
            };

            // The new one takes the old one's place in the row.
            if let Some(row) = row_of(old_button) {
                let position = row
                    .children()
                    .iter()
                    .position(|child| child == old_button.upcast_ref::<gtk::Widget>());
                if let Some(position) = position {
                    row.reorder_child(new_button, position as i32);
                }
            }
            new_button.show();
            old_button.hide();
            let context = old_button.style_context();
            context.remove_class("stack-collapsed");
            context.remove_class("stack-urgent");
            self.make_room(old_button, 0);
            self.make_room(new_button, stack.cards());

            let first = |id: u64| if id == best { 0 } else { 1 };
            let mut buttons = stack.buttons.clone();
            buttons.sort_by_key(|(id, _)| first(*id));
            let mut windows = stack.windows.clone();
            windows.sort_by_key(|window| first(window.id));
            let reshown = Rc::new(Stack {
                shown: best,
                buttons,
                windows,
            });

            let mut stacks = self.stacks.borrow_mut();
            for (id, _) in reshown.buttons.iter() {
                stacks.insert(*id, Rc::clone(&reshown));
            }

            // Scrolling through the taskbar stops at the stack by the window it shows, too.
            let mut scroll_state = self.scroll_state.lock().expect("scroll state lock");
            for stop in scroll_state.visible_window_ids.iter_mut() {
                if *stop == old {
                    *stop = best;
                }
            }
            reshown
        } else {
            Rc::clone(stack)
        };

        if let Some(shown) = stack.button(stack.shown) {
            shown.style_context().add_class("stack-collapsed");
            // The cards and the count come back now it's shut, and they're drawn with the row.
            if let Some(row) = row_of(shown) {
                row.queue_draw();
            }
        }
        self.update_urgency();
    }

    /// Brings the open flyout up to date with its stack, or shuts it if the stack's gone.
    fn refresh_flyout(self: &Rc<Self>) {
        if self.flyout.borrow().is_none() {
            return;
        }
        let Some(stack) = self.open_stack() else {
            self.close();
            return;
        };

        // The flyout keeps the order it opened in for as long as it's open, whatever the stack
        // does meanwhile, so only windows coming or going mean building it again.
        let order: Vec<u64> = self
            .flyout
            .borrow()
            .as_ref()
            .map(|flyout| flyout.buttons.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        let same = order.len() == stack.windows.len()
            && stack
                .windows
                .iter()
                .all(|window| order.contains(&window.id));
        if !same {
            // Those still there keep their places, and any newcomers go on the end.
            self.close();
            let stack = self.stacks.borrow().get(&stack.shown).cloned();
            if let Some(stack) = stack {
                self.open_in_order(&stack, &order);
            }
            return;
        }

        if let Some(flyout) = self.flyout.borrow().as_ref() {
            for (id, button) in flyout.buttons.iter() {
                if let Some(window) = stack.windows.iter().find(|window| window.id == *id) {
                    button.set_focus(window.is_focused);
                    button.set_niri_urgent(window.is_urgent);
                    button.set_title(window.title.as_deref());
                }
            }
            flyout.show_focus(&stack.windows, true);
        }
        if let Some(shown) = stack.button(stack.shown) {
            shown.style_context().remove_class("stack-collapsed");
        }
    }

    fn update_urgency(&self) {
        // The flyout's buttons are copies of the bar's, so a window marked urgent by a
        // notification, which only reaches the bar's button, is passed along to them.
        if let Some(flyout) = self.flyout.borrow().as_ref() {
            if let Some(stack) = self.stacks.borrow().get(&flyout.shown) {
                for (id, button) in flyout.buttons.iter() {
                    let urgent = stack
                        .button(*id)
                        .is_some_and(|row| row.style_context().has_class("urgent"));
                    if urgent && !button.widget().style_context().has_class("urgent") {
                        button.set_urgent();
                    }
                }
            }
            if let Some(indicator) = &flyout.indicator {
                indicator.refresh();
            }
        }

        for stack in self.unique_stacks() {
            let Some(shown) = stack.button(stack.shown) else {
                continue;
            };
            let urgent = stack.buttons.iter().any(|(id, button)| {
                *id != stack.shown && button.style_context().has_class("urgent")
            });
            if urgent {
                shown.style_context().add_class("stack-urgent");
            } else {
                shown.style_context().remove_class("stack-urgent");
            }
        }
    }

    /// The shut stacks in the row, with each one's shown button and where it appears.
    fn shut_in_row(&self, row: &gtk::Box) -> Vec<(Rc<Stack>, Rect)> {
        let open = self.flyout.borrow().as_ref().map(|flyout| flyout.shown);
        let row_alloc = row.allocation();
        self.unique_stacks()
            .into_iter()
            .filter(|stack| open != Some(stack.shown))
            .filter_map(|stack| {
                let shown = stack.button(stack.shown)?;
                if !shown.is_visible() || row_of(shown).as_ref() != Some(row) {
                    return None;
                }
                let rect = content_rect(shown, &row_alloc)?;
                Some((stack, rect))
            })
            .collect()
    }

    /// Draws a card or two peeking out from behind the shown button of each shut stack, down and
    /// to the right like the rest of a deck.
    ///
    /// They're drawn by hand rather than as CSS boxes so they can take the button's own corners,
    /// whatever the stylesheet gives it, and so can the gap they leave for it: they're cut away
    /// wherever the button is, since buttons are often see-through, and a card showing through
    /// one would just look like a lighter button.
    fn draw_cards(&self, row: &gtk::Box, cr: &cairo::Context) {
        let context = row.style_context();
        context.save();
        context.add_class("stack-card");
        // The edge takes the text colour rather than a border's, since stylesheets commonly turn
        // borders off everywhere with a `*` rule, and that would take this one along with them.
        let fill = colour(&context, "background-color");
        let edge = colour(&context, "color");
        context.restore();

        // A focused stack's cards take the focus pill's colour, so the two read as one highlight
        // rather than a pill with a stray line under it.
        context.save();
        context.add_class("indicator");
        let pill = colour(&context, "background-color");
        context.restore();
        let focused_fill = [pill[0], pill[1], pill[2], pill[3] * 0.15];
        let focused_edge = [pill[0], pill[1], pill[2], pill[3] * 0.8];

        for (stack, (x, y, width, height)) in self.shut_in_row(row) {
            let Some(shown) = stack.button(stack.shown) else {
                continue;
            };
            let radius = corner_radius(shown).min(width / 2.0).min(height / 2.0);
            let (fill, edge) = if shown.style_context().has_class("focused") {
                (focused_fill, focused_edge)
            } else {
                (fill, edge)
            };

            // Furthest first, so the nearer one sits over it.
            let cards = stack.cards();
            let reach = CARD_STEP * cards as f64;

            let _ = cr.save();
            cr.set_fill_rule(cairo::FillRule::EvenOdd);
            cr.rectangle(x, y, width + reach + 1.0, height + reach + 1.0);
            rounded_rect(cr, x, y, width, height, radius);
            cr.clip();

            for card in (1..=cards).rev() {
                let offset = CARD_STEP * card as f64;
                rounded_rect(cr, x + offset, y + offset, width, height, radius);
                cr.set_source_rgba(fill[0], fill[1], fill[2], fill[3]);
                let _ = cr.fill_preserve();
                // Half a pixel in, so the one-pixel edge lands on whole pixels.
                cr.new_path();
                rounded_rect(
                    cr,
                    x + offset + 0.5,
                    y + offset + 0.5,
                    width - 1.0,
                    height - 1.0,
                    (radius - 0.5).max(0.0),
                );
                cr.set_source_rgba(edge[0], edge[1], edge[2], edge[3]);
                cr.set_line_width(1.0);
                let _ = cr.stroke();
            }
            let _ = cr.restore();
        }
    }

    /// Draws how many windows are in each shut stack, in its button's top right corner.
    fn draw_counts(&self, row: &gtk::Box, cr: &cairo::Context) {
        let context = row.style_context();
        for (stack, (x, y, width, height)) in self.shut_in_row(row) {
            context.save();
            context.add_class("stack-count");

            // Sized to the button, since the bar's own font is far too big for a badge, taking
            // the family and weight from the stylesheet.
            let layout = row.create_pango_layout(Some(&stack.buttons.len().to_string()));
            let mut font = context
                .style_property_for_state("font", StateFlags::NORMAL)
                .get::<gtk::pango::FontDescription>()
                .unwrap_or_default();
            let size = (height * 0.25).clamp(8.0, 13.0);
            font.set_absolute_size(size * f64::from(gtk::pango::SCALE));
            layout.set_font_description(Some(&font));
            let (_, text) = layout.pixel_extents();
            let padding = context.padding(StateFlags::NORMAL);
            let badge_height = f64::from(text.height() + i32::from(padding.top + padding.bottom));
            let badge_width =
                f64::from(text.width() + i32::from(padding.left + padding.right)).max(badge_height);

            // Tucked into the top corner, out of the way of the pills along the bottom, and
            // overlapping the button's edge a little.
            let bx = x + width - badge_width + 1.0;
            let by = y - 1.0;
            gtk::render_background(&context, cr, bx, by, badge_width, badge_height);
            gtk::render_frame(&context, cr, bx, by, badge_width, badge_height);
            gtk::render_layout(
                &context,
                cr,
                bx + (badge_width - f64::from(text.width())) / 2.0 - f64::from(text.x()),
                by + (badge_height - f64::from(text.height())) / 2.0 - f64::from(text.y()),
                &layout,
            );
            context.restore();
        }
    }
}

/// How long the flyout takes to unfold, and to fold back up.
const OPEN: Duration = Duration::from_millis(200);
const CLOSE: Duration = Duration::from_millis(150);

/// The flyout unfolding from the stack's button: it fades in over the button, then the rest of the
/// stack fans out from under it to the right, with the panel stretching to follow. Folding up runs
/// the same in reverse.
///
/// The panel's drawn by hand, at whatever width it's got to, and the buttons are moved by hand, so
/// the window itself can stay the one size throughout.
struct Unfold {
    from: Cell<f64>,
    to: Cell<f64>,
    started: Cell<Instant>,
    duration: Cell<Duration>,
    ticking: Cell<bool>,
    done: RefCell<Option<Box<dyn FnOnce()>>>,
    window: glib::WeakRef<gtk::Window>,
    /// Where each button belongs once it's fanned out, as the row lays them out.
    slots: RefCell<Vec<(gtk::Widget, gtk::Allocation)>>,
    /// How far the buttons are moved while folded up, so their icons line up with the stack's.
    nudge: (f64, f64),
}

impl Unfold {
    fn new(
        window: &gtk::Window,
        row: &gtk::Box,
        shown: &gtk::Button,
        nudge: (f64, f64),
    ) -> Rc<Self> {
        let unfold = Rc::new(Self {
            from: Cell::new(0.0),
            to: Cell::new(0.0),
            started: Cell::new(Instant::now()),
            duration: Cell::new(OPEN),
            ticking: Cell::new(false),
            done: Default::default(),
            window: window.downgrade(),
            slots: Default::default(),
            nudge,
        });

        // Once the row's put its buttons where they belong, gather them up to wherever the fan
        // has got to.
        row.connect_size_allocate({
            let unfold = Rc::downgrade(&unfold);
            move |row, _| {
                if let Some(unfold) = unfold.upgrade() {
                    *unfold.slots.borrow_mut() = row
                        .children()
                        .into_iter()
                        .map(|child| {
                            let alloc = child.allocation();
                            (child, alloc)
                        })
                        .collect();
                    unfold.place();
                }
            }
        });

        // What's folded away is the width beyond the first button, which is the stack's own.
        let folded = f64::from(shown.allocated_width());

        // Before the buttons are drawn: the panel, faded. The buttons are faded by their own
        // opacity as the animation goes.
        window.connect_draw({
            let unfold = Rc::downgrade(&unfold);
            move |window, cr| {
                let Some(unfold) = unfold.upgrade() else {
                    return Propagation::Proceed;
                };
                let (fan, alpha) = unfold.shape();
                let unfolded = f64::from(window.allocated_width());
                let width = folded + (unfolded - folded).max(0.0) * fan;
                let height = f64::from(window.allocated_height());

                // Around the buttons, in the gap their margins leave, so it frames them.
                if width > 0.0 && height > 0.0 {
                    let context = window.style_context();
                    cr.push_group();
                    gtk::render_background(&context, cr, 0.0, 0.0, width, height);
                    gtk::render_frame(&context, cr, 0.0, 0.0, width, height);
                    if cr.pop_group_to_source().is_ok() {
                        let _ = cr.paint_with_alpha(alpha);
                    }
                }
                if let Some(child) = window.child() {
                    child.set_opacity(alpha);
                }
                Propagation::Proceed
            }
        });

        unfold
    }

    /// How far along it is, from 0 (folded away) to 1 (unfolded).
    fn progress(&self) -> f64 {
        let duration = self.duration.get();
        let t = if duration.is_zero() {
            1.0
        } else {
            (self.started.get().elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0)
        };
        let (from, to) = (self.from.get(), self.to.get());
        from + (to - from) * t
    }

    /// The panel's width and the flyout's opacity for where the animation's got to. The fade
    /// takes the first third, with the panel only starting to slide out once it's mostly there.
    fn shape(&self) -> (f64, f64) {
        let p = self.progress();
        let alpha = (p * 3.0).min(1.0);
        let fan = ease(((p - 0.2) / 0.8).clamp(0.0, 1.0));
        (fan, alpha)
    }

    /// Puts each button partway between the first one and where it belongs, by how far the fan
    /// has got, so they slide out from under it and back, all of them nudged onto the stack's
    /// icon while folded and easing off it as they fan out.
    fn place(&self) {
        let (fan, _) = self.shape();
        let (nudge_x, nudge_y) = (self.nudge.0 * (1.0 - fan), self.nudge.1 * (1.0 - fan));
        let slots = self.slots.borrow();
        let Some(start) = slots.iter().map(|(_, slot)| slot.x()).min() else {
            return;
        };
        for (child, slot) in slots.iter() {
            let x = f64::from(start) + f64::from(slot.x() - start) * fan + nudge_x;
            let y = f64::from(slot.y()) + nudge_y;
            let allocation = gtk::Allocation::new(
                x.round() as i32,
                y.round() as i32,
                slot.width(),
                slot.height(),
            );
            if child.allocation() != allocation {
                // Gtk complains about allocating a widget that hasn't been measured since it last
                // asked to be, so make sure it has been.
                let _ = child.preferred_size();
                child.size_allocate(&allocation);
            }
        }
    }

    /// Heads for `to`, from wherever it's got to, calling `done` once it's there.
    fn run(self: &Rc<Self>, to: f64, duration: Duration, done: Option<Box<dyn FnOnce()>>) {
        self.from.set(self.progress());
        self.to.set(to);
        self.duration.set(duration);
        self.started.set(Instant::now());
        *self.done.borrow_mut() = done;

        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.queue_draw();
        if self.ticking.replace(true) {
            return;
        }

        let unfold = Rc::clone(self);
        window.add_tick_callback(move |window, _| {
            unfold.place();
            window.queue_draw();
            if unfold.started.get().elapsed() < unfold.duration.get() {
                return glib::ControlFlow::Continue;
            }
            unfold.ticking.set(false);
            if let Some(done) = unfold.done.borrow_mut().take() {
                done();
            }
            glib::ControlFlow::Break
        });
    }
}

/// Ease out, so movement arrives gently instead of stopping dead.
fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// How far each card behind a stack sits from the one in front of it, both across and down.
const CARD_STEP: f64 = 2.0;

/// A colour from a style context, as sRGB with alpha.
fn colour(context: &gtk::StyleContext, property: &str) -> [f64; 4] {
    context
        .style_property_for_state(property, StateFlags::NORMAL)
        .get::<gdk::RGBA>()
        .map(|c| [c.red(), c.green(), c.blue(), c.alpha()])
        .unwrap_or([1.0, 1.0, 1.0, 0.3])
}

/// How rounded a button's corners are.
///
/// Gtk 3 only hands out the corners' radius through the old `border-radius` property, which gives
/// the top left one.
fn corner_radius(button: &gtk::Button) -> f64 {
    button
        .style_context()
        .style_property_for_state("border-radius", StateFlags::NORMAL)
        .get::<i32>()
        .map_or(0.0, f64::from)
}

/// Traces a rectangle with rounded corners.
fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        FRAC_PI_2,
    );
    cr.arc(x + radius, y + height - radius, radius, FRAC_PI_2, PI);
    cr.arc(x + radius, y + radius, radius, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// A rectangle, as `(x, y, width, height)`.
type Rect = (f64, f64, f64, f64);

/// A button's rectangle in row coordinates, less its margins.
fn content_rect(button: &gtk::Button, row: &gtk::Allocation) -> Option<Rect> {
    let margin = button.style_context().margin(StateFlags::NORMAL);
    let alloc = button.allocation();
    let width = alloc.width() - i32::from(margin.left) - i32::from(margin.right);
    let height = alloc.height() - i32::from(margin.top) - i32::from(margin.bottom);
    (width > 0 && height > 0).then(|| {
        (
            f64::from(alloc.x() - row.x() + i32::from(margin.left)),
            f64::from(alloc.y() - row.y() + i32::from(margin.top)),
            f64::from(width),
            f64::from(height),
        )
    })
}

fn row_of(button: &gtk::Button) -> Option<gtk::Box> {
    button.parent()?.downcast().ok()
}
