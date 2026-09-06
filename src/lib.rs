use std::{
    collections::{BTreeMap, BTreeSet, HashMap, btree_map::Entry},
    rc::Rc,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_channel::Sender;
use button::Button;
use config::{Config, ScrollScope};
use error::Error;
use indicator::{Indicator, Options as IndicatorOptions};
use niri::{Snapshot, Window};
use notify::EnrichedNotification;
use output::Matcher;
use process::Process;
use state::{Event, State};
use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};
use waybar_cffi::{
    Module,
    gtk::{
        self, Orientation, StackTransitionType,
        gdk::{self, EventMask, EventScroll, ScrollDirection},
        gio,
        glib::{self, MainContext, Propagation, SignalHandlerId, object::Cast},
        prelude::{IsA, ObjectExt, WidgetExtManual},
        traits::{BoxExt, ContainerExt, StackExt, StyleContextExt, WidgetExt},
    },
    waybar_module,
};

mod button;
mod config;
mod error;
mod icon;
mod indicator;
mod niri;
mod notify;
mod output;
mod process;
mod state;

static TRACING: LazyLock<()> = LazyLock::new(|| {
    if let Err(e) = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_span_events(FmtSpan::CLOSE)
        .try_init()
    {
        eprintln!("cannot install global tracing subscriber: {e}");
    }
});

struct TaskbarModule {
    task: Option<glib::JoinHandle<()>>,
}

impl Module for TaskbarModule {
    type Config = Config;

    fn init(info: &waybar_cffi::InitInfo, config: Config) -> Self {
        // Ensure tracing-subscriber is initialised.
        *TRACING;

        let state = State::new(config);

        let context = MainContext::default();
        let task = match context.block_on(init(info, state)) {
            Ok(task) => Some(task),
            Err(e) => {
                tracing::error!(%e, "Niri taskbar module init failed");
                None
            }
        };

        Self { task }
    }
}

