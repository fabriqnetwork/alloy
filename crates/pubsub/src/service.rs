use crate::{
    handle::ConnectionHandle,
    ix::PubSubInstruction,
    managers::{InFlight, RequestManager, SubscriptionManager},
    PubSubConnect, PubSubFrontend,
};

use alloy_json_rpc::{Id, PubSubItem, Request, RequestMeta, Response, ResponsePayload};
use alloy_primitives::U256;
use alloy_transport::{
    utils::{to_json_raw_value, Spawnable},
    TransportError, TransportErrorKind, TransportResult,
};
use serde_json::value::RawValue;
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug)]
/// The service contains the backend handle, a subscription manager, and the
/// configuration details required to reconnect.
pub(crate) struct PubSubService<T> {
    /// The backend handle.
    pub(crate) handle: ConnectionHandle,

    /// The configuration details required to reconnect.
    pub(crate) connector: T,

    /// The inbound requests.
    pub(crate) reqs: mpsc::UnboundedReceiver<PubSubInstruction>,

    /// The subscription manager.
    pub(crate) subs: SubscriptionManager,

    /// The request manager.
    pub(crate) in_flights: RequestManager,
}

impl<T> PubSubService<T>
where
    T: PubSubConnect,
{
    /// Create a new service from a connector.
    pub(crate) async fn connect(connector: T) -> Result<PubSubFrontend, TransportError> {
        let handle = connector.connect().await?;

        let (tx, reqs) = mpsc::unbounded_channel();
        let this = Self {
            handle,
            connector,
            reqs,
            subs: Default::default(),
            in_flights: Default::default(),
        };
        this.spawn();
        Ok(PubSubFrontend::new(tx))
    }

    /// Reconnect by dropping the backend and creating a new one.
    async fn get_new_backend(&mut self) -> Result<ConnectionHandle, TransportError> {
        let mut handle = self.connector.try_reconnect().await?;
        std::mem::swap(&mut self.handle, &mut handle);
        Ok(handle)
    }

    /// Reconnect the backend, re-issue pending requests, and re-start active
    /// subscriptions.
    async fn reconnect(&mut self) -> TransportResult<()> {
        info!("Reconnecting pubsub service backend.");

        let mut old_handle = self.get_new_backend().await?;

        debug!("Draining old backend to_handle");

        // Drain the old backend
        while let Ok(item) = old_handle.from_socket.try_recv() {
            self.handle_item(item)?;
        }

        old_handle.shutdown();

        // Re-issue pending requests.
        debug!(count = self.in_flights.len(), "Reissuing pending requests");
        self.in_flights
            .iter()
            .map(|(_, in_flight)| in_flight.request().serialized().to_owned())
            .collect::<Vec<_>>()
            .into_iter()
            .try_for_each(|brv| self.dispatch_request(brv))?;

        // Re-subscribe to all active subscriptions
        debug!(count = self.subs.len(), "Re-starting active subscriptions");

        // Drop all server IDs. We'll re-insert them as we get responses.
        self.subs.drop_server_ids();

        // Dispatch all subscription requests
        self.subs.iter().try_for_each(|(_, sub)| {
            let req = sub.request().to_owned();
            let (in_flight, _) = InFlight::new(req.clone());
            self.in_flights.insert(in_flight);

            self.handle
                .to_socket
                .send(req.serialized().to_owned())
                .map(drop)
                .map_err(|_| TransportErrorKind::backend_gone())
        })?;

        Ok(())
    }

    /// Dispatch a request to the socket.
    fn dispatch_request(&mut self, brv: Box<RawValue>) -> TransportResult<()> {
        self.handle.to_socket.send(brv).map(drop).map_err(|_| TransportErrorKind::backend_gone())
    }

    /// Service a request.
    fn service_request(&mut self, in_flight: InFlight) -> TransportResult<()> {
        let brv = in_flight.request();

        self.dispatch_request(brv.serialized().to_owned())?;
        self.in_flights.insert(in_flight);

        Ok(())
    }

    /// Service a GetSub instruction.
    ///
    /// If the subscription exists, the waiter is sent a broadcast receiver. If
    /// the subscription does not exist, the waiter is sent nothing, and the
    /// `tx` is dropped. This notifies the waiter that the subscription does
    /// not exist.
    fn service_get_sub(
        &mut self,
        local_id: U256,
        tx: oneshot::Sender<broadcast::Receiver<Box<RawValue>>>,
    ) -> TransportResult<()> {
        let local_id = local_id.into();

        if let Some(rx) = self.subs.get_rx(local_id) {
            let _ = tx.send(rx);
        }

        Ok(())
    }

    /// Service an unsubscribe instruction.
    fn service_unsubscribe(&mut self, local_id: U256) -> TransportResult<()> {
        let local_id = local_id.into();
        let req = Request {
            meta: RequestMeta { id: Id::None, method: "eth_unsubscribe" },
            params: [local_id],
        };
        let brv = req.serialize().expect("no ser error").take_request();

        self.dispatch_request(brv)?;
        self.subs.remove_sub(local_id);
        Ok(())
    }

    /// Service an instruction
    fn service_ix(&mut self, ix: PubSubInstruction) -> TransportResult<()> {
        trace!(?ix, "servicing instruction");
        match ix {
            PubSubInstruction::Request(in_flight) => self.service_request(in_flight),
            PubSubInstruction::GetSub(alias, tx) => self.service_get_sub(alias, tx),
            PubSubInstruction::Unsubscribe(alias) => self.service_unsubscribe(alias),
        }
    }

    /// Handle an item from the backend.
    fn handle_item(&mut self, item: PubSubItem) -> TransportResult<()> {
        match item {
            PubSubItem::Response(resp) => match self.in_flights.handle_response(resp) {
                Some((server_id, in_flight)) => self.handle_sub_response(in_flight, server_id),
                None => Ok(()),
            },
            PubSubItem::Notification(notification) => {
                self.subs.notify(notification);
                Ok(())
            }
        }
    }

    /// Rewrite the subscription id and insert into the subscriptions manager
    fn handle_sub_response(&mut self, in_flight: InFlight, server_id: U256) -> TransportResult<()> {
        let request = in_flight.request;
        let id = request.id().clone();

        self.subs.upsert(request, server_id);

        // lie to the client about the sub id.
        let local_id = self.subs.local_id_for(server_id).unwrap();
        // Serialized B256 is always a valid serialized U256 too.
        let ser_alias = to_json_raw_value(&local_id)?;

        // We send back a success response with the new subscription ID.
        // We don't care if the channel is dead.
        let _ =
            in_flight.tx.send(Ok(Response { id, payload: ResponsePayload::Success(ser_alias) }));

        Ok(())
    }

    /// Spawn the service.
    pub(crate) fn spawn(mut self) {
        let fut = async move {
            let result: TransportResult<()> = loop {
                // We bias the loop so that we always handle new messages before
                // reconnecting, and always reconnect before dispatching new
                // requests.
                tokio::select! {
                    biased;

                    item_opt = self.handle.from_socket.recv() => {
                        if let Some(item) = item_opt {
                            if let Err(e) = self.handle_item(item) {
                                break Err(e)
                            }
                        } else if let Err(e) = self.reconnect().await {
                            break Err(e)
                        }
                    }

                    _ = &mut self.handle.error => {
                        error!("Pubsub service backend error.");
                        if let Err(e) = self.reconnect().await {
                            break Err(e)
                        }
                    }

                    req_opt = self.reqs.recv() => {
                        if let Some(req) = req_opt {
                            if let Err(e) = self.service_ix(req) {
                                break Err(e)
                            }
                        } else {
                            info!("Pubsub service request channel closed. Shutting down.");
                           break Ok(())
                        }
                    }
                }
            };

            if let Err(err) = result {
                error!(%err, "pubsub service reconnection error");
            }
        };
        fut.spawn_task();
    }
}