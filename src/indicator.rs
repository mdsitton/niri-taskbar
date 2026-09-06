//! Pills that sit under taskbar buttons: one that follows the focused window, and one that
//! grows under whichever button the pointer is over.
//!
//! These are painted directly onto the row of buttons rather than being widgets of their own.
//! That lets them sit at fractional positions and sizes while they animate without disturbing
//! the layout, and, since no widget is involved, there is nothing over the buttons that could
//! intercept clicks or scrolls.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use waybar_cffi::gtk::{
    self as gtk, CssProvider, StateFlags, StyleContext, cairo,
    glib::{ControlFlow, SignalHandlerId, Value, object::Cast, object::ObjectExt, value::ToValue},
    prelude::{CssProviderExt, StyleContextExt, WidgetExt, WidgetExtManual},
};

/// The default appearance, which a user stylesheet can override through
/// `.niri-taskbar .indicator` and `.niri-taskbar .indicator.hover`.
const DEFAULT_CSS: &[u8] = b"
.indicator {
  background-color: rgba(255, 255, 255, 0.85);
  border-radius: 999px;
  margin: 0 6px;
}

.indicator.hover {
  background-color: rgba(255, 255, 255, 0.45);
}
";

thread_local! {
    static INDICATOR_CSS_PROVIDER: CssProvider = {
        let css = CssProvider::new();
        if let Err(e) = css.load_from_data(DEFAULT_CSS) {
            tracing::error!(%e, "indicator CSS parse error");
        }

        css
    };
}

/// Which pill is being drawn, and so which style class applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Focus,
    Hover,
}

/// How the indicators should behave, from the taskbar configuration.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub focus: bool,
    pub focus_height: u32,
    pub focus_ms: u32,
    pub hover: bool,
    pub hover_height: u32,
    pub hover_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn lerp(self, other: Self, t: f64) -> Self {
        let mix = |a: f64, b: f64| a + (b - a) * t;

        Self {
            x: mix(self.x, other.x),
            y: mix(self.y, other.y),
            width: mix(self.width, other.width),
            height: mix(self.height, other.height),
        }
    }
}

/// Ease out, so movement arrives gently instead of stopping dead.
fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// How far through a transition of `duration` that started at `started` we are, eased.
fn progress(started: Option<Instant>, duration: Duration) -> f64 {
    match started {
        Some(started) if !duration.is_zero() => {
            ease(started.elapsed().as_secs_f64() / duration.as_secs_f64())
        }
        _ => 1.0,
    }
}

#[derive(Debug, Default)]
struct Focus {
    /// The button the pill sits under.
    button: Option<gtk::Button>,
    /// Where the pill was when the target last changed.
    from: Option<Rect>,
    started: Option<Instant>,
}

#[derive(Debug, Default)]
struct Hover {
    /// The button being hovered, kept while the pill retracts after the pointer leaves.
    button: Option<gtk::Button>,
    /// How far out the pill was when the pointer last entered or left.
    from: f64,
    /// Where it is heading: fully out, or fully away.
    to: f64,
    started: Option<Instant>,
}

struct Inner {
    focus: RefCell<Focus>,
    hover: RefCell<Hover>,
    hover_duration: Duration,
    ticking: Cell<bool>,
    options: Options,
}

impl Inner {
    /// Where the focus pill should be drawn right now, in row coordinates.
    fn focus_rect(&self, row: &gtk::Box, context: &StyleContext) -> Option<Rect> {
        if !self.options.focus {
            return None;
        }

        let focus = self.focus.borrow();
        // The destination is recomputed every frame rather than cached, so the pill follows
        // buttons that haven't been allocated yet, and tracks the row as it reflows.
        let to = self.button_rect(
            focus.button.as_ref(),
            row,
            context,
            f64::from(self.options.focus_height),
        )?;

        let Some(from) = focus.from else {
            return Some(to);
        };
        let duration = Duration::from_millis(u64::from(self.options.focus_ms));
        if duration.is_zero() {
            return Some(to);
        }

        Some(from.lerp(to, progress(focus.started, duration)))
    }

