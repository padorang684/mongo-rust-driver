#[cfg(test)]
pub(crate) mod test;

pub(crate) mod conn;
mod connection_requester;
pub(crate) mod establish;
mod manager;
pub(crate) mod options;
mod status;
mod worker;

use std::sync::Arc;

use derivative::Derivative;
#[cfg(test)]
use tokio::sync::oneshot;

pub use self::conn::ConnectionInfo;
pub(crate) use self::{
    conn::{Command, Connection, RawCommand, RawCommandResponse, StreamDescription},
    status::PoolGenerationSubscriber,
    worker::PoolGeneration,
};
use self::{
    connection_requester::ConnectionRequestResult,
    establish::ConnectionEstablisher,
    options::ConnectionPoolOptions,
};
use crate::{
    bson::oid::ObjectId,
    error::{Error, Result},
    event::cmap::{
        CmapEventHandler,
        ConnectionCheckoutFailedEvent,
        ConnectionCheckoutFailedReason,
        ConnectionCheckoutStartedEvent,
        PoolCreatedEvent,
    },
    options::ServerAddress,
    sdam::TopologyUpdater,
};
use connection_requester::ConnectionRequester;
use manager::PoolManager;
use worker::ConnectionPoolWorker;

#[cfg(test)]
use crate::runtime::WorkerHandle;

const DEFAULT_MAX_POOL_SIZE: u32 = 10;

/// A pool of connections implementing the CMAP spec.
/// This type is actually a handle to task that manages the connections and is cheap to clone and
/// pass around.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub(crate) struct ConnectionPool {
    address: ServerAddress,
    manager: PoolManager,
    connection_requester: ConnectionRequester,
    generation_subscriber: PoolGenerationSubscriber,

    #[derivative(Debug = "ignore")]
    event_handler: Option<Arc<dyn CmapEventHandler>>,
}

impl ConnectionPool {
    pub(crate) fn new(
        address: ServerAddress,
        connection_establisher: ConnectionEstablisher,
        server_updater: TopologyUpdater,
        options: Option<ConnectionPoolOptions>,
    ) -> Self {
        let (manager, connection_requester, generation_subscriber) = ConnectionPoolWorker::start(
            address.clone(),
            connection_establisher,
            server_updater,
            options.clone(),
        );

        let event_handler = options
            .as_ref()
            .and_then(|opts| opts.cmap_event_handler.clone());

        if let Some(ref handler) = event_handler {
            handler.handle_pool_created_event(PoolCreatedEvent {
                address: address.clone(),
                options: options.map(|o| o.to_event_options()),
            });
        };

        Self {
            address,
            manager,
            connection_requester,
            generation_subscriber,
            event_handler,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_mocked(address: ServerAddress) -> Self {
        let (manager, _) = manager::channel();
        let handle = WorkerHandle::new_mocked();
        let (connection_requester, _) = connection_requester::channel(handle);
        let (_, generation_subscriber) = status::channel(PoolGeneration::normal());

        Self {
            address,
            manager,
            connection_requester,
            generation_subscriber,
            event_handler: None,
        }
    }

    fn emit_event<F>(&self, emit: F)
    where
        F: FnOnce(&Arc<dyn CmapEventHandler>),
    {
        if let Some(ref handler) = self.event_handler {
            emit(handler);
        }
    }

    /// Checks out a connection from the pool. This method will yield until this thread is at the
    /// front of the wait queue, and then will block again if no available connections are in the
    /// pool and the total number of connections is not less than the max pool size.
    pub(crate) async fn check_out(&self) -> Result<Connection> {
        self.emit_event(|handler| {
            let event = ConnectionCheckoutStartedEvent {
                address: self.address.clone(),
            };

            handler.handle_connection_checkout_started_event(event);
        });

        let response = self.connection_requester.request().await;

        let conn = match response {
            ConnectionRequestResult::Pooled(c) => Ok(*c),
            ConnectionRequestResult::Establishing(task) => task.await,
            ConnectionRequestResult::PoolCleared(e) => {
                Err(Error::pool_cleared_error(&self.address, &e))
            }
        };

        match conn {
            Ok(ref conn) => {
                self.emit_event(|handler| {
                    handler.handle_connection_checked_out_event(conn.checked_out_event());
                });
            }
            Err(_) => {
                self.emit_event(|handler| {
                    handler.handle_connection_checkout_failed_event(ConnectionCheckoutFailedEvent {
                        address: self.address.clone(),
                        reason: ConnectionCheckoutFailedReason::ConnectionError,
                    })
                });
            }
        }

        conn
    }

    /// Increments the generation of the pool. Rather than eagerly removing stale connections from
    /// the pool, they are left for the background thread to clean up.
    pub(crate) async fn clear(&self, cause: Error, service_id: Option<ObjectId>) {
        self.manager.clear(cause, service_id).await
    }

    /// Mark the pool as "ready" as per the CMAP specification.
    pub(crate) async fn mark_as_ready(&self) {
        self.manager.mark_as_ready().await
    }

    pub(crate) fn generation(&self) -> PoolGeneration {
        self.generation_subscriber.generation()
    }

    #[cfg(test)]
    pub(crate) fn sync_worker(&self) -> oneshot::Receiver<()> {
        self.manager.sync_worker()
    }
}
