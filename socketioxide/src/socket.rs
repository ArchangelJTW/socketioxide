use std::{
    collections::HashMap,
    collections::VecDeque,
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Mutex,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use engineioxide::{sid_generator::Sid, SendPacket as EnginePacket};
use futures::Future;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;

use crate::errors::{GoodNameError, SendError};
use crate::{
    adapter::{Adapter, Room},
    errors::{AckError, Error},
    extensions::Extensions,
    handler::{AckResponse, AckSender, BoxedHandler, MessageHandler},
    handshake::Handshake,
    ns::Namespace,
    operators::{Operators, RoomParam},
    packet::{BinaryPacket, Packet, PacketData},
    SocketIoConfig,
};

/// A Socket represents a client connected to a namespace.
/// It is used to send and receive messages from the client, join and leave rooms, etc.
pub struct Socket<A: Adapter> {
    config: Arc<SocketIoConfig>,
    ns: Arc<Namespace<A>>,
    message_handlers: RwLock<HashMap<String, BoxedHandler<A>>>,
    ack_message: RwLock<HashMap<i64, oneshot::Sender<AckResponse<Value>>>>,
    ack_counter: AtomicI64,
    pub handshake: Handshake,
    pub sid: Sid,
    pub extensions: Extensions,
    sender: Mutex<PacketSender>,
}

struct PacketSender {
    tx: tokio::sync::mpsc::Sender<EnginePacket>,
    bin_payloads: Option<VecDeque<EnginePacket>>,
}

impl PacketSender {
    fn new(
        tx: tokio::sync::mpsc::Sender<EnginePacket>,
        failed_buffer: VecDeque<EnginePacket>,
    ) -> Self {
        Self {
            tx,
            bin_payloads: (!failed_buffer.is_empty()).then(|| failed_buffer),
        }
    }

    fn send_raw(&mut self, mut packet: RetryablePacket) -> Result<(), GoodNameError> {
        if let Err(err) = self.send_binaries() {
            match err {
                GoodNameError::SendFailedBinPayloads(None) => {}
                GoodNameError::SocketClosed => return Err(GoodNameError::SocketClosed),
                _ => unreachable!(),
            };
            Err(GoodNameError::SendMainPacket(packet))
        } else {
            let main_packet = packet.pop_front();
            let Some(main_packet) = main_packet else {
                unreachable!()
            };

            match self.tx.try_send(main_packet) {
                Err(TrySendError::Full(main_packet)) => {
                    packet.push_front(main_packet);
                    Err(GoodNameError::SendMainPacket(packet))
                }
                Err(TrySendError::Closed(_)) => Err(GoodNameError::SocketClosed),
                _ => {
                    self.bin_payloads = Some(packet.into());
                    self.send_binaries()?;
                    Ok(())
                }
            }
        }
    }

    fn send(&mut self, mut packet: Packet) -> Result<(), SendError> {
        if let Err(err) = self.send_binaries() {
            Err(err.add_main_packet(packet).into())
        } else {
            let bin_payloads = match packet.inner {
                PacketData::BinaryEvent(_, ref mut bin, _)
                | PacketData::BinaryAck(ref mut bin, _) => Some(
                    std::mem::take(&mut bin.bin)
                        .into_iter()
                        .map(EnginePacket::Binary)
                        .collect(),
                ),
                _ => None,
            };
            match self.tx.try_send(packet.try_into()?) {
                Err(TrySendError::Full(packet)) => {
                    let mut bin_payloads = bin_payloads.unwrap_or(VecDeque::with_capacity(1));
                    bin_payloads.push_front(packet);
                    return Err(GoodNameError::SendMainPacket(RetryablePacket(bin_payloads)).into());
                }
                Err(TrySendError::Closed(_)) => {
                    return Err(GoodNameError::SocketClosed.into());
                }
                _ => {}
            };
            self.bin_payloads = bin_payloads;
            self.send_binaries()?;
            Ok(())
        }
    }

    fn send_binaries(&mut self) -> Result<(), GoodNameError> {
        let payloads = self.bin_payloads.take();
        if let Some(mut payloads) = payloads {
            while let Some(p) = payloads.pop_front() {
                match self.tx.try_send(p) {
                    Err(TrySendError::Full(p @ EnginePacket::Binary(_))) => {
                        payloads.push_front(p);
                        self.bin_payloads = Some(payloads);
                        return Err(GoodNameError::SendFailedBinPayloads(None));
                    }
                    Err(TrySendError::Full(EnginePacket::Message(_))) => unreachable!(),
                    Err(TrySendError::Closed(_)) => return Err(GoodNameError::SocketClosed),
                    _ => {}
                }
            }
            Ok(())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct RetryablePacket(VecDeque<EnginePacket>);

impl From<RetryablePacket> for VecDeque<EnginePacket> {
    fn from(value: RetryablePacket) -> Self {
        value.0
    }
}

impl RetryablePacket {
    pub fn retry<A: Adapter>(self, socket: &Socket<A>) -> Result<(), GoodNameError> {
        socket.sender.lock().unwrap().send_raw(self)
    }
}

impl DerefMut for RetryablePacket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for RetryablePacket {
    type Target = VecDeque<EnginePacket>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<A: Adapter> Socket<A> {
    pub(crate) fn new(
        sid: Sid,
        ns: Arc<Namespace<A>>,
        handshake: Handshake,
        tx: tokio::sync::mpsc::Sender<EnginePacket>,
        config: Arc<SocketIoConfig>,
    ) -> Self {
        Self {
            ns,
            message_handlers: RwLock::new(HashMap::new()),
            ack_message: RwLock::new(HashMap::new()),
            ack_counter: AtomicI64::new(0),
            handshake,
            sid,
            extensions: Extensions::new(),
            config,
            sender: Mutex::new(PacketSender::new(tx, VecDeque::new())),
        }
    }

    /// ### Register a message handler for the given event.
    ///
    /// The data parameter can be typed with anything that implement [serde::Deserialize](https://docs.rs/serde/latest/serde/)
    ///
    /// ### Acknowledgements
    /// The ack can be sent only once and take a `Serializable` value as parameter.
    ///
    /// For more info about ack see [socket.io documentation](https://socket.io/fr/docs/v4/emitting-events/#acknowledgements)
    ///
    /// If the client sent a normal message without expecting an ack, the ack callback will do nothing.
    ///
    /// #### Simple example with a closure:
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// # use serde::{Serialize, Deserialize};
    ///
    /// #[derive(Debug, Serialize, Deserialize)]
    /// struct MyData {
    ///     name: String,
    ///     age: u8,
    /// }
    ///
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: MyData, _, _| async move {
    ///         println!("Received a test message {:?}", data);
    ///         socket.emit("test-test", MyData { name: "Test".to_string(), age: 8 }).ok(); // Emit a message to the client
    ///     });
    /// });
    ///
    /// ```
    ///
    /// #### Example with a closure and an ackknowledgement + binary data:
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// # use serde::{Serialize, Deserialize};
    ///
    /// #[derive(Debug, Serialize, Deserialize)]
    /// struct MyData {
    ///     name: String,
    ///     age: u8,
    /// }
    ///
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: MyData, bin, ack| async move {
    ///         println!("Received a test message {:?}", data);
    ///         ack.bin(bin).send(data).ok(); // The data received is sent back to the client through the ack
    ///         socket.emit("test-test", MyData { name: "Test".to_string(), age: 8 }).ok(); // Emit a message to the client
    ///     });
    /// });
    /// ```
    pub fn on<C, F, V>(&self, event: impl Into<String>, callback: C)
    where
        C: Fn(Arc<Socket<A>>, V, Vec<Vec<u8>>, AckSender<A>) -> F + Send + Sync + 'static,
        F: Future<Output = ()> + Send + 'static,
        V: DeserializeOwned + Send + Sync + 'static,
    {
        let handler = Box::new(move |s, v, p, ack_fn| Box::pin(callback(s, v, p, ack_fn)) as _);
        self.message_handlers
            .write()
            .unwrap()
            .insert(event.into(), MessageHandler::boxed(handler));
    }

    /// Emit a message to the client
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, bin, _| async move {
    ///         // Emit a test message to the client
    ///         socket.emit("test", data);
    ///     });
    /// });
    pub fn emit(&self, event: impl Into<String>, data: impl Serialize) -> Result<(), SendError> {
        let ns = self.ns.path.clone();
        let data = serde_json::to_value(data)?;
        self.send(Packet::event(ns, event.into(), data))
    }

    pub fn retry_failed(&self) -> Result<(), GoodNameError> {
        self.resend_failed()
    }

    /// Emit a message to the client and wait for acknowledgement.
    ///
    /// The acknowledgement has a timeout specified in the config (5s by default) or with the `timeout()` operator.
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, bin, _| async move {
    ///         // Emit a test message and wait for an acknowledgement
    ///         match socket.emit_with_ack::<Value>("test", data).await {
    ///             Ok(ack) => println!("Ack received {:?}", ack),
    ///             Err(err) => println!("Ack error {:?}", err),
    ///         }
    ///    });
    /// });
    pub async fn emit_with_ack<V>(
        &self,
        event: impl Into<String>,
        data: impl Serialize,
    ) -> Result<AckResponse<V>, AckError>
    where
        V: DeserializeOwned + Send + Sync + 'static,
    {
        let ns = self.ns.path.clone();
        let data = serde_json::to_value(data)?;
        let packet = Packet::event(ns, event.into(), data);

        self.send_with_ack(packet, None).await
    }

    // Room actions

    /// Join the given rooms.
    pub fn join(&self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.add_all(self.sid, rooms)
    }

    /// Leave the given rooms.
    pub fn leave(&self, rooms: impl RoomParam) -> Result<(), A::Error> {
        self.ns.adapter.del(self.sid, rooms)
    }

    /// Leave all rooms where the socket is connected.
    pub fn leave_all(&self) -> Result<(), A::Error> {
        self.ns.adapter.del_all(self.sid)
    }

    /// Get all rooms where the socket is connected.
    pub fn rooms(&self) -> Result<Vec<Room>, A::Error> {
        self.ns.adapter.socket_rooms(self.sid)
    }

    // Socket operators

    /// Select all clients in the given rooms except the current socket.
    ///
    /// If you want to include the current socket, use the `within()` operator.
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, _, _| async move {
    ///         let other_rooms = "room4".to_string();
    ///         // In room1, room2, room3 and room4 except the current
    ///         socket
    ///             .to("room1")
    ///             .to(["room2", "room3"])
    ///             .to(vec![other_rooms])
    ///             .emit("test", data);
    ///     });
    /// });
    pub fn to(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).to(rooms)
    }

    /// Select all clients in the given rooms.
    ///
    /// It does include the current socket contrary to the `to()` operator.
    /// #### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, _, _| async move {
    ///         let other_rooms = "room4".to_string();
    ///         // In room1, room2, room3 and room4 including the current socket
    ///         socket
    ///             .within("room1")
    ///             .within(["room2", "room3"])
    ///             .within(vec![other_rooms])
    ///             .emit("test", data);
    ///     });
    /// });
    pub fn within(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).within(rooms)
    }

    /// Filter out all clients selected with the previous operators which are in the given rooms.
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("register1", |socket, data: Value, _, _| async move {
    ///         socket.join("room1");
    ///     });
    ///     socket.on("register2", |socket, data: Value, _, _| async move {
    ///         socket.join("room2");
    ///     });
    ///     socket.on("test", |socket, data: Value, _, _| async move {
    ///         // This message will be broadcast to all clients in the Namespace
    ///         // except for ones in room1 and the current socket
    ///         socket.broadcast().except("room1").emit("test", data);
    ///     });
    /// });
    pub fn except(&self, rooms: impl RoomParam) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).except(rooms)
    }

    /// Broadcast to all clients only connected on this node (when using multiple nodes).
    /// When using the default in-memory adapter, this operator is a no-op.
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, _, _| async move {
    ///         // This message will be broadcast to all clients in this namespace and connected on this node
    ///         socket.local().emit("test", data);
    ///     });
    /// });
    pub fn local(&self) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).local()
    }

    /// Set a custom timeout when sending a message with an acknowledgement.
    ///
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// # use futures::stream::StreamExt;
    /// # use std::time::Duration;
    /// Namespace::builder().add("/", |socket| async move {
    ///    socket.on("test", |socket, data: Value, bin, _| async move {
    ///       // Emit a test message in the room1 and room3 rooms, except for the room2 room with the binary payload received, wait for 5 seconds for an acknowledgement
    ///       socket.to("room1")
    ///             .to("room3")
    ///             .except("room2")
    ///             .bin(bin)
    ///             .timeout(Duration::from_secs(5))
    ///             .emit_with_ack::<Value>("message-back", data).unwrap().for_each(|ack| async move {
    ///                match ack {
    ///                    Ok(ack) => println!("Ack received {:?}", ack),
    ///                    Err(err) => println!("Ack error {:?}", err),
    ///                }
    ///             }).await;
    ///    });
    /// });
    ///
    pub fn timeout(&self, timeout: Duration) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).timeout(timeout)
    }

    /// Add a binary payload to the message.
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, bin, _| async move {
    ///         // This will send the binary payload received to all clients in this namespace with the test message
    ///         socket.bin(bin).emit("test", data);
    ///     });
    /// });
    pub fn bin(&self, binary: Vec<Vec<u8>>) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).bin(binary)
    }

    /// Broadcast to all clients without any filtering (except the current socket).
    /// ##### Example
    /// ```
    /// # use socketioxide::Namespace;
    /// # use serde_json::Value;
    /// Namespace::builder().add("/", |socket| async move {
    ///     socket.on("test", |socket, data: Value, _, _| async move {
    ///         // This message will be broadcast to all clients in this namespace
    ///         socket.broadcast().emit("test", data);
    ///     });
    /// });
    pub fn broadcast(&self) -> Operators<A> {
        Operators::new(self.ns.clone(), self.sid).broadcast()
    }

    /// Disconnect the socket from the current namespace.
    pub fn disconnect(&self) -> Result<(), SendError> {
        self.ns.disconnect(self.sid)
    }

    /// Get the current namespace path.
    pub fn ns(&self) -> &String {
        &self.ns.path
    }

    pub(crate) fn send(&self, packet: Packet) -> Result<(), SendError> {
        self.sender.lock().unwrap().send(packet)
    }
    pub(crate) fn resend_failed(&self) -> Result<(), GoodNameError> {
        self.sender.lock().unwrap().send_binaries()
    }

    pub(crate) async fn send_with_ack<V: DeserializeOwned>(
        &self,
        mut packet: Packet,
        timeout: Option<Duration>,
    ) -> Result<AckResponse<V>, AckError> {
        let (tx, rx) = oneshot::channel();
        let ack = self.ack_counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.ack_message.write().unwrap().insert(ack, tx);
        packet.inner.set_ack_id(ack);
        self.send(packet)?;
        let timeout = timeout.unwrap_or(self.config.ack_timeout);
        let v = tokio::time::timeout(timeout, rx).await??;
        Ok((serde_json::from_value(v.0)?, v.1))
    }

    // Receive data from client:

    pub(crate) fn recv(self: Arc<Self>, packet: PacketData) -> Result<(), Error> {
        match packet {
            PacketData::Event(e, data, ack) => self.recv_event(e, data, ack),
            PacketData::EventAck(data, ack_id) => self.recv_ack(data, ack_id),
            PacketData::BinaryEvent(e, packet, ack) => self.recv_bin_event(e, packet, ack),
            PacketData::BinaryAck(packet, ack) => self.recv_bin_ack(packet, ack),
            _ => unreachable!(),
        }
    }

    fn recv_event(self: Arc<Self>, e: String, data: Value, ack: Option<i64>) -> Result<(), Error> {
        if let Some(handler) = self.message_handlers.read().unwrap().get(&e) {
            handler.call(self.clone(), data, vec![], ack)?;
        }
        Ok(())
    }

    fn recv_bin_event(
        self: Arc<Self>,
        e: String,
        packet: BinaryPacket,
        ack: Option<i64>,
    ) -> Result<(), Error> {
        if let Some(handler) = self.message_handlers.read().unwrap().get(&e) {
            handler.call(self.clone(), packet.data, packet.bin, ack)?;
        }
        Ok(())
    }

    fn recv_ack(self: Arc<Self>, data: Value, ack: i64) -> Result<(), Error> {
        if let Some(tx) = self.ack_message.write().unwrap().remove(&ack) {
            tx.send((data, vec![])).ok();
        }
        Ok(())
    }

    fn recv_bin_ack(self: Arc<Self>, packet: BinaryPacket, ack: i64) -> Result<(), Error> {
        if let Some(tx) = self.ack_message.write().unwrap().remove(&ack) {
            tx.send((packet.data, packet.bin)).ok();
        }
        Ok(())
    }
}

impl<A: Adapter> Debug for Socket<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socket")
            .field("ns", &self.ns())
            .field("ack_message", &self.ack_message)
            .field("ack_counter", &self.ack_counter)
            .field("handshake", &self.handshake)
            .field("sid", &self.sid)
            .finish()
    }
}

#[cfg(test)]
impl<A: Adapter> Socket<A> {
    pub fn new_dummy(sid: Sid, ns: Arc<Namespace<A>>) -> Socket<A> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                println!("Dummy socket received packet {:?}", packet);
            }
        });
        Socket::new(
            sid,
            ns,
            Handshake::new_dummy(),
            tx,
            Arc::new(SocketIoConfig::default()),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::{Adapter, LocalAdapter};
    use crate::errors::{GoodNameError, SendError};
    use crate::handshake::Handshake;
    use crate::{Namespace, Socket, SocketIoConfig};
    use engineioxide::{sid_generator::Sid, SendPacket as EnginePacket};
    use futures::FutureExt;
    use std::sync::Arc;
    use tokio::sync::mpsc::Receiver;

    impl<A: Adapter> Socket<A> {
        pub fn new_rx_dummy(
            sid: Sid,
            ns: Arc<Namespace<A>>,
        ) -> (Socket<A>, Receiver<EnginePacket>) {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            (
                Socket::new(
                    sid,
                    ns,
                    Handshake::new_dummy(),
                    tx,
                    Arc::new(SocketIoConfig::default()),
                ),
                rx,
            )
        }
    }

    #[tokio::test]
    async fn test_resend() {
        let ns = Namespace::new("/", Arc::new(|_| async move {}.boxed()));
        let (sock, mut rx): (Socket<LocalAdapter>, _) =
            Socket::new_rx_dummy(1i64.into(), ns.clone());
        sock.emit("lol", "\"someString1\"").unwrap();

        let err = sock.emit("lol", "\"someString2\"").unwrap_err();

        let SendError::GoodNameError(GoodNameError::SendMainPacket(packet)) = err else {
          panic!("unexpected err");  
        };
        let err = packet.retry(&sock).unwrap_err();
        rx.recv().await.unwrap();

        let GoodNameError::SendMainPacket(packet) = err else {
            panic!("unexpected err");
        };
        packet.retry(&sock).unwrap();
    }
}
