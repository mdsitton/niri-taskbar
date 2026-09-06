//! A pill that sits under the focused window's button and slides between buttons as the focus
//! moves.
//!
//! This is painted directly onto the row of buttons rather than being a widget of its own. That
//! lets it sit at fractional positions between two buttons while it animates without disturbing
//! the layout, and, since no widget is involved, there is nothing sitting over the buttons that
//! could intercept clicks or scrolls.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use waybar_cffi::gtk::{
    self as gtk, CssProvider, StateFlags, StyleContext, cairo,
    glib::{ControlFlow, Value, object::ObjectExt, value::ToValue},
    prelude::{CssProviderExt, StyleContextExt, WidgetExt, WidgetExtManual},
};

/// The default appearance, which a user stylesheet can override through
/// `.niri-taskbar .indicator`.
const DEFAULT_CSS: &[u8] = b"
.indicator {
  background-color: rgba(255, 255, 255, 0.85);
  border-radius: 999px;
  margin: 0 6px;
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

#[derive(Debug, Default)]
struct Animation {
    /// Where the pill was when the target last changed.
    from: Option<Rect>,
    started: Option<Instant>,
}

struct Inner {
    animation: RefCell<Animation>,
    /// The button the pill should sit under, if any.
    target: RefCell<Option<gtk::Button>>,
    ticking: Cell<bool>,
    duration: Duration,
    height: f64,
}

impl Inner {
    /// Where the pill should be drawn right now, in row coordinates.
    fn rect(&self, row: &gtk::Box, context: &StyleContext) -> Option<Rect> {
        // The destination is recomputed every frame rather than cached, so the pill follows
        // buttons that haven't been allocated yet, and tracks the row as it reflows.
        let to = self.target_rect(row, context)?;

        let animation = self.animation.borrow();
        let (Some(from), Some(started)) = (animation.from, animation.started) else {
            return Some(to);
        };
        if self.duration.is_zero() {
            return Some(to);
        }

        let progress = started.elapsed().as_secs_f64() / self.duration.as_secs_f64();
        if progress >= 1.0 {
            return Some(to);
        }

        // Ease out, so the pill arrives gently instead of stopping dead.
        let eased = 1.0 - (1.0 - progress).powi(3);
        Some(from.lerp(to, eased))
    }

    fn target_rect(&self, row: &gtk::Box, context: &StyleContext) -> Option<Rect> {
        let target = self.target.borrow();
        let button = target.as_ref()?;
        if !button.is_visible() {
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
            y: f64::from(baseline) - self.height,
            width: f64::from(width),
            height: self.height,
        })
    }

    fn animating(&self) -> bool {
        match self.animation.borrow().started {
            Some(started) => started.elapsed() < self.duration,
            None => false,
        }
    }
}

/// The focus indicator for a single taskbar page.
pub struct Indicator {
    row: gtk::Box,
    inner: Rc<Inner>,
}

impl Indicator {
    /// Creates an indicator that tracks the buttons within the given row, painting itself over
    /// the top of them.
    pub fn new(row: &gtk::Box, height: u32, duration_ms: u32) -> Self {
        INDICATOR_CSS_PROVIDER.with(|provider| {
            row.style_context()
                .add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        });

        let inner = Rc::new(Inner {
            animation: RefCell::new(Animation::default()),
            target: RefCell::new(None),
            ticking: Cell::new(false),
            duration: Duration::from_millis(u64::from(duration_ms)),
            height: f64::from(height),
        });

        // Connecting after the default handler means we paint once the buttons have been drawn,
        // so the pill lands on top of them. gtk-rs only exposes the before variant of the draw
        // signal, hence going through the generic signal machinery here.
        row.connect_local("draw", true, {
            let inner = Rc::clone(&inner);
            move |values: &[Value]| {
                let row = values.first().and_then(|v| v.get::<gtk::Box>().ok());
                let cr = values.get(1).and_then(|v| v.get::<cairo::Context>().ok());

                if let (Some(row), Some(cr)) = (row, cr) {
                    let context = row.style_context();

                    // Styling comes from the `indicator` class, which is only in play while we
                    // draw, so it never affects the row itself.
                    context.save();
                    context.add_class("indicator");
                    if let Some(rect) = inner.rect(&row, &context) {
                        gtk::render_background(
                            &context,
                            &cr,
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height,
                        );
                        gtk::render_frame(&context, &cr, rect.x, rect.y, rect.width, rect.height);
                    }
                    context.restore();
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

    /// Points the pill at the given button, sliding from wherever it currently is.
    ///
    /// Pass `animate` as false to move it without animating, which is what you want when the
    /// page itself is sliding, or when the pill wasn't visible to begin with.
    pub fn set_target(&self, button: Option<&gtk::Button>, animate: bool) {
        if self.inner.target.borrow().as_ref() == button && !self.inner.animating() {
            // Already settled in the right place, so there's nothing to do beyond the redraws
            // the row will ask for anyway.
            return;
        }

        let context = self.row.style_context();
        context.save();
        context.add_class("indicator");
        let previous = self.inner.rect(&self.row, &context);
        context.restore();

        *self.inner.target.borrow_mut() = button.cloned();

        {
            let mut animation = self.inner.animation.borrow_mut();
            if animate && button.is_some() && previous.is_some() {
                animation.from = previous;
                animation.started = Some(Instant::now());
            } else {
                animation.from = None;
                animation.started = None;
            }
        }

        self.start_tick();
        self.row.queue_draw();
    }

    /// Drives redraws for the length of the animation, then stops so we're not waking up for
    /// every frame the bar draws.
    fn start_tick(&self) {
        if self.inner.ticking.replace(true) {
            return;
        }

        let inner = Rc::clone(&self.inner);
        self.row.add_tick_callback(move |row, _clock| {
            row.queue_draw();

            if inner.animating() {
                ControlFlow::Continue
            } else {
                inner.ticking.set(false);
                ControlFlow::Break
            }
        });
    }
}
