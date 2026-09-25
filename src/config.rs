use std::collections::HashMap;

use itertools::Itertools;
use regex::Regex;
use serde::{Deserialize, Deserializer};

use crate::gradient::{Config as GradientConfig, Gradient, Space};

/// The taskbar configuration.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    apps: HashMap<String, Vec<AppConfig>>,
    #[serde(default)]
    notifications: Notifications,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    show_all_outputs: bool,
    #[serde(default)]
    active_workspace_only: bool,
    #[serde(default)]
    workspace_animation_ms: u32,
    #[serde(default)]
    focus_indicator: bool,
    #[serde(default = "default_indicator_ms")]
    focus_indicator_ms: u32,
    #[serde(default = "default_indicator_height")]
    focus_indicator_height: u32,
    #[serde(default)]
    focus_indicator_hover_height: Option<u32>,
    #[serde(default)]
    focus_indicator_hover_in: Option<String>,
    #[serde(default)]
    hover_indicator: bool,
    #[serde(default = "default_hover_indicator_ms")]
    hover_indicator_ms: u32,
    #[serde(default = "default_indicator_height")]
    hover_indicator_height: u32,
    #[serde(default)]
    urgent_indicator: bool,
    #[serde(default = "default_indicator_height")]
    urgent_indicator_height: u32,
    #[serde(default = "default_urgent_pulse_ms")]
    urgent_indicator_pulse_ms: u32,
    #[serde(default)]
    scroll_windows: bool,
    #[serde(default)]
    scroll_scope: ScrollScope,
    #[serde(default)]
    scroll_wrap: bool,
    #[serde(default)]
    scroll_reverse: bool,
    #[serde(default)]
    drag_reorder: bool,
    #[serde(default = "default_drag_reorder_slide_ms")]
    drag_reorder_slide_ms: u32,
    #[serde(default)]
    drag_indicator_gradient: Option<GradientConfig>,
    #[serde(default)]
    column_grouping: ColumnGrouping,
}

/// Which of the two widgets this module instance should render.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Taskbar,
    Workspaces,
}

/// How windows that share a Niri column are shown.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColumnGrouping {
    /// A button for every window, as though they had columns of their own.
    #[default]
    None,
    /// One button for the column, which opens up to show the rest while it's hovered.
    Collapse,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScrollScope {
    Taskbar,
    #[default]
    Bar,
}

#[derive(Debug, Deserialize)]
pub struct Notifications {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    map_app_ids: HashMap<String, String>,
    #[serde(default = "default_true")]
    use_desktop_entry: bool,
    #[serde(default)]
    use_fuzzy_matching: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            map_app_ids: Default::default(),
            use_desktop_entry: true,
            use_fuzzy_matching: Default::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Returns all possible CSS classes that a particular application might have set.
    pub fn app_classes(&self, app_id: &str) -> Vec<&str> {
        self.apps
            .get(app_id)
            .map(|configs| {
                configs
                    .iter()
                    .map(|config| config.class.as_str())
                    .collect_vec()
            })
            .unwrap_or_default()
    }

