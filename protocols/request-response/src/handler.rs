// Copyright 2020 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

pub(crate) mod protocol;

pub use protocol::ProtocolSupport;

use crate::codec::Codec;
use crate::handler::protocol::Protocol;
use crate::{RequestId, EMPTY_QUEUE_SHRINK_THRESHOLD};

use futures::{channel::oneshot, future::BoxFuture, prelude::*, stream::FuturesUnordered};
use instant::Instant;
use libp2p_swarm::handler::{
    ConnectionEvent, DialUpgradeError, FullyNegotiatedInbound, FullyNegotiatedOutbound,
    ListenUpgradeError,
};
use libp2p_swarm::{
    handler::{ConnectionHandler, ConnectionHandlerEvent, KeepAlive, StreamUpgradeError},
    SubstreamProtocol,
};
use smallvec::SmallVec;
use std::pin::pin;
use std::{
    collections::VecDeque,
    fmt, io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

/// A connection handler for a request response [`Behaviour`](super::Behaviour) protocol.
pub struct Handler<TCodec>
where
    TCodec: Codec,
{
    /// The supported inbound protocols.
    inbound_protocols: SmallVec<[TCodec::Protocol; 2]>,
    /// The request/response message codec.
    codec: TCodec,
    /// The keep-alive timeout of idle connections. A connection is considered
    /// idle if there are no outbound substreams.
    keep_alive_timeout: Duration,
    /// The timeout for inbound and outbound substreams (i.e. request
    /// and response processing).
    substream_timeout: Duration,
    /// The current connection keep-alive.
    keep_alive: KeepAlive,
    /// Queue of events to emit in `poll()`.
    pending_events: VecDeque<Event<TCodec>>,
    /// Outbound upgrades waiting to be emitted as an `OutboundSubstreamRequest`.
    pending_outbound: VecDeque<OutboundMessage<TCodec>>,

    requested_outbound: VecDeque<OutboundMessage<TCodec>>,
    /// Inbound upgrades waiting for the incoming request.
    inbound: FuturesUnordered<
        BoxFuture<
            'static,
            Result<
                (
                    (RequestId, TCodec::Request),
                    oneshot::Sender<TCodec::Response>,
                ),
                oneshot::Canceled,
            >,
        >,
    >,
    inbound_request_id: Arc<AtomicU64>,

    worker_streams: FuturesUnordered<BoxFuture<'static, Result<Event<TCodec>, io::Error>>>,
}

impl<TCodec> Handler<TCodec>
where
    TCodec: Codec + Send + Clone + 'static,
{
    pub(super) fn new(
        inbound_protocols: SmallVec<[TCodec::Protocol; 2]>,
        codec: TCodec,
        keep_alive_timeout: Duration,
        substream_timeout: Duration,
        inbound_request_id: Arc<AtomicU64>,
    ) -> Self {
        Self {
            inbound_protocols,
            codec,
            keep_alive: KeepAlive::Yes,
            keep_alive_timeout,
            substream_timeout,
            pending_outbound: VecDeque::new(),
            requested_outbound: Default::default(),
            inbound: FuturesUnordered::new(),
            pending_events: VecDeque::new(),
            inbound_request_id,
            worker_streams: Default::default(),
        }
    }

    fn on_fully_negotiated_inbound(
        &mut self,
        FullyNegotiatedInbound {
            protocol: (mut stream, protocol),
            info: (),
        }: FullyNegotiatedInbound<
            <Self as ConnectionHandler>::InboundProtocol,
            <Self as ConnectionHandler>::InboundOpenInfo,
        >,
    ) {
        let mut codec = self.codec.clone();

        // A channel for notifying the handler when the inbound
        // upgrade received the request.
        let (rq_send, rq_recv) = oneshot::channel();

        // A channel for notifying the inbound upgrade when the
        // response is sent.
        let (rs_send, rs_recv) = oneshot::channel();

        // The handler waits for the request to come in. It then emits
        // `Event::Request` together with a
        // `ResponseChannel`.
        self.inbound
            .push(rq_recv.map_ok(move |rq| (rq, rs_send)).boxed());

        let request_id = RequestId(self.inbound_request_id.fetch_add(1, Ordering::Relaxed));
        let timeout = self.substream_timeout;

        let recv = async move {
            let read = codec.read_request(&protocol, &mut stream);
            let request = read.await?;
            match rq_send.send((request_id, request)) {
                Ok(()) => {}
                Err(_) => {
                    panic!("Expect request receiver to be alive i.e. protocol handler to be alive.",)
                }
            }

            if let Ok(response) = rs_recv.await {
                let write = codec.write_response(&protocol, &mut stream, response);
                write.await?;

                stream.close().await?;
                // Response was sent. Indicate to handler to emit a `ResponseSent` event.
                Ok(Event::ResponseSent(request_id))
            } else {
                stream.close().await?;
                // No response was sent. Indicate to handler to emit a `ResponseOmission` event.
                Ok(Event::ResponseOmission(request_id))
            }
        };

        self.worker_streams.push(Box::pin(async move {
            match future::select(pin!(recv), futures_timer::Delay::new(timeout)).await {
                future::Either::Left((recv, _)) => recv,
                future::Either::Right(((), _)) => Err(io::ErrorKind::TimedOut.into()),
            }
        }));
    }

    fn on_fully_negotiated_outbound(
        &mut self,
        FullyNegotiatedOutbound {
            protocol: (mut stream, protocol),
            info: (),
        }: FullyNegotiatedOutbound<
            <Self as ConnectionHandler>::OutboundProtocol,
            <Self as ConnectionHandler>::OutboundOpenInfo,
        >,
    ) {
        let message = self
            .requested_outbound
            .pop_front()
            .expect("negotiated a stream without a pending message");

        let mut codec = self.codec.clone();
        let timeout = self.substream_timeout;
        let request_id = message.request_id;

        let send = async move {
            let write = codec.write_request(&protocol, &mut stream, message.request);
            write.await?;
            stream.close().await?;
            let read = codec.read_response(&protocol, &mut stream);
            let response = read.await?;

            Ok(Event::Response {
                request_id,
                response,
            })
        };

        self.worker_streams.push(Box::pin(async move {
            match future::select(pin!(send), futures_timer::Delay::new(timeout)).await {
                future::Either::Left((recv, _)) => recv,
                future::Either::Right(((), _)) => Ok(Event::OutboundTimeout(request_id)),
            }
        }));
    }

    fn on_dial_upgrade_error(
        &mut self,
        DialUpgradeError { error, info: () }: DialUpgradeError<
            <Self as ConnectionHandler>::OutboundOpenInfo,
            <Self as ConnectionHandler>::OutboundProtocol,
        >,
    ) {
        match error {
            StreamUpgradeError::Timeout => {
                unreachable!("`future::Ready` never times out")
            }
            StreamUpgradeError::NegotiationFailed => {
                let message = self
                    .requested_outbound
                    .pop_front()
                    .expect("negotiated a stream without a pending message");

                // The remote merely doesn't support the protocol(s) we requested.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                // An event is reported to permit user code to react to the fact that
                // the remote peer does not support the requested protocol(s).
                self.pending_events
                    .push_back(Event::OutboundUnsupportedProtocols(message.request_id));
            }
            StreamUpgradeError::Apply(e) => void::unreachable(e),
            StreamUpgradeError::Io(e) => {
                log::debug!("outbound stream failed: {e}");
            }
        }
    }
    fn on_listen_upgrade_error(
        &mut self,
        ListenUpgradeError { error, .. }: ListenUpgradeError<
            <Self as ConnectionHandler>::InboundOpenInfo,
            <Self as ConnectionHandler>::InboundProtocol,
        >,
    ) {
        void::unreachable(error)
    }
}

/// The events emitted by the [`Handler`].
pub enum Event<TCodec>
where
    TCodec: Codec,
{
    /// A request has been received.
    Request {
        request_id: RequestId,
        request: TCodec::Request,
        sender: oneshot::Sender<TCodec::Response>,
    },
    /// A response has been received.
    Response {
        request_id: RequestId,
        response: TCodec::Response,
    },
    /// A response to an inbound request has been sent.
    ResponseSent(RequestId),
    /// A response to an inbound request was omitted as a result
    /// of dropping the response `sender` of an inbound `Request`.
    ResponseOmission(RequestId),
    /// An outbound request timed out while sending the request
    /// or waiting for the response.
    OutboundTimeout(RequestId),
    /// An outbound request failed to negotiate a mutually supported protocol.
    OutboundUnsupportedProtocols(RequestId),
}

impl<TCodec: Codec> fmt::Debug for Event<TCodec> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Request {
                request_id,
                request: _,
                sender: _,
            } => f
                .debug_struct("Event::Request")
                .field("request_id", request_id)
                .finish(),
            Event::Response {
                request_id,
                response: _,
            } => f
                .debug_struct("Event::Response")
                .field("request_id", request_id)
                .finish(),
            Event::ResponseSent(request_id) => f
                .debug_tuple("Event::ResponseSent")
                .field(request_id)
                .finish(),
            Event::ResponseOmission(request_id) => f
                .debug_tuple("Event::ResponseOmission")
                .field(request_id)
                .finish(),
            Event::OutboundTimeout(request_id) => f
                .debug_tuple("Event::OutboundTimeout")
                .field(request_id)
                .finish(),
            Event::OutboundUnsupportedProtocols(request_id) => f
                .debug_tuple("Event::OutboundUnsupportedProtocols")
                .field(request_id)
                .finish(),
        }
    }
}

