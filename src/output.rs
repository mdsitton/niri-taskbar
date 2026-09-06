use std::time::{Duration, Instant};

use async_channel::Sender;
use niri_ipc::{LogicalOutput, Output};
use waybar_cffi::gtk::{
    self as gtk, gdk,
    gdk::{Monitor, traits::MonitorExt},
    gio, glib,
    glib::{SignalHandlerId, object::Cast},
    prelude::{IsA, ObjectExt},
    traits::WidgetExt,
};

use crate::state::{Event, State};

/// A filter to check if we should include a window button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    ShowAll,
    Only(String),
}

impl Filter {
    /// Checks if toplevels on this output should be shown.
    pub fn should_show(&self, output: &str) -> bool {
        match self {
            Self::ShowAll => true,
            Self::Only(only) => only == output,
        }
    }
}

bitflags::bitflags! {
    /// A simple matcher to try to figure out if a Gdk 3 monitor and a Niri output are referring to
    /// the same output.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Matcher: u8 {
        const GEOMETRY = 1 << 0;
        const MODEL = 1 << 1;
        const MANUFACTURER = 1 << 2;
    }
}

impl Matcher {
    pub fn new(monitor: &Monitor, output: &Output) -> Self {
        let Some(logical) = &output.logical else {
            tracing::info!(name = output.name, "output does not have a logical output");
            return Self::empty();
        };

        let mut matches = Self::empty();

        matches.set(
            Matcher::GEOMETRY,
            Geometry::from_gdk_monitor(monitor) == Geometry::from_niri_output(logical),
        );

        matches.set(
            Matcher::MODEL,
            match (monitor.model(), &output.model) {
                (Some(gdk_model), niri_model) => gdk_model.as_str() == niri_model,
                (None, niri_model) if niri_model.is_empty() => true,
                _ => false,
            },
        );

        matches.set(
            Matcher::MANUFACTURER,
            match (monitor.manufacturer(), &output.make) {
                (Some(gdk_manufacturer), niri_make) => gdk_manufacturer.as_str() == niri_make,
                (None, niri_make) if niri_make.is_empty() => true,
                _ => false,
            },
        );

        matches
    }
}

#[derive(Debug, Clone, Copy)]
struct Geometry {
    width: i32,
    height: i32,
    x: i32,
    y: i32,
}

impl Geometry {
    fn from_gdk_monitor(monitor: &Monitor) -> Self {
        let geometry = monitor.geometry();
        let scale = monitor.scale_factor();

        Self {
            width: geometry.width() * scale,
            height: geometry.height() * scale,
            x: geometry.x() * scale,
            y: geometry.y() * scale,
        }
    }

    fn from_niri_output(logical: &LogicalOutput) -> Self {
        let LogicalOutput {
            width,
            height,
            scale,
            x,
            y,
            ..
        } = logical;

        // We'll apply the same general calculation as Gdk 3: any fractional component will be
        // rounded up.
        let scale = scale.ceil() as i32;

        Self {
            width: (*width as i32) * scale,
            height: (*height as i32) * scale,
            x: (*x) * scale,
            y: (*y) * scale,
        }
    }
}

impl PartialEq for Geometry {
    fn eq(&self, other: &Self) -> bool {
        // x and y should be the same regardless, but Gdk is apparently... uh, special when it comes
        // to calculating the width and height of the monitor, so we'll define it as "close enough
        // is good enough".
        let x_delta = ((self.width as f64) / (other.width as f64)) - 1.0;
        let y_delta = ((self.height as f64) / (other.height as f64)) - 1.0;

        x_delta.abs() < 0.03 && y_delta.abs() < 0.03 && self.x == other.x && self.y == other.y
    }
}

/// How long to wait before retrying the output match after an attempt that couldn't determine
/// which output the bar is on.
const OUTPUT_FILTER_RETRY: Duration = Duration::from_secs(2);

/// How long after startup to re-check the output match. Gdk may not yet know which monitor the
/// bar is on when the module is first initialised, so an early match can be wrong.
const OUTPUT_FILTER_RECHECK: Duration = Duration::from_secs(2);

/// Works out, and keeps working out, which Niri output a bar is sitting on.
///
/// Outputs come and go, and Gdk doesn't necessarily know which monitor a bar is on the first time
/// we ask, so this re-checks whenever the outputs might have changed and holds on to the last
/// good answer in the meantime.
pub struct Tracker {
    state: State,
    widget: gtk::Widget,
    filter: Filter,
    matched: bool,
    last_attempt: Option<Instant>,
    display_handlers: Vec<(gdk::Display, SignalHandlerId)>,
}

impl Tracker {
    pub fn new(state: State, widget: &impl IsA<gtk::Widget>) -> Self {
        Self {
            state,
            widget: widget.clone().upcast(),
            filter: Filter::ShowAll,
            matched: false,
            last_attempt: None,
            display_handlers: Vec::new(),
        }
    }