impl Drop for TaskbarModule {
    fn drop(&mut self) {
        // Waybar destroys and recreates bars as outputs come and go, so make sure the instance
        // task (and, through it, the Niri event stream thread) actually stops rather than
        // lingering on and updating a widget that is no longer shown.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

waybar_module!(TaskbarModule);

#[tracing::instrument(level = "DEBUG", skip_all, err)]
async fn init(info: &waybar_cffi::InitInfo, state: State) -> Result<glib::JoinHandle<()>, Error> {
    // Set up the box that we'll use to contain the actual window buttons.
    let root = info.get_root_widget();
    // The container is a stack with one page per workspace (or a single page, if we're showing
    // every workspace), so that switching workspaces can slide between pages like Niri does.
    let container = gtk::Stack::new();
    container.style_context().add_class("niri-taskbar");
    container.set_hhomogeneous(false);
    container.set_vhomogeneous(true);
    container.set_interpolate_size(true);
    container.set_transition_duration(state.config().workspace_animation_ms());
    root.add(&container);

    let scroll_state = Arc::new(Mutex::new(ScrollState::default()));
    install_scroll_handler(&container, state.clone(), scroll_state.clone());

    // We need to spawn a task to receive the window snapshots and update the container.
    let context = MainContext::default();
    let task = context
        .spawn_local(async move { Instance::new(state, container, scroll_state).task().await });

    Ok(task)
}

#[derive(Default)]
struct ScrollState {
    visible_window_ids: Vec<u64>,
    current_idx: Option<usize>,
    smooth_vertical: f64,
    smooth_horizontal: f64,
    last_handled_event_time: u32,
}

impl ScrollState {
    fn cycle_target(&mut self, forward: bool, wrap: bool) -> Option<u64> {
        let len = self.visible_window_ids.len();
        if len == 0 {
            self.current_idx = None;
            return None;
        }

        let target_idx = match (self.current_idx, forward, wrap) {
            (Some(idx), true, _) if idx + 1 < len => idx + 1,
            (Some(idx), false, _) if idx > 0 => idx - 1,
            (Some(_), true, true) => 0,
            (Some(_), false, true) => len - 1,
            (Some(_), _, false) => return None,
            (None, true, _) => 0,
            (None, false, _) => len - 1,
        };

        self.current_idx = Some(target_idx);
        Some(self.visible_window_ids[target_idx])
    }
}

fn install_scroll_handler<W: IsA<gtk::Widget> + Clone + 'static>(
    root: &W,
    state: State,
    scroll_state: Arc<Mutex<ScrollState>>,
) {
    if !state.config().scroll_windows() {
        return;
    }

    match state.config().scroll_scope() {
        ScrollScope::Taskbar => {
            connect_scroll_handler(root, state, scroll_state);
        }
        ScrollScope::Bar => {
            let installed = Arc::new(AtomicBool::new(false));
            try_install_bar_scroll_handler(
                root,
                state.clone(),
                scroll_state.clone(),
                installed.clone(),
            );

            let state_clone = state.clone();
            let scroll_state_clone = scroll_state.clone();
            root.connect_realize(move |root| {
                try_install_bar_scroll_handler(
                    root,
                    state_clone.clone(),
                    scroll_state_clone.clone(),
                    installed.clone(),
                );
            });
        }
    }
}

fn try_install_bar_scroll_handler<W: IsA<gtk::Widget>>(
    root: &W,
    state: State,
    scroll_state: Arc<Mutex<ScrollState>>,
    installed: Arc<AtomicBool>,
) {
    if installed.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(toplevel) = root.toplevel() else {
        tracing::warn!("cannot install bar scroll handler: no toplevel widget");
        installed.store(false, Ordering::SeqCst);
        return;
    };

    let Ok(window) = toplevel.clone().downcast::<gtk::Window>() else {
        tracing::warn!(widget = ?toplevel, "cannot install bar scroll handler: toplevel is not a GtkWindow");
        installed.store(false, Ordering::SeqCst);
        return;
    };

    connect_scroll_handler(&window, state, scroll_state);
}

fn connect_scroll_handler<W: IsA<gtk::Widget>>(
    widget: &W,
    state: State,
    scroll_state: Arc<Mutex<ScrollState>>,
) {
    widget.add_events(EventMask::SCROLL_MASK | EventMask::SMOOTH_SCROLL_MASK);
    widget.connect_scroll_event(move |_, event| {
        handle_bar_scroll_event(&state, &scroll_state, event)
    });
}

fn handle_bar_scroll_event(
    state: &State,
    scroll_state: &Arc<Mutex<ScrollState>>,
    event: &EventScroll,
) -> Propagation {
    let wrap = state.config().scroll_wrap();
    let reverse = state.config().scroll_reverse();
    let mut scroll_state = scroll_state.lock().expect("scroll state lock");
    if scroll_state.visible_window_ids.is_empty() {
        return Propagation::Proceed;
    }

    let forward = match event.direction() {
        ScrollDirection::Up => Some(true),
        ScrollDirection::Down => Some(false),
        ScrollDirection::Right => Some(true),
        ScrollDirection::Left => Some(false),
        ScrollDirection::Smooth => {
            let (dx, dy) = event.delta();

            if event.is_stop() {
                scroll_state.smooth_vertical = 0.0;
                scroll_state.smooth_horizontal = 0.0;
                return Propagation::Stop;
            }

            if dy.abs() >= dx.abs() {
                scroll_state.smooth_vertical += dy;
                if scroll_state.smooth_vertical <= -1.0 {
                    scroll_state.smooth_vertical = 0.0;
                    Some(true)
                } else if scroll_state.smooth_vertical >= 1.0 {
                    scroll_state.smooth_vertical = 0.0;
                    Some(false)
                } else {
                    None
                }
            } else {
                scroll_state.smooth_horizontal += dx;
                if scroll_state.smooth_horizontal >= 1.0 {
                    scroll_state.smooth_horizontal = 0.0;
                    Some(true)
                } else if scroll_state.smooth_horizontal <= -1.0 {
                    scroll_state.smooth_horizontal = 0.0;
                    Some(false)
                } else {
                    None
                }
            }
        }
        _ => return Propagation::Proceed,
    };

    let Some(mut forward) = forward else {
        return Propagation::Stop;
    };

    if reverse {
        forward = !forward;
    }

    if scroll_state.last_handled_event_time == event.time() {
        return Propagation::Stop;
    }

    let target = match scroll_state.cycle_target(forward, wrap) {
        Some(target) => target,
        None => return Propagation::Stop,
    };
    scroll_state.last_handled_event_time = event.time();
    drop(scroll_state);

    if let Err(e) = state.niri().activate_window(target) {
        tracing::warn!(%e, id = target, "error trying to activate window from bar scroll");
    }

    Propagation::Stop
}

/// How long to wait before retrying the output match after an attempt that couldn't determine
/// which output the bar is on.
const OUTPUT_FILTER_RETRY: Duration = Duration::from_secs(2);

/// How long after startup to re-check the output match. Gdk may not yet know which monitor the
/// bar is on when the module is first initialised, so an early match can be wrong.
const OUTPUT_FILTER_RECHECK: Duration = Duration::from_secs(2);

/// Identifies a page in the taskbar stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PageKey {
    /// The single page used when every workspace is shown at once.
    All,
    /// The page for a specific workspace, by Niri workspace ID.
    Workspace(u64),
}