    /// Where the hover pill should be drawn right now, in row coordinates.
    ///
    /// It rises up from below the row and opens outwards from the middle of the button, so it
    /// reads as growing out from underneath rather than simply appearing.
    fn hover_rect(&self, row: &gtk::Box, context: &StyleContext) -> Option<Rect> {
        if !self.options.hover {
            return None;
        }

        let hover = self.hover.borrow();
        let extent = self.hover_extent(&hover);
        if extent <= 0.001 {
            return None;
        }

        let rest = self.button_rect(
            hover.button.as_ref(),
            row,
            context,
            f64::from(self.options.hover_height),
        )?;

        let width = rest.width * extent;

        Some(Rect {
            x: rest.x + (rest.width - width) / 2.0,
            // Below its resting place by its own height plus a little, so it is out of sight
            // (and clipped away by the row) before it starts to rise.
            y: rest.y + (1.0 - extent) * (rest.height + 2.0),
            width,
            height: rest.height,
        })
    }

    fn hover_extent(&self, hover: &Hover) -> f64 {
        let t = progress(hover.started, self.hover_duration);
        hover.from + (hover.to - hover.from) * t
    }

    /// The resting rectangle for a pill of the given thickness under the given button.
    fn button_rect(
        &self,
        button: Option<&gtk::Button>,
        row: &gtk::Box,
        context: &StyleContext,
        height: f64,
    ) -> Option<Rect> {
        let button = button?;
        if !button.is_visible() {
            return None;
        }
        // A button that has been pulled out of this row (because its window moved workspace, or
        // closed) has a meaningless allocation as far as we're concerned.
        if button.parent().as_ref() != Some(row.upcast_ref::<gtk::Widget>()) {
            return None;
        }

        let button_alloc = button.allocation();
        let row_alloc = row.allocation();
        let margin = context.margin(StateFlags::NORMAL);
        let (left, right, bottom) = (
            i32::from(margin.left),
            i32::from(margin.right),
            i32::from(margin.bottom),
        );

        let width = button_alloc.width() - left - right;
        if width <= 0 {
            return None;
        }

        let baseline = button_alloc.y() - row_alloc.y() + button_alloc.height() - bottom;

        Some(Rect {
            x: f64::from(button_alloc.x() - row_alloc.x() + left),
            y: f64::from(baseline) - height,
            width: f64::from(width),
            height,
        })
    }

    fn animating(&self) -> bool {
        let focus = self.focus.borrow();
        let focus_duration = Duration::from_millis(u64::from(self.options.focus_ms));
        let focus_running =
            matches!(focus.started, Some(started) if started.elapsed() < focus_duration);

        let hover = self.hover.borrow();
        let hover_running =
            matches!(hover.started, Some(started) if started.elapsed() < self.hover_duration);

        focus_running || hover_running
    }
}

/// The indicators for a single taskbar page.
pub struct Indicator {
    row: gtk::Box,
    inner: Rc<Inner>,
}

impl Indicator {
    /// Creates indicators that track the buttons within the given row, painting over the top of
    /// them.
    pub fn new(row: &gtk::Box, options: Options) -> Self {
        INDICATOR_CSS_PROVIDER.with(|provider| {
            row.style_context()
                .add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        });

        let inner = Rc::new(Inner {
            focus: RefCell::new(Focus::default()),
            hover: RefCell::new(Hover::default()),
            hover_duration: Duration::from_millis(u64::from(options.hover_ms)),
            ticking: Cell::new(false),
            options,
        });

        // Connecting after the default handler means we paint once the buttons have been drawn,
        // so the pills land on top of them. gtk-rs only exposes the before variant of the draw
        // signal, hence going through the generic signal machinery here.
        row.connect_local("draw", true, {
            let inner = Rc::clone(&inner);
            move |values: &[Value]| {
                let row = values.first().and_then(|v| v.get::<gtk::Box>().ok());
                let cr = values.get(1).and_then(|v| v.get::<cairo::Context>().ok());

                if let (Some(row), Some(cr)) = (row, cr) {
                    // The hover pill goes down first, so the focus pill wins where they overlap.
                    draw(&inner, &row, &cr, Kind::Hover);
                    draw(&inner, &row, &cr, Kind::Focus);
                }

                // Carry on propagating, so we don't interfere with anything else drawing.
                Some(false.to_value())
            }
        });

        Self {
            row: row.clone(),
            inner,
        }
    }