    /// The filter as it currently stands.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Connects the Gtk and Gdk signals that indicate the outputs (or the bar's position on them)
    /// may have changed.
    pub fn connect_signals(&mut self, tx: &Sender<Event>) {
        let notify = {
            let tx = tx.clone();
            move || {
                // A closed channel just means this instance is going away.
                let _ = tx.try_send(Event::OutputsChanged);
            }
        };

        // Monitors coming and going.
        let display = self.widget.display();
        let added = display.connect_monitor_added({
            let notify = notify.clone();
            move |_, _| notify()
        });
        let removed = display.connect_monitor_removed({
            let notify = notify.clone();
            move |_, _| notify()
        });
        self.display_handlers.push((display.clone(), added));
        self.display_handlers.push((display, removed));

        // The bar being resized or moved, for instance because its output changed mode.
        if let Some(toplevel) = self.widget.toplevel() {
            let notify = notify.clone();
            toplevel.connect_configure_event(move |_, _| {
                notify();
                false
            });
        } else {
            tracing::warn!("no toplevel widget; cannot watch for bar configure events");
        }

        // Gdk may not know which monitor the bar is on when we first start, so re-check once
        // things have settled.
        glib::timeout_add_local_once(OUTPUT_FILTER_RECHECK, notify);
    }

    /// Re-evaluates the output filter, keeping the previous filter if the output can't currently
    /// be determined. Returns true if the filter changed.
    #[tracing::instrument(level = "DEBUG", skip(self))]
    pub async fn refresh(&mut self) -> bool {
        self.last_attempt = Some(Instant::now());

        let previous = self.filter.clone();
        match self.build().await {
            Some(filter) => {
                self.filter = filter;
                self.matched = true;
            }
            None => {
                // Keep whatever we had (which, if we've never matched, is showing everything) and
                // retry later.
                self.matched = false;
            }
        }

        if self.filter != previous {
            tracing::info!(?previous, filter = ?self.filter, "output filter changed");
            true
        } else {
            false
        }
    }

    /// Retries the output match if the last attempt failed and enough time has passed.
    pub async fn maybe_retry(&mut self) {
        if self.matched {
            return;
        }

        if self
            .last_attempt
            .is_none_or(|attempt| attempt.elapsed() >= OUTPUT_FILTER_RETRY)
        {
            self.refresh().await;
        }
    }

    /// Works out which output this bar is on, returning `None` if it can't be determined right
    /// now.
    #[tracing::instrument(level = "DEBUG", skip(self))]
    async fn build(&self) -> Option<Filter> {
        if self.state.config().show_all_outputs() {
            return Some(Filter::ShowAll);
        }

        // OK, so we need to figure out what output we're on. Easy, right?
        //
        // Not so fast!
        //
        // In-tree Waybar modules have access to a Wayland client called `Client`, which they can
        // use to access the `wl_display` the bar is created against, and further access metadata
        // from there. Unfortunately, none of that is exposed in CFFI, and, honestly, I'm not really
        // sure how you would trivially wrap it in a C API.
        //
        // We have the Gtk 3 container, though, so that's something — we have to wait until the
        // window has been realised, but that's happened by the time we're in the main loop
        // callback. The problem is that we're also using Gdk 3, which doesn't expose the connection
        // name of the monitor in use, which is the only thing we can match against the Niri output
        // configuration.
        //
        // Now, this wouldn't be so bad on its own, because we _can_ get to the `wl_output` via
        // `gdkwayland`, and version 4 of the core Wayland protocol includes the output name.
        // Unfortunately, we have no way of accessing Gdk's Wayland connection, and Wayland
        // identifiers aren't stable across connections, so we can't just connect to Wayland
        // ourselves and enumerate the outputs. (Trust me, I tried.)
        //
        // So, until Waybar migrates to Gtk 4, that leaves us without a truly reliable solution.
        //
        // What we'll do instead is match up what we can. Niri can tell us everything we want to
        // know about the output, and Gdk 3 does include things like the output geometry, make, and
        // model. So we'll match on those and hope for the best.
        //
        // Since outputs come and go (and Gdk doesn't necessarily know which monitor the bar is on
        // the first time this runs), this gets re-run whenever the outputs might have changed.
        let niri = *self.state.niri();
        let outputs = match gio::spawn_blocking(move || niri.outputs()).await {
            Ok(Ok(outputs)) => outputs,
            Ok(Err(e)) => {
                tracing::warn!(%e, "cannot get Niri outputs");
                return None;
            }
            Err(_) => {
                tracing::error!("error received from gio while waiting for task");
                return None;
            }
        };

        if outputs.is_empty() {
            tracing::warn!("Niri reports no outputs");
            return None;
        }

        let Some(window) = self.widget.window() else {
            tracing::warn!("cannot get Gdk window for container");
            return None;
        };

        let display = window.display();
        let Some(monitor) = display.monitor_at_window(&window) else {
            tracing::warn!(display = ?window.display(), geometry = ?window.geometry(), "cannot get monitor for window");
            return None;
        };

        for (name, output) in outputs.iter() {
            let matches = Matcher::new(&monitor, output);
            if matches == Matcher::all() {
                return Some(Filter::Only(name.clone()));
            }
        }

        // If there's only one output, then this bar must be on it, even if Gdk's idea of the
        // monitor doesn't line up with Niri's (which can happen around mode changes).
        if outputs.len() == 1 {
            let name = outputs.into_keys().next()?;
            tracing::debug!(name, "only one Niri output; assuming the bar is on it");
            return Some(Filter::Only(name));
        }

        tracing::warn!(?monitor, "no Niri output matched the Gdk monitor");
        None
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        // The display outlives us, so we have to disconnect our handlers explicitly.
        for (display, handler) in self.display_handlers.drain(..) {
            display.disconnect(handler);
        }
    }
}
