use std::{collections::BTreeMap, fmt::Display, ops::Deref};

use niri_ipc::{Event, Window as NiriWindow, WindowLayout, Workspace};

/// The toplevel window set within Niri, updated via the Niri event stream.
#[derive(Debug)]
pub struct WindowSet(Option<Inner>);

impl WindowSet {
    /// Creates a new window set.
    pub fn new() -> Self {
        Self(None)
    }

    /// Updates the window set based on the given [`niri_ipc::Event`].
    #[tracing::instrument(level = "TRACE", skip(self))]
    pub fn with_event(&mut self, event: Event) -> Option<Snapshot> {
        // This is mildly annoying, because Niri actually has the same state within it and could
        // easily send it on each event, but we have to replicate Niri's own logic and hope we get
        // it right.
        match event {
            Event::WindowsChanged { windows } => match self.0.take() {
                Some(Inner::WorkspacesOnly(workspaces)) => {
                    self.0 = Some(Inner::Ready(Niri::new(windows, workspaces)));
                }
                Some(Inner::WindowsOnly(_)) | None => {
                    self.0 = Some(Inner::WindowsOnly(windows));
                }
                Some(Inner::Ready(mut state)) => {
                    state.replace_windows(windows);
                    self.0 = Some(Inner::Ready(state));
                }
            },
            Event::WorkspacesChanged { workspaces } => match self.0.take() {
                Some(Inner::WindowsOnly(windows)) => {
                    self.0 = Some(Inner::Ready(Niri::new(windows, workspaces)));
                }
                Some(Inner::WorkspacesOnly(_)) | None => {
                    self.0 = Some(Inner::WorkspacesOnly(workspaces));
                }
                Some(Inner::Ready(mut state)) => {
                    state.replace_workspaces(workspaces);
                    self.0 = Some(Inner::Ready(state));
                }
            },
            Event::WindowClosed { id } => {
                if let Some(Inner::Ready(state)) = &mut self.0 {
                    state.remove_window(id);
                } else {
                    tracing::warn!(%self, "unexpected state for WindowClosed event");
                }
            }
            Event::WindowOpenedOrChanged { window } => {
                if let Some(Inner::Ready(state)) = &mut self.0 {
                    state.upsert_window(window);
                } else {
                    tracing::warn!(%self, "unexpected state for WindowOpenedOrChanged event");
                }
            }
            Event::WindowFocusChanged { id } => {
                if let Some(Inner::Ready(state)) = &mut self.0 {
                    state.set_focus(id);
                } else {
                    tracing::warn!(%self, "unexpected state for WindowFocusChanged event");
                }
            }
            Event::WindowLayoutsChanged { changes } => {
                if let Some(Inner::Ready(state)) = &mut self.0 {
                    for (window_id, layout) in changes.into_iter() {
                        state.update_window_layout(window_id, layout);
                    }
                }
            }
            Event::WorkspaceActivated { id, focused } => {
                if let Some(Inner::Ready(state)) = &mut self.0 {
                    state.activate_workspace(id, focused);
                } else {
                    tracing::warn!(%self, "unexpected state for WorkspaceActivated event");
                }
            }
            _ => {}
        }

        if let Some(Inner::Ready(state)) = &self.0 {
            Some(state.snapshot())
        } else {
            None
        }
    }
}

impl Display for WindowSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match &self.0 {
                Some(Inner::Ready(_)) => "ready",
                Some(Inner::WindowsOnly(_)) => "windows only",
                Some(Inner::WorkspacesOnly(_)) => "workspaces only",
                None => "uninitialised",
            }
        )
    }
}

/// The inner state machine as we establish a new event stream.
///
/// Niri guarantees that we will get [`niri_ipc::Event::WindowsChanged`] and
/// [`niri_ipc::Event::WorkspacesChanged`] events at the start of the stream before getting any
/// update events, but not which order they'll come in, so we have to handle that as we build up
/// the window set.
#[derive(Debug)]
enum Inner {
    WindowsOnly(Vec<NiriWindow>),
    WorkspacesOnly(Vec<Workspace>),
    Ready(Niri),
}

/// The Niri state, as best as we can reconstruct it based on the event stream.
#[derive(Debug)]
struct Niri {
    windows: BTreeMap<u64, NiriWindow>,
    workspaces: BTreeMap<u64, Workspace>,
}

impl Niri {
    fn new(windows: Vec<NiriWindow>, workspaces: Vec<Workspace>) -> Self {
        let mut niri = Niri {
            windows: Default::default(),
            workspaces: Default::default(),
        };

        niri.replace_workspaces(workspaces);
        niri.replace_windows(windows);

        niri
    }

