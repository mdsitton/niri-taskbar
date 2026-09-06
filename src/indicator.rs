//! A pill that sits under the focused window's button and slides between buttons as the focus
//! moves.
//!
//! This is drawn rather than packed into the taskbar, so it can sit at fractional positions
//! between two buttons while it animates, and so it never disturbs the layout of the row.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use waybar_cffi::gtk::{
    self as gtk, CssProvider, StateFlags, StyleContext,
    glib::{ControlFlow, Propagation},
    prelude::{CssProviderExt, StyleContextExt, WidgetExt, WidgetExtManual},
};

/// The default appearance, which a user stylesheet can override through
/// `.niri-taskbar .indicator`.
const DEFAULT_CSS: &[u8] = b"
* {
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
    area: gtk::DrawingArea,
    inner: Rc<Inner>,
}

impl Indicator {
    /// Creates an indicator that tracks buttons within the given row.
    pub fn new(row: &gtk::Box, height: u32, duration_ms: u32) -> Self {
        let area = gtk::DrawingArea::new();
        area.set_can_focus(false);

        let context = area.style_context();
        context.add_class("indicator");
        INDICATOR_CSS_PROVIDER.with(|provider| {
            context.add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        });

        let inner = Rc::new(Inner {
            animation: RefCell::new(Animation::default()),
            target: RefCell::new(None),
            ticking: Cell::new(false),
            duration: Duration::from_millis(u64::from(duration_ms)),
            height: f64::from(height),
        });

        area.connect_draw({
            let inner = Rc::clone(&inner);
            let row = row.clone();
            move |area, cr| {
                let context = area.style_context();
                if let Some(rect) = inner.rect(&row, &context) {
                    gtk::render_background(&context, cr, rect.x, rect.y, rect.width, rect.height);
                    gtk::render_frame(&context, cr, rect.x, rect.y, rect.width, rect.height);
                }

                Propagation::Proceed
            }
        });

        Self { area, inner }
    }

    /// The widget to add to the page overlay.
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Points the pill at the given button, sliding from wherever it currently is.
    ///
    /// Pass `animate` as false to move it without animating, which is what you want when the
    /// page itself is sliding, or when the pill wasn't visible to begin with.
    pub fn set_target(&self, row: &gtk::Box, button: Option<&gtk::Button>, animate: bool) {
        if self.inner.target.borrow().as_ref() == button && !self.inner.animating() {
            // Already settled in the right place, so there's nothing to do beyond the redraws
            // the row will ask for anyway.
            return;
        }

        let previous = self.inner.rect(row, &self.area.style_context());

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
        self.area.queue_draw();
    }

    /// Drives redraws for the length of the animation, then stops so we're not waking up for
    /// every frame the bar draws.
    fn start_tick(&self) {
        if self.inner.ticking.replace(true) {
            return;
        }

        let inner = Rc::clone(&self.inner);
        self.area.add_tick_callback(move |area, _clock| {
            area.queue_draw();

            if inner.animating() {
                ControlFlow::Continue
            } else {
                inner.ticking.set(false);
                ControlFlow::Break
            }
        });
    }
}
