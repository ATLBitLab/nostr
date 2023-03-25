// Copyright (c) 2022-2023 Yuki Kishimoto
// Distributed under the MIT software license

//! Relay Pool

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use nostr::url::Url;
use nostr::{ClientMessage, Event, EventId, Filter, RelayMessage};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{broadcast, Mutex};
use wasm_bindgen_futures::spawn_local;

use super::{Error as RelayError, Relay, RelayOptions, RelayPoolMessage, RelayPoolNotification};

/// [`RelayPool`] error
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Relay error
    #[error(transparent)]
    Relay(#[from] RelayError),
    /// No relay connected
    #[error("no relay connected")]
    NoRelayConnected,
    /// Relay not found
    #[error("relay not found")]
    RelayNotFound,
}

struct RelayPoolTask {
    receiver: Receiver<RelayPoolMessage>,
    notification_sender: broadcast::Sender<RelayPoolNotification>,
    events: VecDeque<EventId>,
}

const MAX_EVENTS: usize = 100000;

impl RelayPoolTask {
    pub fn new(
        pool_task_receiver: Receiver<RelayPoolMessage>,
        notification_sender: broadcast::Sender<RelayPoolNotification>,
    ) -> Self {
        Self {
            receiver: pool_task_receiver,
            events: VecDeque::new(),
            notification_sender,
        }
    }

    pub async fn run(&mut self) {
        log::debug!("RelayPoolTask Thread Started");
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                RelayPoolMessage::ReceivedMsg { relay_url, msg } => {
                    let _ = self
                        .notification_sender
                        .send(RelayPoolNotification::Message(
                            relay_url.clone(),
                            msg.clone(),
                        ));

                    if let RelayMessage::Event { event, .. } = msg {
                        // Verifies if the event is valid
                        if event.verify().is_ok() {
                            // Adds only new events
                            if !self.events.contains(&event.id) {
                                self.add_event(event.id);
                                let notification =
                                    RelayPoolNotification::Event(relay_url, event.as_ref().clone());
                                let _ = self.notification_sender.send(notification);
                            }
                        }
                    }
                }
                RelayPoolMessage::EventSent(event) => {
                    self.add_event(event.id);
                }
                RelayPoolMessage::Shutdown => {
                    if let Err(e) = self
                        .notification_sender
                        .send(RelayPoolNotification::Shutdown)
                    {
                        log::error!("Impossible to send shutdown notification: {}", e);
                    }
                    log::debug!("Exited from RelayPoolTask thread");
                    self.receiver.close();
                    break;
                }
            }
        }
    }

    fn add_event(&mut self, event_id: EventId) {
        while self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event_id);
    }
}

/// Relay Pool
#[derive(Debug, Clone)]
pub struct RelayPool {
    relays: Arc<Mutex<HashMap<Url, Relay>>>,
    pool_task_sender: Sender<RelayPoolMessage>,
    notification_sender: broadcast::Sender<RelayPoolNotification>,
    filters: Arc<Mutex<Vec<Filter>>>,
}

