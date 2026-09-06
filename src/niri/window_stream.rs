use std::{thread, time::Duration};

use async_channel::{Receiver, Sender};
use niri_ipc::{Event, Request};

use crate::error::Error;

use super::{
    reply, socket,
    state::{Snapshot, WindowSet},
};

/// An item yielded by the [`WindowStream`].
#[derive(Debug)]
pub enum Item {
    /// The workspace set changed, which means the set of outputs (or which workspaces live on
    /// which outputs) may also have changed. A [`Item::Snapshot`] follows whenever the window
    /// state is ready.
    ///
    /// This is also sent whenever the stream (re)connects to Niri, since anything may have
    /// changed while we were disconnected.
    WorkspacesChanged,

    /// A new snapshot of the window set.
    Snapshot(Snapshot),
}

/// A stream that receives events from Niri and produces window [`Snapshot`]s, along with hints
/// when the workspace/output configuration changes.
///
/// The underlying connection to Niri is re-established automatically if it fails.
pub struct WindowStream {
    rx: Receiver<Item>,
}

impl WindowStream {
    pub(super) fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        thread::spawn(move || window_stream_loop(tx));

        Self { rx }
    }

    /// Awaits the next [`Item`].
    pub async fn next(&self) -> Option<Item> {
        self.rx.recv().await.ok()
    }
}

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Runs the event stream until the receiving side goes away, reconnecting with backoff whenever
/// the connection to Niri fails.
fn window_stream_loop(tx: Sender<Item>) {
    let mut backoff = MIN_BACKOFF;

    loop {
        match window_stream(&tx, &mut backoff) {
            Error::WindowStreamSend => {
                tracing::debug!("window stream receiver dropped; stopping");
                return;
            }
            e => {
                tracing::error!(%e, ?backoff, "Niri taskbar window stream error; reconnecting");
            }
        }

        thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Runs a single event stream connection. Only ever returns with the error that ended it.
fn window_stream(tx: &Sender<Item>, backoff: &mut Duration) -> Error {
    let mut socket = match socket() {
        Ok(socket) => socket,
        Err(e) => return e,
    };
    let reply = match socket.send(Request::EventStream) {
        Ok(reply) => reply,
        Err(e) => return Error::NiriIpc(e),
    };
    if let Err(e) = reply::typed!(Handled, reply) {
        return e;
    }

    // We have a working connection, so any subsequent failure starts backing off from scratch.
    *backoff = MIN_BACKOFF;

    let mut next = socket.read_events();
    let mut state = WindowSet::new();

    // Anything may have changed while we weren't connected.
    if tx.send_blocking(Item::WorkspacesChanged).is_err() {
        return Error::WindowStreamSend;
    }

    loop {
        // There appears to be no EOF state, presumably on the assumption that if Niri goes away it
        // doesn't matter what happens to this process. (In practice, EOF surfaces as an
        // UnexpectedEof error from the JSON parser, which we handle below by reconnecting.)
        match next() {
            Ok(event) => {
                let workspaces_changed = matches!(event, Event::WorkspacesChanged { .. });
                let snapshot = state.with_event(event);

                // Send the hint before the snapshot so the receiver can update its view of the
                // outputs before applying the new window set.
                if workspaces_changed && tx.send_blocking(Item::WorkspacesChanged).is_err() {
                    return Error::WindowStreamSend;
                }
                if let Some(snapshot) = snapshot {
                    if tx.send_blocking(Item::Snapshot(snapshot)).is_err() {
                        return Error::WindowStreamSend;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::warn!(%e, "Niri IPC: skipping unknown event");
            }
            Err(e) => {
                tracing::error!(%e, "Niri IPC error reading from event stream");
                return Error::NiriIpc(e);
            }
        }
    }
}
