//! The menu that pops up when a taskbar button is right-clicked, with the things you'd otherwise
//! reach for a keybinding to do to that window.

use niri_ipc::{Action, WorkspaceReferenceArg};
use waybar_cffi::gtk::{
    self as gtk,
    gdk::Event,
    glib,
    prelude::{GtkMenuExt, GtkMenuItemExt, MenuShellExt, WidgetExt, WidgetExtManual},
};

use crate::{niri::Niri, state::State};

/// Pops up the menu for the given window under the pointer.
pub fn popup(state: &State, button: &gtk::Button, window_id: u64, event: &Event) {
    let niri = *state.niri();

    // The menu is built from Niri's state as it is right now, rather than whatever the taskbar
    // last saw, since it's only a local socket away and this only happens on a click.
    let windows = niri.windows().unwrap_or_else(|e| {
        tracing::warn!(%e, "cannot get windows for the window menu");
        Vec::new()
    });
    let Some(window) = windows.iter().find(|window| window.id == window_id) else {
        return;
    };
    let workspaces = niri.workspaces().unwrap_or_else(|e| {
        tracing::warn!(%e, "cannot get workspaces for the window menu");
        Vec::new()
    });
    let outputs = niri.outputs().unwrap_or_default();

    let menu = gtk::Menu::new();
    let add = |menu: &gtk::Menu, label: &str, action: Action| {
        let item = gtk::MenuItem::with_label(label);
        item.connect_activate(move |_| run(niri, action.clone()));
        menu.append(&item);
    };

    let floating_label = if window.is_floating {
        "Tile window"
    } else {
        "Float window"
    };
    add(
        &menu,
        floating_label,
        Action::ToggleWindowFloating {
            id: Some(window_id),
        },
    );
    add(
        &menu,
        "Fullscreen",
        Action::FullscreenWindow {
            id: Some(window_id),
        },
    );

    // Niri can only maximize the focused column, so this focuses the window on the way.
    if !window.is_floating {
        let item = gtk::MenuItem::with_label("Maximize column");
        item.connect_activate(move |_| {
            run(niri, Action::FocusWindow { id: window_id });
            run(niri, Action::MaximizeColumn {});
        });
        menu.append(&item);
    }

    // Stacking a window into the column beside it, or taking it back out of a stack. Niri does
    // both with the same action, which consumes a window that's alone in its column and expels one
    // that isn't, so the menu offers whichever one applies.
    if let Some((column, _)) = window.layout.pos_in_scrolling_layout {
        let columns: Vec<usize> = windows
            .iter()
            .filter(|other| other.workspace_id == window.workspace_id)
            .filter_map(|other| other.layout.pos_in_scrolling_layout)
            .map(|(column, _)| column)
            .collect();
        let stacked = columns.iter().filter(|c| **c == column).count() > 1;

        if stacked {
            add(
                &menu,
                "Unstack window",
                Action::ConsumeOrExpelWindowRight {
                    id: Some(window_id),
                },
            );

            // Everything in the stack moves down a slot, with the bottom window coming round to
            // the top. Niri can only move the focused window within its column, so this focuses
            // the bottom one, walks it up to the top, and then gives focus back.
            let mut stack: Vec<_> = windows
                .iter()
                .filter(|other| other.workspace_id == window.workspace_id)
                .filter_map(|other| {
                    let (c, tile) = other.layout.pos_in_scrolling_layout?;
                    (c == column).then_some((tile, other.id))
                })
                .collect();
            stack.sort();
            let bottom = stack.last().map(|(_, id)| *id);
            let steps = stack.len() - 1;
            let focused = windows.iter().find(|w| w.is_focused).map(|w| w.id);
            if let Some(bottom) = bottom {
                let item = gtk::MenuItem::with_label("Cycle stack");
                item.connect_activate(move |_| {
                    run(niri, Action::FocusWindow { id: bottom });
                    for _ in 0..steps {
                        run(niri, Action::MoveWindowUp {});
                    }
                    if let Some(focused) = focused {
                        run(niri, Action::FocusWindow { id: focused });
                    }
                });
                menu.append(&item);
            }
        } else {
            if columns.iter().any(|c| *c < column) {
                add(
                    &menu,
                    "Stack into left column",
                    Action::ConsumeOrExpelWindowLeft {
                        id: Some(window_id),
                    },
                );
            }
            if columns.iter().any(|c| *c > column) {
                add(
                    &menu,
                    "Stack into right column",
                    Action::ConsumeOrExpelWindowRight {
                        id: Some(window_id),
                    },
                );
            }
        }
    }

    // Every workspace except the one the window is already on, grouped by output when there's
    // more than one. Moving it by ID rather than index means it lands on the right output too.
    let current_workspace = window.workspace_id;
    let mut targets: Vec<_> = workspaces
        .iter()
        .filter(|ws| Some(ws.id) != current_workspace)
        .collect();
    targets.sort_by(|a, b| (&a.output, a.idx).cmp(&(&b.output, b.idx)));
    if !targets.is_empty() {
        let submenu = gtk::Menu::new();
        let several_outputs = outputs.len() > 1;
        for ws in targets {
            let name = ws.name.clone().unwrap_or_else(|| ws.idx.to_string());
            let label = match (&ws.output, several_outputs) {
                (Some(output), true) => format!("{name} ({output})"),
                _ => name,
            };
            add(
                &submenu,
                &label,
                Action::MoveWindowToWorkspace {
                    window_id: Some(window_id),
                    reference: WorkspaceReferenceArg::Id(ws.id),
                    focus: false,
                },
            );
        }
        let item = gtk::MenuItem::with_label("Move to workspace");
        item.set_submenu(Some(&submenu));
        menu.append(&item);
    }

    // Every other output, when there are any.
    let current_output = current_workspace
        .and_then(|id| workspaces.iter().find(|ws| ws.id == id))
        .and_then(|ws| ws.output.clone());
    let mut others: Vec<_> = outputs
        .keys()
        .filter(|name| Some(*name) != current_output.as_ref())
        .cloned()
        .collect();
    others.sort();
    if !others.is_empty() {
        let submenu = gtk::Menu::new();
        for output in others {
            let label = describe_output(&outputs, &output);
            add(
                &submenu,
                &label,
                Action::MoveWindowToMonitor {
                    id: Some(window_id),
                    output,
                },
            );
        }
        let item = gtk::MenuItem::with_label("Move to monitor");
        item.set_submenu(Some(&submenu));
        menu.append(&item);
    }

    menu.append(&gtk::SeparatorMenuItem::new());
    add(
        &menu,
        "Close window",
        Action::CloseWindow {
            id: Some(window_id),
        },
    );

    // Attaching the menu to the button keeps it alive while it's open, and lets it pick up the
    // bar's styling. Once it closes it's done with, but it has to outlive the close, since Gtk
    // closes a menu before activating whatever was chosen from it.
    menu.set_attach_widget(Some(button));
    menu.connect_deactivate(|menu| {
        let menu = menu.clone();
        glib::idle_add_local_once(move || {
            // SAFETY: nothing else holds on to the menu once it's closed; it was built just for
            // this popup.
            unsafe { menu.destroy() };
        });
    });
    menu.show_all();
    menu.popup_at_pointer(Some(event));
}

/// Names an output by its make and model where Niri knows them, with the connector alongside,
/// since the connector alone doesn't say much about which screen is which.
fn describe_output(
    outputs: &std::collections::HashMap<String, niri_ipc::Output>,
    name: &str,
) -> String {
    match outputs.get(name) {
        Some(output) => format!("{} {} ({name})", output.make, output.model),
        None => name.to_string(),
    }
}

fn run(niri: Niri, action: Action) {
    if let Err(e) = niri.action(action) {
        tracing::warn!(%e, "error running window menu action");
    }
}