impl Default for RelayPool {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayPool {
    /// Create new `RelayPool`
    pub fn new() -> Self {
        let (notification_sender, _) = broadcast::channel(1024);
        let (pool_task_sender, pool_task_receiver) = mpsc::channel(1024);

        let mut relay_pool_task =
            RelayPoolTask::new(pool_task_receiver, notification_sender.clone());

        spawn_local(async move { relay_pool_task.run().await });

        Self {
            relays: Arc::new(Mutex::new(HashMap::new())),
            pool_task_sender,
            notification_sender,
            filters: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get new notification listener
    pub fn notifications(&self) -> broadcast::Receiver<RelayPoolNotification> {
        self.notification_sender.subscribe()
    }

    /// Get relays
    pub async fn relays(&self) -> HashMap<Url, Relay> {
        let relays = self.relays.lock().await;
        relays.clone()
    }

    /// Get subscription filters
    pub async fn subscription_filters(&self) -> Vec<Filter> {
        self.filters.lock().await.clone()
    }

    /// Update subscription filters
    async fn update_subscription_filters(&self, filters: Vec<Filter>) {
        let mut f = self.filters.lock().await;
        *f = filters;
    }

    /// Add new relay
    pub async fn add_relay(&self, url: Url, opts: RelayOptions) -> Result<(), Error> {
        let mut relays = self.relays.lock().await;
        if !relays.contains_key(&url) {
            let relay = Relay::new(
                url,
                self.pool_task_sender.clone(),
                self.notification_sender.clone(),
                opts,
            );
            relays.insert(relay.url(), relay);
        }
        Ok(())
    }

    /// Disconnect and remove relay
    pub async fn remove_relay(&self, url: Url) -> Result<(), Error> {
        let mut relays = self.relays.lock().await;
        if let Some(relay) = relays.remove(&url) {
            self.disconnect_relay(&relay).await?;
        }
        Ok(())
    }

    /// Send client message
    pub async fn send_msg(&self, msg: ClientMessage) -> Result<(), Error> {
        let relays = self.relays().await;

        if relays.is_empty() {
            return Err(Error::NoRelayConnected);
        }

        if let ClientMessage::Event(event) = &msg {
            if let Err(e) = self
                .pool_task_sender
                .send(RelayPoolMessage::EventSent(event.clone()))
                .await
            {
                log::error!("{e}");
            };
        }

        for (url, relay) in relays.into_iter() {
            if let Err(e) = relay.send_msg(msg.clone()).await {
                log::error!("Impossible to send msg to {url}: {e}");
            }
        }

        Ok(())
    }

    /// Send client message
    pub async fn send_msg_to(&self, url: Url, msg: ClientMessage) -> Result<(), Error> {
        let relays = self.relays().await;
        if let Some(relay) = relays.get(&url) {
            relay.send_msg(msg).await?;
            Ok(())
        } else {
            Err(Error::RelayNotFound)
        }
    }

    /// Subscribe to filters
    pub async fn subscribe(&self, filters: Vec<Filter>) {
        let relays = self.relays().await;
        self.update_subscription_filters(filters.clone()).await;
        for relay in relays.values() {
            if let Err(e) = relay.subscribe(filters.clone()).await {
                log::error!("{e}");
            }
        }
    }

    /// Unsubscribe from filters
    pub async fn unsubscribe(&self) {
        let relays = self.relays().await;
        for relay in relays.values() {
            if let Err(e) = relay.unsubscribe().await {
                log::error!("{e}");
            }
        }
    }

    /// Get events of filters
    pub async fn get_events_of(
        &self,
        filters: Vec<Filter>,
        timeout: Option<Duration>,
    ) -> Result<Vec<Event>, Error> {
        let mut events: Vec<Event> = Vec::new();
        let relays = self.relays().await;
        for (url, relay) in relays.into_iter() {
            if let Err(e) = relay
                .get_events_of_with_callback(filters.clone(), timeout, |event| {
                    if !events.contains(&event) {
                        events.push(event);
                    }
                })
                .await
            {
                log::error!("Failed to get events from {url}: {e}");
            }
        }

        Ok(events)
    }

    /// Request events of filter. All events will be sent to notification listener
    pub async fn req_events_of(&self, filters: Vec<Filter>, timeout: Option<Duration>) {
        let relays = self.relays().await;
        for relay in relays.values() {
            relay.req_events_of(filters.clone(), timeout);
        }
    }

    /// Connect to all added relays and keep connection alive
    pub async fn connect(&self) {
        let relays = self.relays().await;
        for relay in relays.values() {
            self.connect_relay(relay).await;
        }
    }

    /// Disconnect from all relays
    pub async fn disconnect(&self) -> Result<(), Error> {
        let relays = self.relays().await;
        for relay in relays.values() {
            self.disconnect_relay(relay).await?;
        }

        Ok(())
    }

    /// Connect to relay
    pub async fn connect_relay(&self, relay: &Relay) {
        let filters: Vec<Filter> = self.subscription_filters().await;
        relay.update_subscription_filters(filters).await;
        relay.connect().await;
    }

    /// Disconnect from relay
    pub async fn disconnect_relay(&self, relay: &Relay) -> Result<(), Error> {
        relay.terminate().await?;
        Ok(())
    }

    /// Completly shutdown pool
    pub async fn shutdown(self) -> Result<(), Error> {
        self.disconnect().await?;
        if let Err(e) = self.pool_task_sender.send(RelayPoolMessage::Shutdown).await {
            log::error!("Impossible to shutdown pool: {e}");
        };
        Ok(())
    }
}