impl PageKey {
    fn name(self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Workspace(id) => format!("workspace-{id}"),
        }
    }
}

/// One page of the taskbar stack: a row of buttons, with an optional focus indicator painted
/// over the top of them.
#[derive(Clone)]
struct Page {
    row: gtk::Box,
    indicator: Option<Rc<Indicator>>,
}

/// A window button, along with the page it currently lives on, and the hover tracking that page
/// installed on it.
struct Slot {
    button: Button,
    page: PageKey,
    hover: Option<SignalHandlerId>,
}

struct Instance {
    buttons: BTreeMap<u64, Slot>,
    container: gtk::Stack,
    /// The workspace index of the currently visible page, used to pick the slide direction.
    current_idx: Option<u8>,
    display_handlers: Vec<(gdk::Display, SignalHandlerId)>,
    filter: output::Filter,
    filter_matched: bool,
    last_filter_attempt: Option<Instant>,
    last_snapshot: Option<Snapshot>,
    pages: BTreeMap<PageKey, Page>,
    scroll_state: Arc<Mutex<ScrollState>>,
    state: State,
}

impl Instance {
    pub fn new(state: State, container: gtk::Stack, scroll_state: Arc<Mutex<ScrollState>>) -> Self {
        Self {
            buttons: Default::default(),
            container,
            current_idx: None,
            display_handlers: Vec::new(),
            filter: output::Filter::ShowAll,
            filter_matched: false,
            last_filter_attempt: None,
            last_snapshot: None,
            pages: Default::default(),
            scroll_state,
            state,
        }
    }

    pub async fn task(&mut self) {
        let (tx, rx) = self.state.event_stream();
        self.connect_output_signals(&tx);

        // We have to build the output filter here, because until the Glib event loop has run the
        // container hasn't been realised, which means we can't figure out which output we're on.
        self.refresh_output_filter().await;

        while let Ok(event) = rx.recv().await {
            match event {
                Event::Notification(notification) => self.process_notification(notification).await,
                Event::WindowSnapshot(windows) => {
                    self.maybe_retry_output_filter().await;
                    self.process_window_snapshot(windows);
                }
                Event::OutputsChanged => {
                    if self.refresh_output_filter().await {
                        // The filter changed, so re-apply the last snapshot now rather than
                        // waiting for the next window event to come along.
                        if let Some(snapshot) = self.last_snapshot.clone() {
                            self.process_window_snapshot(snapshot);
                        }
                    }
                }
            }
        }
    }

