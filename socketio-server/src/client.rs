use std::collections::HashMap;
use std::sync::{Arc, Weak};

use engineio_server::socket::Socket as EIoSocket;
use engineio_server::{engine::EngineIo, layer::EngineIoHandler};
use serde::Serialize;
use tracing::debug;

use crate::handshake::Handshake;
use crate::{
    config::SocketIoConfig,
    errors::Error,
    ns::{EventCallback, Namespace},
    packet::{Packet, PacketData},
};

pub struct Client {
    config: SocketIoConfig,
    ns: HashMap<String, Namespace>,
    engine: Weak<EngineIo<Self>>,
}

impl Client {
    pub fn new(
        config: SocketIoConfig,
        engine: Weak<EngineIo<Self>>,
        ns_handlers: HashMap<String, EventCallback>,
    ) -> Self {
        let client = Self {
            config,
            engine,
            ns: ns_handlers
                .into_iter()
                .map(|(path, callback)| (path.clone(), Namespace::new(path, callback)))
                .collect(),
        };
        client
    }

    pub async fn emit<T>(&self, sid: i64, packet: Packet<T>) -> Result<(), Error>
    where
        T: Serialize,
    {
        // debug!("Emitting packet: {:?}", packet);
        let socket = self.engine.upgrade().unwrap().get_socket(sid).unwrap();
        socket.emit(packet.try_into()?).await.unwrap();
        Ok(())
    }
}

#[engineio_server::async_trait]
impl EngineIoHandler for Client {
    fn on_connect(self: Arc<Self>, socket: &EIoSocket<Self>) {
        println!("socket connect {}", socket.sid);
        // self.state = SocketState::AwaitingConnect;
    }
    fn on_disconnect(self: Arc<Self>, socket: &EIoSocket<Self>) {
        println!("socket disconnect {}", socket.sid);
    }

    async fn on_message(self: Arc<Self>, msg: String, socket: &EIoSocket<Self>) {
        debug!("Received message: {:?}", msg);
        match Packet::<serde_json::Value>::try_from(msg) {
            Ok(Packet {
                inner: PacketData::Connect(auth),
                ns: ns_path,
            }) => {
                debug!("auth: {:?}", auth);
                let handshake = Handshake {
                    url: "".to_string(),
                    issued: 0,
                    auth,
                };
                if let Some(ns) = self.ns.get(&ns_path) {
                    ns.connect(socket.sid, self.clone(), handshake);
                    self.emit(socket.sid, Packet::connect(ns_path, socket.sid))
                        .await
                        .unwrap();
                } else {
                    self.emit(socket.sid, Packet::invalid_namespace(ns_path))
                        .await
                        .unwrap();
                }
            }
            Ok(Packet {
                inner: PacketData::Event(msg, d),
                ns,
            }) => {
                if let Some(ns) = self.ns.get(&ns) {
                    ns.recv_event(socket.sid, msg, d);
                }
            },
            Ok(Packet {
                inner: PacketData::Disconnect,
                ns
            }) => {
                if let Some(ns) = self.ns.get(&ns) {
                    ns.disconnect(socket.sid);
                    // If there is no other namespaces connected, close the underlying socket
                    // if !self.ns.values().any(|ns| ns.has(socket.sid)) {
                    //     socket.emit_close().await;
                    // }
                }
            }
            Err(e) => {
                debug!("socket serialization error: {}", e);
                socket.emit_close().await;
            }
            _ => {}
        };
    }

    async fn on_binary(self: Arc<Self>, data: Vec<u8>, socket: &EIoSocket<Self>) {
        println!("Ping pong binary message {:?}", data);
    }
}
