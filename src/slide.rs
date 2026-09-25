//! Animated movement for the buttons in a row while they're being dragged around.
//!
//! A [`gtk::Box`] only knows how to put its children in their slots, so this sits on top of it:
//! after every allocation the box makes, it moves the dragged button to wherever the pointer has
//! it, which leaves its slot empty, and slides any button whose slot changed from where it was
//! showing to where it now belongs. The buttons are moved for real rather than drawn somewhere
//! else, so the pills, which follow allocations, keep up of their own accord, and the dragged
//! button keeps its pointer grab.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use waybar_cffi::gtk::{
    self as gtk, cairo,
    glib::{ControlFlow, Value, WeakRef, object::Cast, object::ObjectExt, value::ToValue},
    prelude::{ContainerExt, WidgetExt, WidgetExtManual},
};

pub struct Slide {
    /// Lets the tick callback hold on to this without every caller needing an `Rc`.
    this: Weak<Slide>,
    row: WeakRef<gtk::Box>,
    duration: Duration,
    /// The slot the box last gave each child, which is where it belongs once it stops moving.
    slots: RefCell<HashMap<gtk::Widget, gtk::Allocation>>,
    /// Children on their way from one slot to another.
    moving: RefCell<HashMap<gtk::Widget, Move>>,
    /// The button being dragged, and the x position the pointer has it at.
    floating: RefCell<Option<(gtk::Widget, f64)>>,
    /// The button drawn over the others: the dragged one, until it's back in its slot.
    raised: RefCell<Option<gtk::Widget>>,
    /// Set while the raised button is being drawn on top, which is the only time it's drawn at
    /// all: drawing it in its usual turn as well would double up anything translucent.
    drawing_raised: Cell<bool>,
    /// Whether slot changes should slide rather than jump, which is only while a drag is on and
    /// for a moment after, so the rest of the time the row behaves as it always has.
    animate: Cell<bool>,
    animate_until: Cell<Option<Instant>>,
    ticking: Cell<bool>,
}

struct Move {
    from: f64,
    started: Instant,
}

impl Slide {
    /// Starts managing the given row. This has to happen before anything else draws over the
    /// row, so the dragged button ends up underneath the pills rather than over them.
    pub fn new(row: &gtk::Box, duration: Duration) -> Rc<Self> {
        let slide = Rc::new_cyclic(|this| Self {
            this: this.clone(),
            row: row.downgrade(),
            duration,
            slots: Default::default(),
            moving: Default::default(),
            floating: Default::default(),
            raised: Default::default(),
            drawing_raised: Cell::new(false),
            animate: Cell::new(false),
            animate_until: Cell::new(None),
            ticking: Cell::new(false),
        });

        // Size allocation runs the box's own handler first, so by the time this runs every child
        // has just been put in its slot.
        row.connect_size_allocate({
            let slide = Rc::clone(&slide);
            move |row, _| slide.allocated(row)
        });

        // The box draws its children in order, so the raised button gets drawn again on top once
        // they're done. gtk-rs only exposes the before variant of the draw signal, hence going
        // through the generic signal machinery here.
        row.connect_local("draw", true, {
            let slide = Rc::clone(&slide);
            move |values: &[Value]| {
                let row = values.first().and_then(|v| v.get::<gtk::Box>().ok());
                let cr = values.get(1).and_then(|v| v.get::<cairo::Context>().ok());
                let raised = slide.raised.borrow().clone();
                if let (Some(row), Some(cr), Some(raised)) = (row, cr, raised) {
                    if raised.parent().as_ref() == Some(row.upcast_ref()) {
                        slide.drawing_raised.set(true);
                        row.propagate_draw(&raised, &cr);
                        slide.drawing_raised.set(false);
                    }
                }
                Some(false.to_value())
            }
        });

        slide
    }

    /// Returns true if this manages the given row.
    pub fn is_for(&self, row: &gtk::Box) -> bool {
        self.row.upgrade().as_ref() == Some(row)
    }

    /// Returns true if the given child should skip drawing itself right now, because it's raised
    /// and will be drawn on top of the others instead.
    pub fn hides(&self, child: &gtk::Widget) -> bool {
        !self.drawing_raised.get() && self.raised.borrow().as_ref() == Some(child)
    }

    /// Returns true if the row this manages is still around.
    pub fn is_alive(&self) -> bool {
        self.row.upgrade().is_some()
    }

    /// The slot the box has given a child, as opposed to wherever it's being shown right now.
    pub fn slot(&self, child: &gtk::Widget) -> gtk::Allocation {
        self.slots
            .borrow()
            .get(child)
            .copied()
            .unwrap_or_else(|| child.allocation())
    }

    /// Turns sliding on for the length of a drag.
    pub fn start(&self) {
        self.animate.set(true);
    }

