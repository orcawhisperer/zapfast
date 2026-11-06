//! A call's media in a browser. The desktop dials a relay over UDP and runs DTLS and SCTP itself;
//! a page cannot, but an `RTCPeerConnection` given a written answer (`relay_answer`) reaches the
//! same relay over the same pre-negotiated data channel.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::oneshot;
use wacore::voip_control::transport::{
    RelayDisconnectReason, RelayEndpointParams, RelayTransport, RelayTransportEvent,
    RelayTransportFactory, RelayTransportProvider,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcDataChannelType, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

/// The label and stream id both ends assume. A pre-negotiated channel has no handshake of its own.
const CHANNEL_LABEL: &str = "pre-negotiated";
const CHANNEL_ID: u16 = 0;

/// How many packets may wait for the call's engine. A browser hands them over in a callback that
/// cannot wait, so past this the oldest go, which media can afford.
const INBOUND_DEPTH: usize = 256;

/// How long a relay has to answer. There is one address to try, so this is the handshakes alone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

thread_local! {
    /// Access units dropped since the camera last looked, which leaves the peer's decoder without
    /// its reference. The library thinks they went, as the send succeeded.
    static SHED_UNITS: Cell<u32> = const { Cell::new(0) };
}

/// How many access units were dropped since this was last asked.
pub fn take_shed_units() -> u32 {
    SHED_UNITS.take()
}

/// How media reaches the relay in a page.
pub struct BrowserRelay;

#[async_trait(?Send)]
impl RelayTransportProvider for BrowserRelay {
    async fn factory(&self, relay: &RelayEndpointParams) -> Result<Arc<dyn RelayTransportFactory>> {
        let present = js_sys::Reflect::get(&js_sys::global(), &"RTCPeerConnection".into())
            .is_ok_and(|found| !found.is_undefined());
        if !present {
            bail!("this browser has no WebRTC, which a call's media needs");
        }
        Ok(Arc::new(Dialer {
            params: relay.clone(),
        }))
    }
}

struct Dialer {
    params: RelayEndpointParams,
}

#[async_trait(?Send)]
impl RelayTransportFactory for Dialer {
    async fn connect(
        &self,
    ) -> Result<(
        Arc<dyn RelayTransport>,
        async_channel::Receiver<RelayTransportEvent>,
    )> {
        dial(&self.params).await
    }
}

/// Dials the relay at `params`, within the limit.
pub async fn dial(
    params: &RelayEndpointParams,
) -> Result<(
    Arc<dyn RelayTransport>,
    async_channel::Receiver<RelayTransportEvent>,
)> {
    // The relay's credentials stay out of this: an error here is logged.
    crate::web::time::timeout(CONNECT_TIMEOUT, connect(params))
        .await
        .map_err(|_| anyhow!("the relay did not answer within {CONNECT_TIMEOUT:?}"))?
}

/// A JavaScript error's words, for a log.
fn described(error: &JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map_or_else(|| "an error".to_owned(), |error| String::from(error.name()))
}

/// Everything one channel keeps alive. The browser calls into the closures, so they go last, once
/// the channel can no longer call them.
struct Wiring {
    connection: RtcPeerConnection,
    channel: RtcDataChannel,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _closed: Closure<dyn FnMut(Event)>,
    _failed: Closure<dyn FnMut(Event)>,
    _opened: Closure<dyn FnMut(Event)>,
}

impl Drop for Wiring {
    fn drop(&mut self) {
        self.channel.set_onmessage(None);
        self.channel.set_onclose(None);
        self.channel.set_onerror(None);
        self.channel.set_onopen(None);
        self.channel.close();
        self.connection.close();
    }
}

async fn connect(
    params: &RelayEndpointParams,
) -> Result<(
    Arc<dyn RelayTransport>,
    async_channel::Receiver<RelayTransportEvent>,
)> {
    // No servers to ask: the relay is the one candidate, and it is in the answer.
    let config = RtcConfiguration::new();
    config.set_ice_servers(&js_sys::Array::new());
    let connection = RtcPeerConnection::new_with_configuration(&config)
        .map_err(|error| anyhow!("RTCPeerConnection: {}", described(&error)))?;

    let init = RtcDataChannelInit::new();
    init.set_negotiated(true);
    init.set_id(CHANNEL_ID);
    // Unreliable and unordered: real-time media on a reliable stream stalls at every loss.
    init.set_ordered(false);
    init.set_max_retransmits(0);
    let channel = connection.create_data_channel_with_data_channel_dict(CHANNEL_LABEL, &init);
    channel.set_binary_type(RtcDataChannelType::Arraybuffer);

    let (events, inbound) = async_channel::bounded(INBOUND_DEPTH);
    let (opened_tx, opened_rx) = oneshot::channel::<()>();
    // Whichever of opening or failing comes first settles the wait for it.
    let waiting = Rc::new(Cell::new(Some(opened_tx)));
    let dropped = Rc::new(Cell::new(0u32));
    let received = Rc::new(Cell::new(0u64));

    let message = {
        let events = events.clone();
        let dropped = Rc::clone(&dropped);
        let received = Rc::clone(&received);
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            let Some(buffer) = data.dyn_ref::<js_sys::ArrayBuffer>() else {
                return;
            };
            received.set(received.get() + 1);
            let packet = Bytes::from(js_sys::Uint8Array::new(buffer).to_vec());
            if let Ok(Some(_)) = events.force_send(RelayTransportEvent::PacketReceived(packet)) {
                dropped.set(dropped.get().saturating_add(1));
            }
            let lost = dropped.get();
            if lost > 0
                && events
                    .try_send(RelayTransportEvent::InboundDropped(lost))
                    .is_ok()
            {
                dropped.set(0);
            }
        })
    };
    let ending = |reason: fn() -> RelayDisconnectReason| {
        let events = events.clone();
        let waiting = Rc::clone(&waiting);
        Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            // Over before it opened: there is nothing more to wait for.
            waiting.take();
            // The one event the engine cannot do without, even when the queue is full.
            let _ = events.force_send(RelayTransportEvent::Disconnected(reason()));
        })
    };
    let closed = ending(|| RelayDisconnectReason::Closed);
    let failed =
        ending(|| RelayDisconnectReason::ReadError("the relay channel reported an error".into()));
    let opened = {
        let waiting = Rc::clone(&waiting);
        Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            if let Some(waiting) = waiting.take() {
                let _ = waiting.send(());
            }
        })
    };
    channel.set_onmessage(Some(message.as_ref().unchecked_ref()));
    channel.set_onclose(Some(closed.as_ref().unchecked_ref()));
    channel.set_onerror(Some(failed.as_ref().unchecked_ref()));
    channel.set_onopen(Some(opened.as_ref().unchecked_ref()));
    // From here, however this ends, dropping this closes the connection.
    let wiring = Wiring {
        connection,
        channel: channel.clone(),
        _message: message,
        _closed: closed,
        _failed: failed,
        _opened: opened,
    };

    let offer = js_sys::Reflect::get(
        &JsFuture::from(wiring.connection.create_offer())
            .await
            .map_err(|error| anyhow!("createOffer: {}", described(&error)))?,
        &"sdp".into(),
    )
    .ok()
    .and_then(|sdp| sdp.as_string())
    .ok_or_else(|| anyhow!("the browser's offer had no SDP"))?;
    let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local.set_sdp(&offer);
    JsFuture::from(wiring.connection.set_local_description(&local))
        .await
        .map_err(|error| anyhow!("setLocalDescription: {}", described(&error)))?;

    let answer =
        crate::relay_answer::answer(&offer, params.addr, &params.ice_ufrag, &params.ice_pwd)
            .map_err(|error| anyhow!(error))?;
    let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    remote.set_sdp(&answer);
    JsFuture::from(wiring.connection.set_remote_description(&remote))
        .await
        .map_err(|error| anyhow!("setRemoteDescription: {}", described(&error)))?;

    opened_rx
        .await
        .map_err(|_| anyhow!("the relay channel closed before it opened"))?;
    log::info!("[CALL] the relay's media channel is open");
    Ok((
        Arc::new(Channel {
            wiring,
            params: params.clone(),
            sent: Cell::new(0),
            received,
            refused: Cell::new(false),
            shed: Cell::new(0),
            unit: Cell::new(crate::relay_answer::VideoUnit::Between),
            closed: Cell::new(false),
        }),
        inbound,
    ))
}

