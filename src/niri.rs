use std::collections::HashMap;

use niri_ipc::{Action, Output, Reply, Request, WorkspaceReferenceArg, socket::Socket};
pub use state::{Snapshot, Window};
pub use window_stream::{Item as WindowStreamItem, WindowStream};

use crate::error::Error;

mod reply;
mod state;
mod window_stream;

/// The top level client for Niri.
#[derive(Debug, Clone, Copy)]
pub struct Niri {}

impl Niri {
    pub fn new() -> Self {
        // Since niri_ipc is essentially stateless, we don't maintain anything much here.
        Self {}
    }

    /// Requests that the given window ID should be activated.
    #[tracing::instrument(level = "TRACE", err)]
    pub fn activate_window(&self, id: u64) -> Result<(), Error> {
        let reply = request(Request::Action(Action::FocusWindow { id }))?;
        reply::typed!(Handled, reply)
    }

    /// Requests that the focused column be moved to the given 1-based index on its workspace.
    #[tracing::instrument(level = "TRACE", err)]
    pub fn move_column_to_index(&self, index: usize) -> Result<(), Error> {
        let reply = request(Request::Action(Action::MoveColumnToIndex { index }))?;
        reply::typed!(Handled, reply)
    }

    /// Requests that the given workspace ID should be focused.
    #[tracing::instrument(level = "TRACE", err)]
    pub fn focus_workspace(&self, id: u64) -> Result<(), Error> {
        let reply = request(Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(id),
        }))?;
        reply::typed!(Handled, reply)
    }

    /// Performs an action that doesn't reply with anything but whether it was handled.
    #[tracing::instrument(level = "TRACE", err)]
    pub fn action(&self, action: Action) -> Result<(), Error> {
        let reply = request(Request::Action(action))?;
        reply::typed!(Handled, reply)
    }

    /// Returns the current windows.
    pub fn windows(&self) -> Result<Vec<niri_ipc::Window>, Error> {
        let reply = request(Request::Windows)?;
        reply::typed!(Windows, reply)
    }

    /// Returns the current workspaces.
    pub fn workspaces(&self) -> Result<Vec<niri_ipc::Workspace>, Error> {
        let reply = request(Request::Workspaces)?;
        reply::typed!(Workspaces, reply)
    }

    /// Returns the current outputs.
    pub fn outputs(&self) -> Result<HashMap<String, Output>, Error> {
        let reply = request(Request::Outputs)?;
        reply::typed!(Outputs, reply)
    }

    /// Returns a stream of window snapshots.
    pub fn window_stream(&self) -> WindowStream {
        WindowStream::new()
    }
}

// Helper to marshal request errors into our own type system.
//
// This can't be used for event streams, since the stream callback is thrown away in this function.
#[tracing::instrument(level = "TRACE", err)]
fn request(request: Request) -> Result<Reply, Error> {
    socket()?.send(request).map_err(Error::NiriIpc)
}

// Helper to connect to the Niri socket.
#[tracing::instrument(level = "TRACE", err)]
fn socket() -> Result<Socket, Error> {
    Socket::connect().map_err(Error::NiriIpc)
}
