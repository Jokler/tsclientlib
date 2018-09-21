use std::{mem, ptr};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use {evmap, slog, slog_async, slog_term, tokio};
use bytes::{Bytes, BytesMut};
use futures::{future, Future, Sink, stream, Stream};
use futures::sync::mpsc;
use slog::Drain;
use tokio::codec::BytesCodec;
use tokio::net::{UdpFramed, UdpSocket};

use {Error, Result};
use connection::*;
use connectionmanager::ConnectionManager;
use crypto::EccKeyPrivP256;
use packet_codec::{PacketCodecReceiver, PacketCodecSender};
use packets::{Packet, PacketType};
use resend::DefaultResender;

pub type DataM<CM> = Arc<Mutex<Data<CM>>>;

/// A listener for added and removed connections.
pub trait ConnectionListener<CM: ConnectionManager>: Send {
    /// Called when a new connection is created.
    ///
    /// Return `false` to remain in the list of listeners.
    /// If `true` is returned, this listener will be removed.
    ///
    /// Per default, `false` is returned.
    fn on_connection_created(&mut self, _key: &mut CM::Key,
        _adata: &mut CM::AssociatedData, _con: &mut Connection) -> bool {
        false
    }

    /// Called when a connection is removed.
    ///
    /// Return `false` to remain in the list of listeners.
    /// If `true` is returned, this listener will be removed.
    ///
    /// Per default, `false` is returned.
    fn on_connection_removed(&mut self, _key: &CM::Key,
        _con: &mut ConnectionValue<CM::AssociatedData>) -> bool {
        false
    }
}

/// Will be cloned for some of the incoming packets. So be careful with
/// modifying the internal state, it is not global.
pub trait PacketHandler<T: 'static>: Send {
    /// Called for every new connection.
    ///
    /// The `command_stream` gets all command and init packets.
    /// The `audio_stream` gets all audio packets.
    fn new_connection<S1, S2>(
        &mut self,
        con_val: &ConnectionValue<T>,
        command_stream: S1,
        audio_stream: S2,
    ) where
        S1: Stream<Item=Packet, Error=Error> + Send + 'static,
        S2: Stream<Item=Packet, Error=Error> + Send + 'static;
}

#[derive(Debug)]
pub struct ConnectionValue<T: 'static> {
    pub mutex: Arc<Mutex<(T, Connection)>>,
}

impl<T: Send + 'static> ConnectionValue<T> {
    pub fn new(data: T, con: Connection) -> Self {
        Self { mutex: Arc::new(Mutex::new((data, con))) }
    }

    fn encode_packet(&self, packet: Packet)
        -> Box<Stream<Item=(PacketType, u16, Bytes), Error=Error> + Send> {
        let sink = self.as_udp_packet_sink();
        let mut con = self.mutex.lock().unwrap();
        ::log::PacketLogger::log_packet(&con.1.logger,
            con.1.is_client, false, &packet);

        let codec = PacketCodecSender::new(con.1.is_client, con.1.logger.clone());
        let p_type = packet.header.get_type();

        let mut udp_packets = match codec.encode_packet(&mut con.1, packet) {
            Ok(r) => r,
            Err(e) => return Box::new(stream::once(Err(e))),
        };

        let udp_packets = udp_packets.drain(..).map(|(p_id, p)|
            (p_type, p_id, p)).collect::<Vec<_>>();
        Box::new(stream::iter_ok(udp_packets))
    }

    pub fn as_udp_packet_sink(&self)
        -> ::connection::ConnectionUdpPacketSink<T> {
        ::connection::ConnectionUdpPacketSink::new(self.clone())
    }

    pub fn as_packet_sink(&self) -> impl Sink<SinkItem=Packet, SinkError=Error> {
        let cv = self.clone();
        self.as_udp_packet_sink().with_flat_map(move |p| {
            cv.encode_packet(p)
        })
    }
}

impl<T: 'static> PartialEq for ConnectionValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self as *const ConnectionValue<_> == other as *const _
    }
}

impl<T: 'static> Eq for ConnectionValue<T> {}

impl<T: 'static> Clone for ConnectionValue<T> {
    fn clone(&self) -> Self {
        Self { mutex: self.mutex.clone() }
    }
}

impl<T: 'static> evmap::ShallowCopy for ConnectionValue<T> {
    unsafe fn shallow_copy(&mut self) -> Self {
        // Try to copy without generating memory leaks
        let mut r: Self = mem::uninitialized();
        ptr::copy_nonoverlapping(self, &mut r, 1);
        r
    }
}

/// The stored data for our server or client.
///
/// This handles a single socket.
///
/// The list of connections is not managed by this struct, but it is passed to
/// an instance of [`ConnectionManager`] that is stored here.
///
/// [`ConnectionManager`]: trait.ConnectionManager.html
pub struct Data<CM: ConnectionManager + 'static> {
    /// If this structure is owned by a client or a server.
    pub is_client: bool,
    /// The address of the socket.
    pub local_addr: SocketAddr,
    /// The private key of this instance.
    pub private_key: EccKeyPrivP256,
    pub logger: slog::Logger,

    /// The sink of udp packets.
    pub udp_packet_sink: mpsc::Sender<(SocketAddr, Bytes)>,

    /// The default resend config. It gets copied for each new connection.
    resend_config: ::resend::ResendConfig,

    pub connections: evmap::ReadHandle<CM::Key, ConnectionValue<CM::AssociatedData>>,
    pub connections_writer: evmap::WriteHandle<CM::Key, ConnectionValue<CM::AssociatedData>>,
    pub packet_handler: CM::PacketHandler,

    /// A list of all connected clients or servers
    ///
    /// You should not add or remove connections directly using the manager,
    /// unless you know what you are doing. E.g. connection listeners are not
    /// called if you do so.
    /// Instead, use the [`add_connection`] und [`remove_connection`] function.
    ///
    /// [`add_connection`]: #method.add_connection
    /// [`remove_connection`]: #method.remove_connection
    pub connection_manager: CM,
    /// Listen for new or removed connections.
    pub connection_listeners: Vec<Box<ConnectionListener<CM>>>,
}