    /// Returns the actual CSS classes that should be set for the given application and title.
    pub fn app_matches<'a>(
        &'a self,
        app_id: &str,
        title: &'a str,
    ) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        match self.apps.get(app_id) {
            Some(configs) => Box::new(
                configs
                    .iter()
                    .filter(|config| config.re.is_match(title))
                    .map(|config| config.class.as_str()),
            ),
            None => Box::new(std::iter::empty()),
        }
    }

    /// Returns true if notification support is enabled.
    pub fn notifications_enabled(&self) -> bool {
        self.notifications.enabled
    }

    /// Returns any mapping that might exist for this app ID.
    pub fn notifications_app_map(&self, app_id: &str) -> Option<&'_ str> {
        self.notifications
            .map_app_ids
            .get(app_id)
            .map(String::as_str)
    }

    /// Returns true if notification support should use the desktop entry as a
    /// fallback.
    pub fn notifications_use_desktop_entry(&self) -> bool {
        self.notifications.use_desktop_entry
    }

    pub fn notifications_use_fuzzy_matching(&self) -> bool {
        self.notifications.use_fuzzy_matching
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn show_all_outputs(&self) -> bool {
        self.show_all_outputs
    }

    pub fn active_workspace_only(&self) -> bool {
        self.active_workspace_only
    }

    pub fn workspace_animation_ms(&self) -> u32 {
        self.workspace_animation_ms
    }

    pub fn focus_indicator(&self) -> bool {
        self.focus_indicator
    }

    pub fn focus_indicator_ms(&self) -> u32 {
        self.focus_indicator_ms
    }

    pub fn focus_indicator_height(&self) -> u32 {
        self.focus_indicator_height
    }

    /// How thick the focus pill grows to while its button is hovered, defaulting to a little
    /// thicker than usual.
    pub fn focus_indicator_hover_height(&self) -> u32 {
        self.focus_indicator_hover_height
            .unwrap_or(self.focus_indicator_height + 2)
    }

    pub fn hover_indicator(&self) -> bool {
        self.hover_indicator
    }

    pub fn hover_indicator_ms(&self) -> u32 {
        self.hover_indicator_ms
    }

    pub fn hover_indicator_height(&self) -> u32 {
        self.hover_indicator_height
    }

    pub fn urgent_indicator(&self) -> bool {
        self.urgent_indicator
    }

    pub fn urgent_indicator_height(&self) -> u32 {
        self.urgent_indicator_height
    }

    pub fn urgent_indicator_pulse_ms(&self) -> u32 {
        self.urgent_indicator_pulse_ms
    }

    pub fn scroll_windows(&self) -> bool {
        self.scroll_windows
    }

    pub fn scroll_scope(&self) -> ScrollScope {
        self.scroll_scope
    }

    pub fn scroll_wrap(&self) -> bool {
        self.scroll_wrap
    }

    pub fn scroll_reverse(&self) -> bool {
        self.scroll_reverse
    }

    pub fn drag_reorder(&self) -> bool {
        self.drag_reorder
    }

    pub fn column_grouping(&self) -> ColumnGrouping {
        self.column_grouping
    }

    pub fn drag_reorder_slide_ms(&self) -> u32 {
        self.drag_reorder_slide_ms
    }

    /// The colour space the focus pill fades between its usual and hovered colours in.
    pub fn focus_indicator_hover_space(&self) -> Space {
        let Some(space) = self.focus_indicator_hover_in.as_deref() else {
            return Space::default();
        };
        Space::parse(space)
            .inspect_err(|e| tracing::warn!(%e, "ignoring focus_indicator_hover_in"))
            .unwrap_or_default()
    }

    /// The gradient the focus pill cycles through while its button is being dragged, if any.
    pub fn drag_indicator_gradient(&self) -> Option<Gradient> {
        let config = self.drag_indicator_gradient.as_ref()?;
        Gradient::parse(config)
            .inspect_err(|e| tracing::warn!(%e, "ignoring drag_indicator_gradient"))
            .ok()
    }
}

#[derive(Deserialize, Debug)]
struct AppConfig {
    #[serde(rename = "match", deserialize_with = "deserialise_regex")]
    re: Regex,
    class: String,
}

fn deserialise_regex<'de, D>(de: D) -> Result<Regex, D::Error>
where
    D: Deserializer<'de>,
{
    Regex::new(&String::deserialize(de)?).map_err(serde::de::Error::custom)
}

fn default_drag_reorder_slide_ms() -> u32 {
    150
}

fn default_indicator_ms() -> u32 {
    180
}

fn default_hover_indicator_ms() -> u32 {
    150
}

fn default_urgent_pulse_ms() -> u32 {
    1200
}

fn default_indicator_height() -> u32 {
    3
}