    fn activate_workspace(&mut self, id: u64, focused: bool) {
        // This mirrors niri_ipc's own event stream state handling: the activated workspace becomes
        // the active one on its output, and (if focused) the focused workspace overall.
        let Some(output) = self.workspaces.get(&id).map(|ws| ws.output.clone()) else {
            tracing::warn!(id, "activated workspace is unknown");
            return;
        };

        for ws in self.workspaces.values_mut() {
            let activated = ws.id == id;
            if ws.output == output {
                ws.is_active = activated;
            }
            if focused {
                ws.is_focused = activated;
            }
        }
    }

    fn remove_window(&mut self, id: u64) {
        self.windows.remove(&id);
    }

    fn replace_windows(&mut self, windows: Vec<NiriWindow>) {
        self.windows = windows
            .into_iter()
            .map(|window| (window.id, window))
            .collect();
    }

    fn replace_workspaces(&mut self, workspaces: Vec<Workspace>) {
        self.workspaces = workspaces.into_iter().map(|ws| (ws.id, ws)).collect();
    }

    fn set_focus(&mut self, id: Option<u64>) {
        // We have to manually patch up the window is_focused values.
        for window in self.windows.values_mut() {
            window.is_focused = Some(window.id) == id;
        }
    }

    fn update_window_layout(&mut self, window_id: u64, layout: WindowLayout) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.layout = layout;
        } else {
            tracing::warn!(window_id, ?layout, "got window layout for unknown window");
        }
    }

    fn upsert_window(&mut self, window: NiriWindow) {
        // Ensure that we update other windows if the new window is focused.
        if window.is_focused {
            self.windows.values_mut().for_each(|window| {
                window.is_focused = false;
            })
        }

        self.windows.insert(window.id, window);
    }

    /// Create a snapshot of the current window state, ordered by workspace index.
    fn snapshot(&self) -> Snapshot {
        struct WindowWorkspace<'a> {
            window: &'a NiriWindow,
            workspace: &'a Workspace,
        }

        let mut wws: Vec<_> = self
            .windows
            .values()
            .filter_map(|window| {
                if let Some(ws_id) = window.workspace_id {
                    if let Some(workspace) = self.workspaces.get(&ws_id) {
                        return Some(WindowWorkspace { window, workspace });
                    }
                }
                None
            })
            .collect();
        wws.sort_by(|a, b| {
            // Compare by workspace ID first, then window position, then window ID as a last
            // fallback.
            a.workspace
                .idx
                .cmp(&b.workspace.idx)
                .then_with(|| {
                    let a_pos = a.window.layout.pos_in_scrolling_layout.unwrap_or_default();
                    let b_pos = b.window.layout.pos_in_scrolling_layout.unwrap_or_default();

                    // Compare by column index, then tile index within the column.
                    a_pos.0.cmp(&b_pos.0).then_with(|| a_pos.1.cmp(&b_pos.1))
                })
                .then_with(|| a.window.id.cmp(&b.window.id))
        });

        let windows = wws
            .into_iter()
            .map(|ww| Window {
                window: ww.window.clone(),
                workspace: WorkspaceInfo::from(ww.workspace),
            })
            .collect();

        let mut workspaces: Vec<_> = self.workspaces.values().map(WorkspaceInfo::from).collect();
        workspaces.sort_by_key(|ws| (ws.idx, ws.id));

        Snapshot {
            windows,
            workspaces,
        }
    }
}

/// A snapshot of the current toplevel windows (ordered by workspace index) and workspaces.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub windows: Vec<Window>,
    pub workspaces: Vec<WorkspaceInfo>,
}

/// The parts of a Niri workspace that the taskbar cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub idx: u8,
    pub output: Option<String>,
    /// Whether this is the active workspace on its output.
    pub is_active: bool,
    /// Whether this is the focused workspace overall.
    pub is_focused: bool,
}

impl From<&Workspace> for WorkspaceInfo {
    fn from(ws: &Workspace) -> Self {
        Self {
            id: ws.id,
            idx: ws.idx,
            output: ws.output.clone(),
            is_active: ws.is_active,
            is_focused: ws.is_focused,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Window {
    window: NiriWindow,
    workspace: WorkspaceInfo,
}

impl Window {
    pub fn output(&self) -> Option<&str> {
        self.workspace.output.as_deref()
    }

    /// The workspace the window is on.
    pub fn workspace(&self) -> &WorkspaceInfo {
        &self.workspace
    }
}

impl Deref for Window {
    type Target = NiriWindow;

    fn deref(&self) -> &Self::Target {
        &self.window
    }
}