    /// Connects the Gtk and Gdk signals that indicate the outputs (or the bar's position on them)
    /// may have changed.
    fn connect_output_signals(&mut self, tx: &Sender<Event>) {
        let notify = {
            let tx = tx.clone();
            move || {
                // A closed channel just means this instance is going away.
                let _ = tx.try_send(Event::OutputsChanged);
            }
        };

        // Monitors coming and going.
        let display = self.container.display();
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
        if let Some(toplevel) = self.container.toplevel() {
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
    async fn refresh_output_filter(&mut self) -> bool {
        self.last_filter_attempt = Some(Instant::now());

        let previous = self.filter.clone();
        match self.build_output_filter().await {
            Some(filter) => {
                self.filter = filter;
                self.filter_matched = true;
            }
            None => {
                // Keep whatever we had (which, if we've never matched, is showing everything) and
                // retry later.
                self.filter_matched = false;
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
    async fn maybe_retry_output_filter(&mut self) {
        if self.filter_matched {
            return;
        }

        if self
            .last_filter_attempt
            .is_none_or(|attempt| attempt.elapsed() >= OUTPUT_FILTER_RETRY)
        {
            self.refresh_output_filter().await;
        }
    }

    /// Works out which output this bar is on, returning `None` if it can't be determined right
    /// now.
    #[tracing::instrument(level = "DEBUG", skip(self))]
    async fn build_output_filter(&self) -> Option<output::Filter> {
        if self.state.config().show_all_outputs() {
            return Some(output::Filter::ShowAll);
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

        let Some(window) = self.container.window() else {
            tracing::warn!("cannot get Gdk window for container");
            return None;
        };

        let display = window.display();
        let Some(monitor) = display.monitor_at_window(&window) else {
            tracing::warn!(display = ?window.display(), geometry = ?window.geometry(), "cannot get monitor for window");
            return None;
        };

        for (name, output) in outputs.iter() {
            let matches = output::Matcher::new(&monitor, output);
            if matches == Matcher::all() {
                return Some(output::Filter::Only(name.clone()));
            }
        }

        // If there's only one output, then this bar must be on it, even if Gdk's idea of the
        // monitor doesn't line up with Niri's (which can happen around mode changes).
        if outputs.len() == 1 {
            let name = outputs.into_keys().next()?;
            tracing::debug!(name, "only one Niri output; assuming the bar is on it");
            return Some(output::Filter::Only(name));
        }

        tracing::warn!(?monitor, "no Niri output matched the Gdk monitor");
        None
    }

    #[tracing::instrument(level = "TRACE", skip(self))]
    async fn process_notification(&mut self, notification: Box<EnrichedNotification>) {
        // We'll try to set the urgent class on the relevant window if we can
        // figure out which toplevel is associated with the notification.
        //
        // Obviously, for that, we need toplevels.
        let Some(toplevels) = &self.last_snapshot else {
            return;
        };

        if let Some(mut pid) = notification.pid() {
            tracing::trace!(
                pid,
                "got notification with PID; trying to match it to a toplevel"
            );

            // If we have the sender PID — either from the notification itself,
            // or D-Bus — then the heuristic we'll use is to walk up from the
            // sender PID and see if any of the parents are toplevels.
            //
            // The easiest way to do that is with a map, which we can build from
            // the toplevels.
            let pids = PidWindowMap::new(toplevels.windows.iter());

            // We'll track if we found anything, since we might fall back to
            // some fuzzy matching.
            let mut found = false;

            loop {
                if let Some(window) = pids.get(pid) {
                    // If the window is already focused, there isn't really much
                    // to do.
                    if !window.is_focused {
                        if let Some(button) = self.buttons.get(&window.id).map(|slot| &slot.button)
                        {
                            tracing::trace!(
                                ?button,
                                ?window,
                                pid,
                                "found matching window; setting urgent"
                            );
                            button.set_urgent();
                            found = true;
                        }
                    }
                }

                match Process::new(pid).await {
                    Ok(Process { ppid }) => {
                        if let Some(ppid) = ppid {
                            // Keep walking up.
                            pid = ppid;
                        } else {
                            // There are no more parents.
                            break;
                        }
                    }
                    Err(e) => {
                        // On error, we'll log but do nothing else: this
                        // shouldn't be fatal for the bar, since it's possible
                        // the process has simply already exited.
                        tracing::info!(pid, %e, "error walking up process tree");
                        break;
                    }
                }
            }

            // If we marked one or more toplevels as urgent, then we're done.
            if found {
                return;
            }
        }

        tracing::trace!("no PID in notification, or no match found");

        // Otherwise, we'll fall back to the desktop entry if we got one, and
        // see what we can find.
        //
        // There are a bunch of things that can get in the way here.
        // Applications don't necessarily know the application ID they're
        // registered under on the system: Flatpaks, for instance, have no idea
        // what the Flatpak actually called them when installed. So we'll do our
        // best and make some educated guesses, but that's really what it is.
        if !self.state.config().notifications_use_desktop_entry() {
            tracing::trace!("use of desktop entries is disabled; no match found");
            return;
        }
        let Some(desktop_entry) = &notification.notification().hints.desktop_entry else {
            tracing::trace!("no desktop entry found in notification; nothing more to be done");
            return;
        };

        // So we only have to walk the window list once, we'll keep track of the
        // fuzzy matches we find, even if we don't use them.
        let use_fuzzy = self.state.config().notifications_use_fuzzy_matching();
        let mut fuzzy = Vec::new();

        // XXX: do we still need this with fuzzy matching?
        let mapped = self
            .state
            .config()
            .notifications_app_map(desktop_entry)
            .unwrap_or(desktop_entry);
        let mapped_lower = mapped.to_lowercase();
        let mapped_last_lower = mapped
            .split('.')
            .next_back()
            .unwrap_or_default()
            .to_lowercase();

        let mut found = false;
        for window in toplevels.windows.iter() {
            let Some(app_id) = window.app_id.as_deref() else {
                continue;
            };

            if app_id == mapped {
                if let Some(button) = self.buttons.get(&window.id).map(|slot| &slot.button) {
                    tracing::trace!(app_id, ?button, ?window, "toplevel match found via app ID");
                    button.set_urgent();
                    found = true;
                }
            } else if use_fuzzy {
                // See if we have a fuzzy match, which we'll basically specify
                // as "does the app ID match case insensitively, or does the
                // last component of the app ID match the last component of the
                // desktop entry?".
                if app_id.to_lowercase() == mapped_lower {
                    tracing::trace!(
                        app_id,
                        ?window,
                        "toplevel match found via case-transformed app ID"
                    );
                    fuzzy.push(window.id);
                } else if app_id.contains('.') {
                    tracing::trace!(
                        app_id,
                        ?window,
                        "toplevel match found via last element of app ID"
                    );
                    if let Some(last) = app_id.split('.').next_back() {
                        if last.to_lowercase() == mapped_last_lower {
                            fuzzy.push(window.id);
                        }
                    }
                }
            }
        }

        if !found {
            for id in fuzzy.into_iter() {
                if let Some(button) = self.buttons.get(&id).map(|slot| &slot.button) {
                    button.set_urgent();
                }
            }
        }
    }

    #[tracing::instrument(level = "DEBUG", skip(self))]
    fn process_window_snapshot(&mut self, snapshot: Snapshot) {
        let filter = self.filter.clone();
        let active_workspace_only = self.state.config().active_workspace_only();
        let animation_ms = self.state.config().workspace_animation_ms();

        // Work out which page should be visible: the active workspace on this output (preferring
        // the focused workspace if the filter spans several outputs), or the single "all" page.
        let (target, target_idx) = if active_workspace_only {
            let active: Vec<_> = snapshot
                .workspaces
                .iter()
                .filter(|ws| {
                    ws.is_active && filter.should_show(ws.output.as_deref().unwrap_or_default())
                })
                .collect();

            match active.iter().find(|ws| ws.is_focused).or(active.first()) {
                Some(ws) => (PageKey::Workspace(ws.id), Some(ws.idx)),
                None => (PageKey::All, None),
            }
        } else {
            (PageKey::All, None)
        };

        // We need to track which, if any, windows are no longer present.
        let mut omitted = self.buttons.keys().copied().collect::<BTreeSet<_>>();
        let mut visible_window_ids = Vec::new();
        let mut focused_window_id = None;

        for window in snapshot
            .windows
            .iter()
            .filter(|window| filter.should_show(window.output().unwrap_or_default()))
        {
            let key = if active_workspace_only {
                PageKey::Workspace(window.workspace().id)
            } else {
                PageKey::All
            };
            let page = self.ensure_page(key);

            let slot = match self.buttons.entry(window.id) {
                Entry::Occupied(entry) => {
                    let slot = entry.into_mut();
                    if slot.page != key {
                        // The window moved to another workspace, so move its button along with
                        // it.
                        if let Some(old) = self.pages.get(&slot.page) {
                            old.row.remove(slot.button.widget());
                        }
                        if let Some(handler) = slot.hover.take() {
                            slot.button.widget().disconnect(handler);
                        }
                        page.row.add(slot.button.widget());
                        slot.hover = page
                            .indicator
                            .as_ref()
                            .map(|indicator| indicator.watch(slot.button.widget()));
                        slot.page = key;
                    }
                    slot
                }
                Entry::Vacant(entry) => {
                    let button = Button::new(&self.state, window);

                    // Implicitly adding the button widget to the page as we create it simplifies
                    // reordering, since it means we can just do it as we go.
                    page.row.add(button.widget());
                    let hover = page
                        .indicator
                        .as_ref()
                        .map(|indicator| indicator.watch(button.widget()));
                    entry.insert(Slot {
                        button,
                        page: key,
                        hover,
                    })
                }
            };

            // Update the window properties.
            slot.button.set_focus(window.is_focused);
            slot.button.set_title(window.title.as_deref());

            // Ensure we don't remove this button from the container.
            omitted.remove(&window.id);

            // Since we get the windows in order in the snapshot, we can just push this to the
            // back and then let other widgets push in front as we iterate.
            page.row.reorder_child(slot.button.widget(), -1);

            if key == target {
                visible_window_ids.push(window.id);
                if window.is_focused {
                    focused_window_id = Some(window.id);
                }
            }
        }

        // Remove any windows that no longer exist.
        for id in omitted.into_iter() {
            if let Some(slot) = self.buttons.remove(&id) {
                if let Some(page) = self.pages.get(&slot.page) {
                    page.row.remove(slot.button.widget());
                }
            }
        }

        // The target page may be a workspace with no windows on it yet.
        self.ensure_page(target);

        // Ensure everything is rendered. This has to happen before switching pages, since a stack
        // won't switch to a child that isn't visible.
        self.container.show_all();

        // Switch pages, sliding in the same direction Niri moves its workspaces.
        let name = target.name();
        let page_changed = self.container.visible_child_name().as_deref() != Some(name.as_str());
        if page_changed {
            let transition = match (self.current_idx, target_idx) {
                _ if animation_ms == 0 => StackTransitionType::None,
                (Some(from), Some(to)) if to > from => StackTransitionType::SlideUp,
                (Some(from), Some(to)) if to < from => StackTransitionType::SlideDown,
                _ => StackTransitionType::None,
            };
            self.container.set_visible_child_full(&name, transition);
        }
        self.current_idx = target_idx;

        // Slide the focus indicator to the focused button. There's no point animating across a
        // page change, since the whole page is already moving.
        if let Some(page) = self.pages.get(&target) {
            if let Some(indicator) = &page.indicator {
                let button = focused_window_id
                    .and_then(|id| self.buttons.get(&id))
                    .map(|slot| slot.button.widget().clone());
                indicator.set_focus(button.as_ref(), !page_changed);
                // Urgency can have changed without focus moving at all.
                indicator.refresh();
            }
        }

        self.prune_pages(&snapshot, target);

        // Update the bar-wide scroll state.
        let mut scroll_state = self.scroll_state.lock().expect("scroll state lock");
        let previous_idx = scroll_state.current_idx;
        scroll_state.visible_window_ids = visible_window_ids;
        scroll_state.current_idx = focused_window_id
            .and_then(|id| {
                scroll_state
                    .visible_window_ids
                    .iter()
                    .position(|candidate| *candidate == id)
            })
            .or_else(|| {
                previous_idx
                    .map(|idx| idx.min(scroll_state.visible_window_ids.len().saturating_sub(1)))
            });
        drop(scroll_state);

        // Update the last snapshot.
        self.last_snapshot = Some(snapshot);
    }

    /// Returns the page for the given key, creating it if necessary.
    fn ensure_page(&mut self, key: PageKey) -> Page {
        if let Some(page) = self.pages.get(&key) {
            return page.clone();
        }

        let row = gtk::Box::new(Orientation::Horizontal, 0);

        let config = self.state.config();
        let indicator =
            if config.focus_indicator() || config.hover_indicator() || config.urgent_indicator() {
                Some(Rc::new(Indicator::new(
                    &row,
                    IndicatorOptions {
                        focus: config.focus_indicator(),
                        focus_height: config.focus_indicator_height(),
                        focus_ms: config.focus_indicator_ms(),
                        hover: config.hover_indicator(),
                        hover_height: config.hover_indicator_height(),
                        hover_ms: config.hover_indicator_ms(),
                        urgent: config.urgent_indicator(),
                        urgent_height: config.urgent_indicator_height(),
                        urgent_pulse_ms: config.urgent_indicator_pulse_ms(),
                    },
                )))
            } else {
                None
            };

        row.show();
        self.container.add_named(&row, &key.name());

        let page = Page { row, indicator };
        self.pages.insert(key, page.clone());

        page
    }

    /// Removes pages for workspaces that no longer exist.
    fn prune_pages(&mut self, snapshot: &Snapshot, target: PageKey) {
        // Removing the page a transition is sliding away from would cut the animation short, so
        // leave stale pages alone until the stack is idle: they're empty and hidden anyway, and
        // we'll get another chance on the next snapshot.
        if self.container.is_transition_running() {
            return;
        }

        let stale: Vec<_> = self
            .pages
            .keys()
            .copied()
            .filter(|key| match key {
                PageKey::All => false,
                PageKey::Workspace(id) => {
                    *key != target && !snapshot.workspaces.iter().any(|ws| ws.id == *id)
                }
            })
            .collect();

        for key in stale {
            if let Some(page) = self.pages.remove(&key) {
                self.container.remove(&page.row);
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // The display outlives us, so we have to disconnect our handlers explicitly.
        for (display, handler) in self.display_handlers.drain(..) {
            display.disconnect(handler);
        }
    }
}

/// A basic map of PIDs to windows.
///
/// Windows that don't have a PID are ignored, since we can't match on them
/// anyway. (Also, how does that happen?)
struct PidWindowMap<'a>(HashMap<i64, &'a Window>);

impl<'a> PidWindowMap<'a> {
    fn new(iter: impl Iterator<Item = &'a Window>) -> Self {
        Self(
            iter.filter_map(|window| window.pid.map(|pid| (i64::from(pid), window)))
                .collect(),
        )
    }

    fn get(&self, pid: i64) -> Option<&'a Window> {
        self.0.get(&pid).copied()
    }
}