    /// Floats the given button at `x`, kept within the row, returning where it actually ended up.
    pub fn float(&self, button: &gtk::Widget, x: f64) -> f64 {
        let x = self.clamp(button, x);
        *self.floating.borrow_mut() = Some((button.clone(), x));
        *self.raised.borrow_mut() = Some(button.clone());
        self.moving.borrow_mut().remove(button);

        if let Some(row) = self.row.upgrade() {
            self.place(button);
            row.queue_draw();
        }
        x
    }

    /// Ends a drag, sliding the floating button back into its slot. Sliding stays on for a
    /// little while after, so the row catching up with Niri afterwards is animated too.
    pub fn finish(&self) {
        self.animate.set(false);
        self.animate_until.set(Some(
            Instant::now() + self.duration + Duration::from_millis(500),
        ));

        if let Some((button, x)) = self.floating.borrow_mut().take() {
            self.moving.borrow_mut().insert(
                button,
                Move {
                    from: x,
                    started: Instant::now(),
                },
            );
        }
        self.tick();
    }

    fn animating(&self) -> bool {
        self.animate.get() || self.animate_until.get().is_some_and(|t| Instant::now() < t)
    }

    fn allocated(&self, row: &gtk::Box) {
        let children = row.children();
        let animate = self.animating();
        let now = Instant::now();

        {
            let mut slots = self.slots.borrow_mut();
            let mut moving = self.moving.borrow_mut();
            let floating = self.floating.borrow();

            for child in children.iter() {
                let slot = child.allocation();
                let Some(old) = slots.insert(child.clone(), slot) else {
                    continue;
                };
                if !animate || old.x() == slot.x() {
                    continue;
                }
                if floating.as_ref().is_some_and(|(button, _)| button == child) {
                    continue;
                }

                // Start from wherever it's showing now, which is partway along if it was
                // already on the move.
                let from = moving
                    .get(child)
                    .map_or(f64::from(old.x()), |m| self.position(m, old.x()));
                moving.insert(child.clone(), Move { from, started: now });
            }

            slots.retain(|child, _| children.contains(child));
            moving.retain(|child, _| children.contains(child));
        }

        for child in children.iter() {
            self.place(child);
        }
        if !self.moving.borrow().is_empty() {
            self.tick();
        }
    }

    /// Where a moving child is right now, on its way to `to`.
    fn position(&self, m: &Move, to: i32) -> f64 {
        let t = if self.duration.is_zero() {
            1.0
        } else {
            ease(m.started.elapsed().as_secs_f64() / self.duration.as_secs_f64())
        };
        m.from + (f64::from(to) - m.from) * t
    }

    /// Puts a child wherever it should be showing right now, if that isn't where it already is.
    fn place(&self, child: &gtk::Widget) {
        let slot = self.slot(child);
        let x = match &*self.floating.borrow() {
            Some((button, x)) if button == child => *x,
            _ => match self.moving.borrow().get(child) {
                Some(m) => self.position(m, slot.x()),
                None => f64::from(slot.x()),
            },
        };

        let allocation =
            gtk::Allocation::new(x.round() as i32, slot.y(), slot.width(), slot.height());
        if child.allocation() != allocation {
            // Gtk complains about allocating a widget that hasn't been measured since it last
            // asked to be, so make sure it has been.
            let _ = child.preferred_size();
            child.size_allocate(&allocation);
        }
    }

    fn clamp(&self, child: &gtk::Widget, x: f64) -> f64 {
        let Some(row) = self.row.upgrade() else {
            return x;
        };
        let row = row.allocation();
        let min = f64::from(row.x());
        let max = f64::from(row.x() + row.width() - self.slot(child).width());
        x.clamp(min, max.max(min))
    }

    /// Drives the slides for as long as anything is moving.
    fn tick(&self) {
        if self.ticking.replace(true) {
            return;
        }
        let (Some(row), Some(slide)) = (self.row.upgrade(), self.this.upgrade()) else {
            self.ticking.set(false);
            return;
        };

        row.add_tick_callback(move |row, _clock| {
            let children: Vec<_> = slide.moving.borrow().keys().cloned().collect();
            for child in children.iter() {
                slide.place(child);
            }

            // Anything that's arrived stops moving, and once the dragged button is home it no
            // longer needs drawing over the others.
            let duration = slide.duration;
            slide
                .moving
                .borrow_mut()
                .retain(|_, m| m.started.elapsed() < duration);
            let moving = slide.moving.borrow();
            let dragging = slide.floating.borrow().is_some();
            {
                let mut raised = slide.raised.borrow_mut();
                if !dragging && raised.as_ref().is_some_and(|r| !moving.contains_key(r)) {
                    *raised = None;
                }
            }
            for child in children.iter().filter(|c| !moving.contains_key(*c)) {
                slide.place(child);
            }
            row.queue_draw();

            if moving.is_empty() {
                slide.ticking.set(false);
                ControlFlow::Break
            } else {
                ControlFlow::Continue
            }
        });
    }
}

/// Ease out, so movement arrives gently instead of stopping dead.
fn ease(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}