/// The open channel, as the call's engine sees it.
struct Channel {
    wiring: Wiring,
    params: RelayEndpointParams,
    sent: Cell<u64>,
    received: Rc<Cell<u64>>,
    /// Whether a refused write has been logged, which the engine does not do.
    refused: Cell<bool>,
    shed: Cell<u64>,
    unit: Cell<crate::relay_answer::VideoUnit>,
    closed: Cell<bool>,
}

#[async_trait(?Send)]
impl RelayTransport for Channel {
    async fn send(&self, data: Bytes) -> Result<()> {
        let channel = &self.wiring.channel;
        if channel.ready_state() != RtcDataChannelState::Open {
            bail!("the relay channel is not open");
        }
        let (send, verdict) = self.unit.get().admit(&data, channel.buffered_amount());
        self.unit.set(verdict.next);
        if verdict.dropped_unit {
            self.shed.set(self.shed.get() + 1);
            SHED_UNITS.set(SHED_UNITS.get().saturating_add(1));
        }
        if !send {
            return Ok(());
        }
        // Copied out of the program's memory into a buffer of the browser's own.
        let buffer = js_sys::Uint8Array::from(&data[..]).buffer();
        match channel.send_with_array_buffer(&buffer) {
            Ok(()) => {
                self.sent.set(self.sent.get() + 1);
                Ok(())
            }
            Err(error) => {
                // A failed send ends the call without saying why, so it is said here.
                if !self.refused.replace(true) {
                    log::warn!(
                        "[CALL] the relay channel refused a packet: {}",
                        described(&error)
                    );
                }
                bail!("the relay channel refused a packet")
            }
        }
    }

    async fn disconnect(&self) {
        if !self.closed.replace(true) {
            log::info!(
                "[CALL] the relay's channel carried {} packets out, {} in, and dropped {} video frames",
                self.sent.get(),
                self.received.get(),
                self.shed.get(),
            );
            self.wiring.channel.close();
            self.wiring.connection.close();
        }
    }

    /// Moves to another relay with the same credentials, which is all the engine says about one.
    async fn reconnect(
        &self,
        endpoint: SocketAddr,
    ) -> Result<(
        Arc<dyn RelayTransport>,
        async_channel::Receiver<RelayTransportEvent>,
    )> {
        let mut params = self.params.clone();
        params.addr = endpoint;
        dial(&params).await
    }
}
