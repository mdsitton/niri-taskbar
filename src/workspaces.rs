//! The workspace switcher.
//!
//! This is the other thing this module can render, selected with `"mode": "workspaces"`. It
//! shares everything that matters with the taskbar: the same Niri connection and event stream,
//! the same logic for working out which output the bar is on, and the same indicator pills.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    rc::Rc,
};

use waybar_cffi::gtk::{
    self as gtk, ReliefStyle, StyleContext,
    glib::SignalHandlerId,
    prelude::{ButtonExt, ObjectExt},
    traits::{BoxExt, ContainerExt, StyleContextExt, WidgetExt},
};

use crate::{
    indicator::{Indicator, Options as IndicatorOptions},
    niri::Snapshot,
    output,
    state::{Event, State},
};

/// A workspace button, along with what it currently says.
struct Slot {
    button: gtk::Button,
    label: String,
    hover: Option<SignalHandlerId>,
}

pub struct Instance {
    buttons: BTreeMap<u64, Slot>,
    container: gtk::Box,
    indicator: Option<Rc<Indicator>>,
    last_snapshot: Option<Snapshot>,
    outputs: output::Tracker,
    state: State,
}

impl Instance {
    pub fn new(state: State, container: gtk::Box) -> Self {
        let options = {
            let config = state.config();

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
            }
        };

        let indicator = if options.focus || options.hover || options.urgent {
            Some(Rc::new(Indicator::new(&container, options)))
        } else {
            None
        };

        Self {
            buttons: Default::default(),
            indicator,
            last_snapshot: None,
            outputs: output::Tracker::new(state.clone(), &container),
            container,
            state,
        }
    }

    pub async fn task(&mut self) {
        // Notifications are a taskbar concern, so there's no point opening a D-Bus connection
        // for them here.
        let (tx, rx) = self.state.event_stream(false);
        self.outputs.connect_signals(&tx);
        self.outputs.refresh().await;

        while let Ok(event) = rx.recv().await {
            match event {
                Event::WindowSnapshot(snapshot) => {
                    self.outputs.maybe_retry().await;
                    self.render(snapshot);
                }
                Event::OutputsChanged => {
                    if self.outputs.refresh().await {
                        // The set of workspaces we should be showing just changed, so re-apply
                        // the last snapshot rather than waiting for the next one.
                        if let Some(snapshot) = self.last_snapshot.clone() {
                            self.render(snapshot);
                        }
                    }
                }
                Event::Notification(_) => {}
            }
        }
    }

    #[tracing::instrument(level = "DEBUG", skip(self))]
    fn render(&mut self, snapshot: Snapshot) {
        let filter = self.outputs.filter().clone();

        let mut visible: Vec<_> = snapshot
            .workspaces
            .iter()
            .filter(|workspace| filter.should_show(workspace.output.as_deref().unwrap_or_default()))
            .collect();
        visible.sort_by_key(|workspace| (workspace.idx, workspace.id));

        // Track which buttons are no longer wanted.
        let mut omitted = self.buttons.keys().copied().collect::<BTreeSet<_>>();
        // Which workspace the indicator should sit under: the active one on this output, or the
        // focused one if the bar is showing several outputs at once.
        let mut target = None;

        for workspace in visible.iter() {
            omitted.remove(&workspace.id);

            let label = workspace.label();
            let slot = match self.buttons.entry(workspace.id) {
                Entry::Occupied(entry) => {
                    let slot = entry.into_mut();
                    if slot.label != label {
                        slot.button.set_label(&label);
                        slot.label = label;
                    }
                    slot
                }
                Entry::Vacant(entry) => {
                    let button = gtk::Button::with_label(&label);
                    button.set_relief(ReliefStyle::None);

                    let state = self.state.clone();
                    let id = workspace.id;
                    button.connect_clicked(move |_| {
                        if let Err(e) = state.niri().focus_workspace(id) {
                            tracing::warn!(%e, id, "error focusing workspace");
                        }
                    });

                    self.container.add(&button);
                    let hover = self
                        .indicator
                        .as_ref()
                        .map(|indicator| indicator.watch(&button));

                    entry.insert(Slot {
                        button,
                        label,
                        hover,
                    })
                }
            };

            let context = slot.button.style_context();
            set_class(&context, "focused", workspace.is_focused);
            set_class(&context, "active", workspace.is_active);
            set_class(&context, "urgent", workspace.is_urgent);

            // The snapshot is in order, so pushing to the back as we go leaves the row sorted.
            self.container.reorder_child(&slot.button, -1);

            if workspace.is_active && (target.is_none() || workspace.is_focused) {
                target = Some(workspace.id);
            }
        }

        for id in omitted.into_iter() {
            if let Some(slot) = self.buttons.remove(&id) {
                if let Some(handler) = slot.hover {
                    slot.button.disconnect(handler);
                }
                self.container.remove(&slot.button);
            }
        }

        self.container.show_all();

        if let Some(indicator) = &self.indicator {
            let button = target
                .and_then(|id| self.buttons.get(&id))
                .map(|slot| slot.button.clone());

            indicator.set_focus(button.as_ref(), true);
            // Urgency can change without the active workspace moving at all.
            indicator.refresh();
        }

        self.last_snapshot = Some(snapshot);
    }
}

fn set_class(context: &StyleContext, class: &str, present: bool) {
    if present {
        context.add_class(class);
    } else {
        context.remove_class(class);
    }
}