pub struct OutboundMessage<TCodec: Codec> {
    pub(crate) request_id: RequestId,
    pub(crate) request: TCodec::Request,
    pub(crate) protocols: SmallVec<[TCodec::Protocol; 2]>,
}

impl<TCodec> fmt::Debug for OutboundMessage<TCodec>
where
    TCodec: Codec,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundMessage").finish_non_exhaustive()
    }
}

impl<TCodec> ConnectionHandler for Handler<TCodec>
where
    TCodec: Codec + Send + Clone + 'static,
{
    type InEvent = OutboundMessage<TCodec>;
    type OutEvent = Event<TCodec>;
    type Error = void::Void;
    type InboundProtocol = Protocol<TCodec::Protocol>;
    type OutboundProtocol = Protocol<TCodec::Protocol>;
    type OutboundOpenInfo = ();
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(
            Protocol {
                protocols: self.inbound_protocols.clone(),
            },
            (),
        )
    }

    fn on_behaviour_event(&mut self, request: Self::InEvent) {
        self.keep_alive = KeepAlive::Yes;
        self.pending_outbound.push_back(request);
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keep_alive
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ConnectionHandlerEvent<Protocol<TCodec::Protocol>, (), Self::OutEvent, Self::Error>>
    {
        while let Poll::Ready(Some(result)) = self.worker_streams.poll_next_unpin(cx) {
            match result {
                Ok(event) => return Poll::Ready(ConnectionHandlerEvent::Custom(event)),
                Err(e) => {
                    log::debug!("worker stream failed: {e}")
                }
            }
        }

        // Drain pending events.
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(ConnectionHandlerEvent::Custom(event));
        } else if self.pending_events.capacity() > EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_events.shrink_to_fit();
        }

        // Check for inbound requests.
        while let Poll::Ready(Some(result)) = self.inbound.poll_next_unpin(cx) {
            match result {
                Ok(((id, rq), rs_sender)) => {
                    // We received an inbound request.
                    self.keep_alive = KeepAlive::Yes;
                    return Poll::Ready(ConnectionHandlerEvent::Custom(Event::Request {
                        request_id: id,
                        request: rq,
                        sender: rs_sender,
                    }));
                }
                Err(oneshot::Canceled) => {
                    // The inbound upgrade has errored or timed out reading
                    // or waiting for the request. The handler is informed
                    // via `on_connection_event` call with `ConnectionEvent::ListenUpgradeError`.
                }
            }
        }

        // Emit outbound requests.
        if let Some(request) = self.pending_outbound.pop_front() {
            let protocols = request.protocols.clone();
            self.requested_outbound.push_back(request);

            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest {
                protocol: SubstreamProtocol::new(Protocol { protocols }, ()),
            });
        }

        debug_assert!(self.pending_outbound.is_empty());

        if self.pending_outbound.capacity() > EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_outbound.shrink_to_fit();
        }

        if self.inbound.is_empty() && self.keep_alive.is_yes() {
            // No new inbound or outbound requests. However, we may just have
            // started the latest inbound or outbound upgrade(s), so make sure
            // the keep-alive timeout is preceded by the substream timeout.
            let until = Instant::now() + self.substream_timeout + self.keep_alive_timeout;
            self.keep_alive = KeepAlive::Until(until);
        }

        Poll::Pending
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(fully_negotiated_inbound) => {
                self.on_fully_negotiated_inbound(fully_negotiated_inbound)
            }
            ConnectionEvent::FullyNegotiatedOutbound(fully_negotiated_outbound) => {
                self.on_fully_negotiated_outbound(fully_negotiated_outbound)
            }
            ConnectionEvent::DialUpgradeError(dial_upgrade_error) => {
                self.on_dial_upgrade_error(dial_upgrade_error)
            }
            ConnectionEvent::ListenUpgradeError(listen_upgrade_error) => {
                self.on_listen_upgrade_error(listen_upgrade_error)
            }
            ConnectionEvent::AddressChange(_)
            | ConnectionEvent::LocalProtocolsChange(_)
            | ConnectionEvent::RemoteProtocolsChange(_) => {}
        }
    }
}
