use std::sync::Arc;

use async_channel::{Receiver, Sender};
use futures::StreamExt;
use waybar_cffi::gtk::glib;

use crate::{
    config::Config,
    icon,
    niri::{Niri, Snapshot, WindowStream, WindowStreamItem},
    notify::{self, EnrichedNotification},
};

/// Global state for the taskbar.
#[derive(Debug, Clone)]
pub struct State(Arc<Inner>);

impl State {
    /// Instantiates the global state.
    pub fn new(config: Config) -> Self {
        Self(Arc::new(Inner {
            config,
            icon_cache: icon::Cache::default(),
            niri: Niri::new(),
        }))
    }

    /// Returns the taskbar configuration.
    pub fn config(&self) -> &Config {
        &self.0.config
    }

    /// Accesses the global icon cache.
    pub fn icon_cache(&self) -> &icon::Cache {
        &self.0.icon_cache
    }

    /// Accesses the global [`Niri`] instance.
    pub fn niri(&self) -> &Niri {
        &self.0.niri
    }

    /// Starts the event sources for a taskbar instance.
    ///
    /// The returned sender can be used to inject additional events (for example, from Gtk signal
    /// handlers); the receiver yields all events in order. Dropping the receiver stops all of the
    /// event sources.
    pub fn event_stream(&self) -> (Sender<Event>, Receiver<Event>) {
        let (tx, rx) = async_channel::unbounded();

        if self.config().notifications_enabled() {
            glib::spawn_future_local(notify_stream(tx.clone()));
        }

        glib::spawn_future_local(window_stream(tx.clone(), self.niri().window_stream()));

        (tx, rx)
    }
}

#[derive(Debug)]
struct Inner {
    config: Config,
    icon_cache: icon::Cache,
    niri: Niri,
}

#[derive(Debug)]
pub enum Event {
    Notification(Box<EnrichedNotification>),
    WindowSnapshot(Snapshot),
    /// The set of outputs, or the mapping of the taskbar onto an output, may have changed, and
    /// the output filter should be re-evaluated.
    OutputsChanged,
}

async fn notify_stream(tx: Sender<Event>) {
    let mut stream = Box::pin(notify::stream());

    while let Some(notification) = stream.next().await {
        if tx
            .send(Event::Notification(Box::new(notification)))
            .await
            .is_err()
        {
            tracing::debug!("notification receiver dropped; stopping notification stream");
            return;
        }
    }
}

async fn window_stream(tx: Sender<Event>, window_stream: WindowStream) {
    while let Some(item) = window_stream.next().await {
        let event = match item {
            WindowStreamItem::WorkspacesChanged => Event::OutputsChanged,
            WindowStreamItem::Snapshot(snapshot) => Event::WindowSnapshot(snapshot),
        };

        if tx.send(event).await.is_err() {
            tracing::debug!("window snapshot receiver dropped; stopping window stream");
            return;
        }
    }
}