    /// Points the focus pill at the given button, sliding from wherever it currently is.
    ///
    /// Pass `animate` as false to move it without animating, which is what you want when the
    /// page itself is sliding, or when the pill wasn't visible to begin with.
    pub fn set_focus(&self, button: Option<&gtk::Button>, animate: bool) {
        if !self.inner.options.focus {
            return;
        }
        if self.inner.focus.borrow().button.as_ref() == button && !self.inner.animating() {
            // Already settled in the right place, so there's nothing to do beyond the redraws
            // the row will ask for anyway.
            return;
        }

        let previous = with_class(&self.row, Kind::Focus, |context| {
            self.inner.focus_rect(&self.row, context)
        });

        {
            let mut focus = self.inner.focus.borrow_mut();
            focus.button = button.cloned();
            if animate && button.is_some() && previous.is_some() {
                focus.from = previous;
                focus.started = Some(Instant::now());
            } else {
                focus.from = None;
                focus.started = None;
            }
        }

        self.wake();
    }

    /// Starts tracking hover on the given button, returning the handler so the caller can stop
    /// tracking if the button moves to another page.
    pub fn watch(&self, button: &gtk::Button) -> SignalHandlerId {
        let inner = Rc::clone(&self.inner);
        let row = self.row.clone();

        button.connect_state_flags_changed(move |button, _previous| {
            if !inner.options.hover {
                return;
            }

            let hovered = button.state_flags().contains(StateFlags::PRELIGHT);
            let mut hover = inner.hover.borrow_mut();
            let is_current = hover.button.as_ref() == Some(button);

            if hovered {
                if is_current && hover.to >= 1.0 {
                    return;
                }
                // Moving straight from one button to another restarts the growth on the new one
                // rather than sliding across, which is what "grows out from underneath" means.
                hover.from = if is_current {
                    inner.hover_extent(&hover)
                } else {
                    0.0
                };
                hover.button = Some(button.clone());
                hover.to = 1.0;
            } else {
                if !is_current || hover.to <= 0.0 {
                    return;
                }
                hover.from = inner.hover_extent(&hover);
                hover.to = 0.0;
            }
            hover.started = Some(Instant::now());
            drop(hover);

            wake(&row, &inner);
        })
    }

    fn wake(&self) {
        wake(&self.row, &self.inner);
    }
}

/// Draws one of the pills, with the style classes that let it be themed from CSS.
fn draw(inner: &Rc<Inner>, row: &gtk::Box, cr: &cairo::Context, kind: Kind) {
    let rect = with_class(row, kind, |context| match kind {
        Kind::Focus => inner.focus_rect(row, context),
        Kind::Hover => inner.hover_rect(row, context),
    });

    let Some(rect) = rect else {
        return;
    };
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

    with_class(row, kind, |context| {
        gtk::render_background(context, cr, rect.x, rect.y, rect.width, rect.height);
        gtk::render_frame(context, cr, rect.x, rect.y, rect.width, rect.height);
    });
}

/// Runs `f` with the indicator style classes temporarily applied to the row, so the pills can be
/// styled from CSS without those classes ever affecting the row itself.
fn with_class<T>(row: &gtk::Box, kind: Kind, f: impl FnOnce(&StyleContext) -> T) -> T {
    let context = row.style_context();

    context.save();
    context.add_class("indicator");
    if kind == Kind::Hover {
        context.add_class("hover");
    }

    let result = f(&context);

    context.restore();

    result
}

/// Drives redraws for the length of the animation, then stops so we're not waking up for every
/// frame the bar draws.
fn wake(row: &gtk::Box, inner: &Rc<Inner>) {
    row.queue_draw();

    if inner.ticking.replace(true) {
        return;
    }

    let inner = Rc::clone(inner);
    row.add_tick_callback(move |row, _clock| {
        row.queue_draw();

        if inner.animating() {
            ControlFlow::Continue
        } else {
            inner.ticking.set(false);
            ControlFlow::Break
        }
    });
}
