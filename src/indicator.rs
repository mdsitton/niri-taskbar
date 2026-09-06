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
    prelude::{ContainerExt, CssProviderExt, StyleContextExt, WidgetExt, WidgetExtManual},
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

.indicator.urgent {
  background-color: rgb(235, 77, 75);
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
    Urgent,
}

impl Kind {
    /// The extra style class this pill carries, alongside `indicator`.
    fn class(self) -> Option<&'static str> {
        match self {
            Self::Focus => None,
            Self::Hover => Some("hover"),
            Self::Urgent => Some("urgent"),
        }
    }
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
    pub urgent: bool,
    pub urgent_height: u32,
    pub urgent_pulse_ms: u32,
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
    /// Fixed point the pulse is measured from, so every urgent pill throbs in step.
    epoch: Instant,
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

    fn animating(&self, row: &gtk::Box) -> bool {
        let focus = self.focus.borrow();
        let focus_duration = Duration::from_millis(u64::from(self.options.focus_ms));
        let focus_running =
            matches!(focus.started, Some(started) if started.elapsed() < focus_duration);

        let hover = self.hover.borrow();
        let hover_running =
            matches!(hover.started, Some(started) if started.elapsed() < self.hover_duration);

        // The urgent pulse has no end of its own: it runs for as long as something is urgent.
        // This is worked out from the row directly rather than from the last paint, because the
        // tick is checked before the frame is drawn and would otherwise stop before the first
        // paint had noticed anything.
        let pulsing = self.options.urgent && self.options.urgent_pulse_ms > 0 && has_urgent(row);

        focus_running || hover_running || pulsing
    }

    /// How opaque the urgent pill should be right now, easing between dim and full so it
    /// breathes rather than blinking.
    fn urgent_alpha(&self) -> f64 {
        if self.options.urgent_pulse_ms == 0 {
            return 1.0;
        }

        let period = f64::from(self.options.urgent_pulse_ms) / 1000.0;
        let phase = (self.epoch.elapsed().as_secs_f64() / period) * std::f64::consts::TAU;

        0.35 + 0.65 * (0.5 + 0.5 * phase.cos())
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
            epoch: Instant::now(),
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
                    // Painted bottom to top, so the focus pill wins wherever they overlap.
                    draw_urgent(&inner, &row, &cr);
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
        if self.inner.focus.borrow().button.as_ref() == button && !self.inner.animating(&self.row) {
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

    /// Kicks the animation along after something that isn't focus or hover changed, such as a
    /// window becoming urgent.
    pub fn refresh(&self) {
        self.wake();
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
        // Urgent is plural, so it goes through draw_urgent instead.
        Kind::Urgent => None,
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

/// Draws a pill under every urgent button, since any number of windows can be asking for
/// attention at once.
fn draw_urgent(inner: &Rc<Inner>, row: &gtk::Box, cr: &cairo::Context) {
    if !inner.options.urgent {
        return;
    }

    let height = f64::from(inner.options.urgent_height);
    let alpha = inner.urgent_alpha();

    for child in row.children() {
        let Ok(button) = child.downcast::<gtk::Button>() else {
            continue;
        };
        if !button.style_context().has_class("urgent") {
            continue;
        }

        let rect = with_class(row, Kind::Urgent, |context| {
            inner.button_rect(Some(&button), row, context, height)
        });
        let Some(rect) = rect else {
            continue;
        };

        with_class(row, Kind::Urgent, |context| {
            // Drawn into a group so the pulse can fade the whole pill, including whatever
            // border the stylesheet gave it, as one.
            cr.push_group();
            gtk::render_background(context, cr, rect.x, rect.y, rect.width, rect.height);
            gtk::render_frame(context, cr, rect.x, rect.y, rect.width, rect.height);
            if cr.pop_group_to_source().is_ok() {
                let _ = cr.paint_with_alpha(alpha);
            }
        });
    }
}

/// Whether any button in the row is currently asking for attention.
fn has_urgent(row: &gtk::Box) -> bool {
    row.children().into_iter().any(|child| {
        child
            .downcast::<gtk::Button>()
            .is_ok_and(|button| button.style_context().has_class("urgent"))
    })
}

/// Runs `f` with the indicator style classes temporarily applied to the row, so the pills can be
/// styled from CSS without those classes ever affecting the row itself.
fn with_class<T>(row: &gtk::Box, kind: Kind, f: impl FnOnce(&StyleContext) -> T) -> T {
    let context = row.style_context();

    context.save();
    context.add_class("indicator");
    if let Some(class) = kind.class() {
        context.add_class(class);
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

        if inner.animating(row) {
            ControlFlow::Continue
        } else {
            inner.ticking.set(false);
            ControlFlow::Break
        }
    });
}
