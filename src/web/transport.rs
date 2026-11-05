//! The WhatsApp socket through the browser's `WebSocket`.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};
use whatsapp_rust::wacore::net::{
    DisconnectReason, Transport, TransportEvent, TransportFactory, WHATSAPP_WEB_WS_URL,
};

/// Dials WhatsApp from the page.
pub struct WebSocketFactory;

struct WebSocketTransport {
    socket: WebSocket,
    // Kept for as long as the socket can call them.
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
}

impl Drop for WebSocketTransport {
    fn drop(&mut self) {
        // Its closures go with it, so it must not call them again.
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        let _ = self.socket.close();
    }
}

#[async_trait::async_trait(?Send)]
impl Transport for WebSocketTransport {
    async fn send(&self, data: Bytes) -> Result<()> {
        self.socket
            .send_with_u8_array(&data)
            .map_err(|_| anyhow!("the socket refused the data"))
    }

    async fn disconnect(&self) {
        let _ = self.socket.close();
    }
}

#[async_trait::async_trait(?Send)]
impl TransportFactory for WebSocketFactory {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>)> {
        let socket = WebSocket::new(WHATSAPP_WEB_WS_URL)
            .map_err(|_| anyhow!("could not open the socket"))?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let (events, received) = async_channel::unbounded();
        let (opened, open) = async_channel::bounded::<Result<(), String>>(1);

        let on_open = {
            let opened = opened.clone();
            Closure::<dyn FnMut(Event)>::new(move |_| {
                let _ = opened.try_send(Ok(()));
            })
        };
        let on_error = {
            let opened = opened.clone();
            Closure::<dyn FnMut(Event)>::new(move |_| {
                // After it opens, the close event that follows says how it ended.
                let _ = opened.try_send(Err("the socket failed to connect".to_owned()));
            })
        };
        let on_message = {
            let events = events.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                    let _ = events.try_send(TransportEvent::DataReceived(Bytes::from(bytes)));
                }
            })
        };
        let on_close = {
            let events = events.clone();
            Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
                let _ = opened.try_send(Err("the socket closed while connecting".to_owned()));
                let _ = events.try_send(TransportEvent::Disconnected(
                    DisconnectReason::ServerClose {
                        code: Some(event.code()),
                        reason: event.reason(),
                    },
                ));
            })
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        // From here a dropped future closes the half-open socket.
        let transport = Arc::new(WebSocketTransport {
            socket,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        });
        match open.recv().await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => return Err(anyhow!(reason)),
            Err(_) => return Err(anyhow!("the socket went away while connecting")),
        }
        let _ = events.try_send(TransportEvent::Connected);
        Ok((transport, received))
    }
}
