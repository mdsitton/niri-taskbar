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

use crate::gradient::{Gradient, Space};
use waybar_cffi::gtk::{
    self as gtk, CssProvider, StateFlags, StyleContext, cairo,
    glib::{ControlFlow, SignalHandlerId, Value, object::Cast, object::ObjectExt, value::ToValue},
    prelude::{ContainerExt, CssProviderExt, StyleContextExt, WidgetExt, WidgetExtManual},
};

/// The default appearance, which a user stylesheet can override through
/// `.niri-taskbar .indicator`, `.niri-taskbar .indicator.focus-hover` and
/// `.niri-taskbar .indicator.hover`.
const DEFAULT_CSS: &[u8] = b"
.indicator {
  background-color: rgba(255, 255, 255, 0.85);
  border-radius: 999px;
  margin: 0 6px;
}

.indicator.focus-hover {
  background-color: rgb(255, 255, 255);
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
    /// The focus pill's look while its button is hovered, which it blends into.
    FocusHover,
    Hover,
    Urgent,
}

impl Kind {
    /// The extra style class this pill carries, alongside `indicator`.
    fn class(self) -> Option<&'static str> {
        match self {
            Self::Focus => None,
            Self::FocusHover => Some("focus-hover"),
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
    pub focus_hover_height: u32,
    /// The colour space the focus pill fades into its hovered colour through.
    pub focus_hover_space: Space,
    /// Drawn in place of the focus pill's usual look while its button is being dragged.
    pub drag_gradient: Option<Gradient>,
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

/// A transition between two looks of the focus pill, from 0 (its usual look) to 1.
#[derive(Debug, Default)]
struct Fade {
    from: f64,
    to: f64,
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
    /// How far the focus pill has changed into its hovered look, which takes the place of the
    /// hover pill on the focused button.
    focus_hover: RefCell<Fade>,
    /// How far the focus pill has changed into the drag gradient.
    drag_fade: RefCell<Fade>,
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
        let mut to = self.button_rect(
            focus.button.as_ref(),
            row,
            context,
            f64::from(self.options.focus_height),
        )?;

        // While hovered, the pill takes on the thickness and margins of its hovered look. A drag
        // counts as hovered too, even though it clears the hover state, so the pill stays lifted.
        let extent = self.focus_hover_extent().max(self.drag_extent());
        if extent > 0.0 {
            let hovered = with_class(row, Kind::FocusHover, |context| {
                self.button_rect(
                    focus.button.as_ref(),
                    row,
                    context,
                    f64::from(self.options.focus_hover_height),
                )
            });
            if let Some(hovered) = hovered {
                to = to.lerp(hovered, extent);
            }
        }

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

    /// Whether the focused button is being dragged with a gradient to show for it.
    fn dragging(&self) -> bool {
        self.options.drag_gradient.is_some()
            && self
                .focus
                .borrow()
                .button
                .as_ref()
                .is_some_and(|button| button.style_context().has_class("dragging"))
    }

    /// The drag gradient's fade in or out, if there's a gradient to fade.
    fn drag_fade_duration(&self) -> Duration {
        let ms = self
            .options
            .drag_gradient
            .map_or(0, |gradient| gradient.fade_ms);
        Duration::from_millis(u64::from(ms))
    }

    /// How far the focus pill is into its drag gradient, from 0 to 1.
    fn drag_extent(&self) -> f64 {
        let fade = self.drag_fade.borrow();
        let t = progress(fade.started, self.drag_fade_duration());
        fade.from + (fade.to - fade.from) * t
    }

    /// Starts the drag gradient fading in or out if a drag has started or stopped, returning
    /// true if it did.
    ///
    /// Nothing tells us when that happens, but the button changes class as it does, which
    /// redraws the row, so this is checked from there.
    fn sync_drag(&self) -> bool {
        let to = if self.dragging() { 1.0 } else { 0.0 };
        if self.drag_fade.borrow().to == to {
            return false;
        }

        let from = self.drag_extent();
        let mut fade = self.drag_fade.borrow_mut();
        fade.from = from;
        fade.to = to;
        fade.started = Some(Instant::now());
        true
    }

    /// How far the focus pill is into its hovered look, from 0 to 1.
    fn focus_hover_extent(&self) -> f64 {
        let focus_hover = self.focus_hover.borrow();
        let t = progress(focus_hover.started, self.hover_duration);
        focus_hover.from + (focus_hover.to - focus_hover.from) * t
    }

    /// Points the focus pill towards or away from its hovered look, depending on whether the
    /// pointer is over the focused button, returning true if anything changed.
    ///
    /// Dragging to reorder clears the hover state, so this switches itself off mid-drag.
    fn update_focus_hover(&self) -> bool {
        if !self.options.focus || !self.options.hover {
            return false;
        }

        let hovered = self
            .focus
            .borrow()
            .button
            .as_ref()
            .is_some_and(|button| button.state_flags().contains(StateFlags::PRELIGHT));
        let to = if hovered { 1.0 } else { 0.0 };
        if self.focus_hover.borrow().to == to {
            return false;
        }

        let from = self.focus_hover_extent();
        let mut focus_hover = self.focus_hover.borrow_mut();
        focus_hover.from = from;
        focus_hover.to = to;
        focus_hover.started = Some(Instant::now());
        true
    }

    /// Whether the hover pill belongs under the given button: it's hovered, and it doesn't already
    /// have the focus pill under it, since the two would just fight over the same spot.
    fn wants_hover(&self, button: &gtk::Button) -> bool {
        button.state_flags().contains(StateFlags::PRELIGHT)
            && !(self.options.focus && self.focus.borrow().button.as_ref() == Some(button))
    }

    /// Grows or retracts the hover pill under the given button, returning true if anything
    /// changed.
    fn update_hover(&self, button: &gtk::Button, hovered: bool) -> bool {
        let mut hover = self.hover.borrow_mut();
        let is_current = hover.button.as_ref() == Some(button);

        if hovered {
            if is_current && hover.to >= 1.0 {
                return false;
            }
            // Moving straight from one button to another restarts the growth on the new one
            // rather than sliding across, which is what "grows out from underneath" means.
            hover.from = if is_current {
                self.hover_extent(&hover)
            } else {
                0.0
            };
            hover.button = Some(button.clone());
            hover.to = 1.0;
        } else {
            if !is_current || hover.to <= 0.0 {
                return false;
            }
            hover.from = self.hover_extent(&hover);
            hover.to = 0.0;
        }
        hover.started = Some(Instant::now());
        true
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

        let focus_hover = self.focus_hover.borrow();
        let focus_hover_running =
            matches!(focus_hover.started, Some(started) if started.elapsed() < self.hover_duration);

        // The urgent pulse has no end of its own: it runs for as long as something is urgent.
        // This is worked out from the row directly rather than from the last paint, because the
        // tick is checked before the frame is drawn and would otherwise stop before the first
        // paint had noticed anything.
        let pulsing = self.options.urgent && self.options.urgent_pulse_ms > 0 && has_urgent(row);

        // The gradient scrolls for as long as it's showing at all, fading out included.
        let gradient_showing = self.drag_fade.borrow().to > 0.0 || self.drag_extent() > 0.0;

        focus_running || hover_running || focus_hover_running || pulsing || gradient_showing
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
            focus_hover: RefCell::new(Fade::default()),
            drag_fade: RefCell::new(Fade::default()),
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
                    if inner.sync_drag() {
                        wake(&row, &inner);
                    }

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

        self.inner.update_focus_hover();

        // Focus arriving at the hovered button takes the hover pill away, and focus leaving a
        // button the pointer is still over brings it back.
        if self.inner.options.hover {
            let hovered = self.inner.hover.borrow().button.clone();
            if let Some(hovered) = hovered {
                self.inner
                    .update_hover(&hovered, self.inner.wants_hover(&hovered));
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

            // Both run regardless of the other, so neither can be skipped.
            let hover_changed = inner.update_hover(button, inner.wants_hover(button));
            let focus_changed = inner.update_focus_hover();
            if hover_changed || focus_changed {
                wake(&row, &inner);
            }
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
        // The hovered look is blended in below, on top of the focus pill.
        Kind::FocusHover => None,
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

    // Fade the focus pill into its hovered colour. Rather than cross-fading the two, which
    // could only ever mix them in sRGB, work out the colour in between in the configured colour
    // space and paint the pill over in that.
    if kind == Kind::Focus {
        let extent = inner.focus_hover_extent();
        if extent > 0.0 {
            let from = with_class(row, Kind::Focus, background_colour);
            let to = with_class(row, Kind::FocusHover, background_colour);
            let [r, g, b, a] = inner.options.focus_hover_space.mix(from, to, extent);

            with_class(row, Kind::Focus, |context| {
                cr.push_group();
                gtk::render_background(context, cr, rect.x, rect.y, rect.width, rect.height);
                cr.set_operator(cairo::Operator::In);
                cr.set_source_rgba(r, g, b, a);
                let _ = cr.paint();
                if cr.pop_group_to_source().is_ok() {
                    let _ = cr.paint();
                }
            });
        }

        if let Some(gradient) = inner.options.drag_gradient {
            let extent = inner.drag_extent();
            if extent > 0.0 {
                draw_gradient(inner, row, cr, rect, gradient, extent);
            }
        }
    }
}

/// The background colour a style context would paint, as sRGB with alpha.
fn background_colour(context: &StyleContext) -> [f64; 4] {
    context
        .style_property_for_state("background-color", StateFlags::NORMAL)
        .get::<gtk::gdk::RGBA>()
        .map(|c| [c.red(), c.green(), c.blue(), c.alpha()])
        .unwrap_or([0.0; 4])
}

/// Draws the focus pill filled with a gradient that scrolls along it, for while its button is
/// being dragged.
fn draw_gradient(
    inner: &Rc<Inner>,
    row: &gtk::Box,
    cr: &cairo::Context,
    rect: Rect,
    gradient: Gradient,
    extent: f64,
) {
    // The gradient runs there and back so it tiles without a seam, and each leg spans the whole
    // pill so there's always a full sweep of colour on show.
    let period = rect.width * 2.0;
    let phase = if gradient.cycle_ms == 0 {
        0.0
    } else {
        let cycle = f64::from(gradient.cycle_ms) / 1000.0;
        (inner.epoch.elapsed().as_secs_f64() / cycle).fract()
    };
    let start = rect.x + phase * period;

    let pattern = cairo::LinearGradient::new(start - period, 0.0, start, 0.0);
    pattern.set_extend(cairo::Extend::Repeat);
    const STEPS: u32 = 16;
    for step in 0..=STEPS {
        let t = f64::from(step) / f64::from(STEPS);
        let [r, g, b, a] = gradient.at(t);
        pattern.add_color_stop_rgba(t / 2.0, r, g, b, a);
        pattern.add_color_stop_rgba(1.0 - t / 2.0, r, g, b, a);
    }

    // Coming and going, the gradient spreads out from the middle of the pill (or shrinks back
    // into it) as a smaller pill of its own, fading as it goes. It starts no narrower than it is
    // tall, so it opens out of a dot rather than a sliver.
    let width = rect.height.min(rect.width) + (rect.width - rect.height).max(0.0) * extent;
    let x = rect.x + (rect.width - width) / 2.0;

    // The stylesheet still decides the pill's shape: draw it as usual, then swap its colour for
    // the gradient wherever it painted. The gradient stays put relative to the full pill, so it
    // doesn't squash as it spreads.
    with_class(row, Kind::Focus, |context| {
        cr.push_group();
        gtk::render_background(context, cr, x, rect.y, width, rect.height);
        cr.set_operator(cairo::Operator::In);
        if cr.set_source(&pattern).is_ok() {
            let _ = cr.paint();
        }
        if cr.pop_group_to_source().is_ok() {
            let _ = cr.paint_with_alpha(extent);
        }
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