impl<CM: ConnectionManager + 'static> Data<CM> {
    /// An optional logger can be provided. If none is provided, a new one will
    /// be created.
    pub fn new<L: Into<Option<slog::Logger>>>(
        local_addr: SocketAddr,
        private_key: EccKeyPrivP256,
        is_client: bool,
        unknown_udp_packet_sink: Option<mpsc::Sender<(SocketAddr, BytesMut)>>,
        packet_handler: CM::PacketHandler,
        connection_manager: CM,
        logger: L,
    ) -> Result<Arc<Mutex<Self>>> {
        let logger = logger.into().unwrap_or_else(|| {
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator).build().fuse();
            let drain = slog_async::Async::new(drain).build().fuse();

            slog::Logger::root(drain, o!())
        });

        // Create the socket
        let socket = UdpSocket::bind(&local_addr)?;
        let local_addr = socket.local_addr().unwrap_or(local_addr);
        debug!(logger, "Listening"; "local_addr" => %local_addr);
        let (sink, stream) = UdpFramed::new(socket, BytesCodec::new()).split();

        let (udp_packet_sink, udp_packet_sink_sender) = mpsc::channel(::UDP_SINK_CAPACITY);
        let logger2 = logger.clone();

        let (connections, connections_writer) = evmap::new();

        let data = Self {
            is_client,
            local_addr,
            private_key,
            logger,
            udp_packet_sink,
            resend_config: Default::default(),
            connections,
            connections_writer,
            packet_handler,
            connection_manager,
            connection_listeners: Vec::new(),
        };

        let logger = data.logger.clone();
        let is_client = data.is_client;
        tokio::spawn(udp_packet_sink_sender.map(move |(addr, p)| {
                ::log::PacketLogger::log_udp_packet(&logger, addr, is_client,
                    false, &::packets::UdpPacket(&p));
                (p, addr)
            })
            .forward(sink.sink_map_err(move |e|
                error!(logger2, "Failed to send udp packet"; "error" => ?e))
            ).map(|_| ()));

        // Handle incoming packets
        let logger = data.logger.clone();
        let logger2 = logger.clone();
        let mut codec = PacketCodecReceiver::new(&data, unknown_udp_packet_sink);
        tokio::spawn(stream
            .map_err(move |e| error!(logger2, "Packet stream errored";
                "error" => ?e))
            .for_each(move |(p, a)| {
                let logger = logger.clone();
                codec.handle_udp_packet((a, p)).then(move |r| {
                    if let Err(e) = r {
                        warn!(logger, "Packet receiver errored"; "error" => ?e);
                    }
                    Ok(())
                })
            }));
        Ok(Arc::new(Mutex::new(data)))
    }

    /// Add a new connection to this socket.
    pub fn add_connection(
        &mut self,
        data_mut: Arc<Mutex<Self>>,
        mut data: CM::AssociatedData,
        addr: SocketAddr,
    ) -> CM::Key {
        // Add options like ip to logger
        let logger = self.logger.new(o!("addr" => addr.to_string()));
        let resender = DefaultResender::new(self.resend_config.clone(),
            logger.clone());
        // Use an unbounded channel so try_send never fails
        let (command_send, command_recv) = mpsc::unbounded();
        let (audio_send, audio_recv) = mpsc::unbounded();
        let mut con = Connection::new(addr, resender, logger.clone(),
            self.udp_packet_sink.clone(), self.is_client, command_send,
            audio_send);

        let mut key = self.connection_manager.new_connection_key(&mut data,
            &mut con);
        // Call listeners
        let mut i = 0;
        while i < self.connection_listeners.len() {
            if self.connection_listeners[i].on_connection_created(
                &mut key, &mut data, &mut con) {
                self.connection_listeners.remove(i);
            } else {
                i += 1;
            }
        }

        let con_val = ConnectionValue::new(data, con);
        self.packet_handler.new_connection(
            &con_val,
            command_recv.map_err(|_| format_err!("Failed to receive").into()),
            audio_recv.map_err(|_| format_err!("Failed to receive").into()),
        );

        // Add connection
        self.connections_writer.insert(key.clone(), con_val);
        self.connections_writer.refresh();

        // Start resender
        let resend_fut = ::resend::ResendFuture::new(&self, data_mut, key.clone());
        ::tokio::spawn(resend_fut.map_err(move |e| {
            error!(logger, "Resend future failed"; "error" => ?e);
        }));

        key
    }

    pub fn remove_connection(&mut self, key: &CM::Key)
        -> Option<ConnectionValue<CM::AssociatedData>> {
        // Get connection
        let mut res = self.get_connection(key);
        if let Some(con) = &mut res {
            // Remove connection
            self.connections_writer.clear(key.clone());
            self.connections_writer.refresh();

            // Call listeners
            let mut i = 0;
            while i < self.connection_listeners.len() {
                if self.connection_listeners[i].on_connection_removed(&key, con) {
                    self.connection_listeners.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        res
    }

    pub fn get_connection(&self, key: &CM::Key) -> Option<ConnectionValue<CM::AssociatedData>> {
        self.connections.get_and(&key, |v| v[0].clone())
    }
}
