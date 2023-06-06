use libp2p_core::multihash::Multihash;
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent, StreamMuxerExt};
use libp2p_core::{OutboundUpgrade, UpgradeInfo};
use libp2p_identity::{Keypair, PeerId};
use send_wrapper::SendWrapper;
use std::collections::HashSet;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStreamDefaultReader;

use crate::bindings::{WebTransport, WebTransportBidirectionalStream};
use crate::endpoint::Endpoint;
use crate::fused_js_promise::FusedJsPromise;
use crate::utils::{parse_reader_response, to_js_type};
use crate::{Error, Stream};

/// An opened WebTransport connection.
pub struct Connection {
    session: SendWrapper<WebTransport>,
    create_stream_promise: FusedJsPromise,
    incoming_stream_promise: FusedJsPromise,
    incoming_streams_reader: SendWrapper<ReadableStreamDefaultReader>,
    closed: bool,
}

impl Connection {
    pub(crate) fn new(endpoint: &Endpoint) -> Result<Self, Error> {
        let url = endpoint.url();

        let session = if endpoint.certhashes.is_empty() {
            // Endpoint has CA-signed TLS certificate
            WebTransport::new(&url).map_err(Error::from_js_value)?
        } else {
            // Endpoint has self-signed TLS certificates
            let opts = endpoint.webtransport_opts();
            WebTransport::new_with_options(&url, &opts).map_err(Error::from_js_value)?
        };

        let incoming_streams = session.incoming_bidirectional_streams();
        let incoming_streams_reader =
            to_js_type::<ReadableStreamDefaultReader>(incoming_streams.get_reader())?;

        Ok(Connection {
            session: SendWrapper::new(session),
            create_stream_promise: FusedJsPromise::new(),
            incoming_stream_promise: FusedJsPromise::new(),
            incoming_streams_reader: SendWrapper::new(incoming_streams_reader),
            closed: false,
        })
    }

    pub(crate) async fn authenticate(
        &mut self,
        keypair: &Keypair,
        remote_peer: Option<PeerId>,
        certhashes: HashSet<Multihash>,
    ) -> Result<PeerId, Error> {
        self.ready().await?;
        let stream = self.create_stream().await?;

        let mut noise = libp2p_noise::Config::new(keypair)?;

        if !certhashes.is_empty() {
            noise = noise.with_webtransport_certhashes(certhashes);
        }

        // We do not use `upgrade::apply_outbound` function because it uses
        // `multistream_select` protocol, which is not used by WebTransport spec.
        let info = noise.protocol_info().next().unwrap_or_default();
        let (peer_id, _io) = noise.upgrade_outbound(stream, info).await?;

        // TODO: This should be part libp2p-noise
        if let Some(expected_peer_id) = remote_peer {
            if peer_id != expected_peer_id {
                return Err(Error::UnknownRemotePeerId);
            }
        }

        Ok(peer_id)
    }

    /// Awaits the session to be ready.
    async fn ready(&mut self) -> Result<(), Error> {
        let fut = SendWrapper::new(JsFuture::from(self.session.ready()));
        fut.await.map_err(Error::from_js_value)?;
        Ok(())
    }

    /// Creates new outbound stream.
    async fn create_stream(&mut self) -> Result<Stream, Error> {
        poll_fn(|cx| self.poll_outbound_unpin(cx)).await
    }

    /// Initiates and polls a promise from `create_bidirectional_stream`.
    fn poll_create_bidirectional_stream(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<Stream, Error>> {
        // Create bidirectional stream
        let val = ready!(self
            .create_stream_promise
            .maybe_init_and_poll(cx, || self.session.create_bidirectional_stream()))
        .map_err(Error::from_js_value)?;

        let bidi_stream = to_js_type::<WebTransportBidirectionalStream>(val)?;
        let stream = Stream::new(bidi_stream)?;

        Poll::Ready(Ok(stream))
    }

    /// Polls for incoming stream from `incoming_bidirectional_streams` reader.
    fn poll_incoming_bidirectional_streams(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<Stream, Error>> {
        // Read the next incoming stream from the JS channel
        let val = ready!(self
            .incoming_stream_promise
            .maybe_init_and_poll(cx, || self.incoming_streams_reader.read()))
        .map_err(Error::from_js_value)?;

        let val = parse_reader_response(&val)
            .map_err(Error::from_js_value)?
            .ok_or_else(|| Error::JsError("incoming_bidirectional_streams closed".to_string()))?;

        let bidi_stream = to_js_type::<WebTransportBidirectionalStream>(val)?;
        let stream = Stream::new(bidi_stream)?;

        Poll::Ready(Ok(stream))
    }

    /// Closes the session.
    ///
    /// This closes the streams also and they will return an error
    /// when they will be used.
    fn close_session(&mut self) {
        if !self.closed {
            self.session.close();
            self.closed = true;
        }
    }
}

/// WebTransport native multiplexing
impl StreamMuxer for Connection {
    type Substream = Stream;
    type Error = Error;

    fn poll_inbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        self.poll_incoming_bidirectional_streams(cx)
    }

    fn poll_outbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        self.poll_create_bidirectional_stream(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.close_session();
        Poll::Ready(Ok(()))
    }

    fn poll(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
        Poll::Pending
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close_session();
    }
}
