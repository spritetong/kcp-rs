use crate::protocol::Kcp;
use crate::transport::*;
use crate::{conv::ConvCache, stream::*};

use ::bytes::{Bytes, BytesMut};
use ::futures::{
    future::{poll_immediate, ready},
    Sink, SinkExt, Stream, StreamExt,
};
use ::hashlink::LinkedHashMap;
use ::std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use ::tokio::{
    net::{lookup_host, ToSocketAddrs, UdpSocket},
    select,
    sync::mpsc::{
        channel, unbounded_channel, OwnedPermit, Receiver, Sender, UnboundedReceiver,
        UnboundedSender,
    },
    task::JoinHandle,
};
use ::tokio_util::{codec::BytesCodec, sync::CancellationToken, udp::UdpFramed};

pub struct KcpUdpStream {
    config: Arc<KcpConfig>,
    stream_rx: Receiver<(KcpStream, SocketAddr)>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl KcpUdpStream {
    pub async fn listen<A: ToSocketAddrs>(
        config: Arc<KcpConfig>,
        addr: A,
        backlog: usize,
        conv_cache: Option<ConvCache>,
    ) -> io::Result<Self> {
        let udp = UdpSocket::bind(addr).await?;
        Self::socket_listen(config, udp, backlog, conv_cache)
    }

    pub fn socket_listen(
        config: Arc<KcpConfig>,
        udp: UdpSocket,
        backlog: usize,
        conv_cache: Option<ConvCache>,
    ) -> io::Result<Self> {
        let token = CancellationToken::new();
        let (stream_tx, stream_rx) = channel(backlog.max(8));
        let task = Task::new(config.clone(), conv_cache, stream_tx, token.clone());
        Ok(Self {
            config,
            stream_rx,
            token,
            task: Some(tokio::spawn(task.run(udp))),
        })
    }

    pub async fn accept(&mut self) -> io::Result<(KcpStream, SocketAddr)> {
        self.stream_rx
            .recv()
            .await
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }

    pub async fn close(&mut self) -> io::Result<()> {
        if let Some(task) = self.task.take() {
            self.token.cancel();
            self.stream_rx.close();
            let _ = task.await;
        }
        Ok(())
    }
}

impl KcpUdpStream {
    pub async fn connect<A: ToSocketAddrs>(
        config: Arc<KcpConfig>,
        addr: A,
    ) -> io::Result<(KcpStream, SocketAddr)> {
        let addr = lookup_host(addr)
            .await?
            .next()
            .ok_or(io::ErrorKind::AddrNotAvailable)?;

        let local_addr: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let udp = UdpSocket::bind(local_addr).await?;

        Self::socket_connect(config, addr, udp).await
    }

    pub async fn socket_connect<A: ToSocketAddrs>(
        config: Arc<KcpConfig>,
        addr: A,
        udp: UdpSocket,
    ) -> io::Result<(KcpStream, SocketAddr)> {
        let addr = lookup_host(addr)
            .await?
            .next()
            .ok_or(io::ErrorKind::AddrNotAvailable)?;

        KcpStream::connect::<_, BytesMut, _>(
            config,
            UdpStream::new(udp, addr),
            futures::sink::drain(),
            None,
        )
        .await
        .map(|x| (x, addr))
    }
}

impl Drop for KcpUdpStream {
    fn drop(&mut self) {
        self.token.cancel();
        self.stream_rx.close();
    }
}

impl Stream for KcpUdpStream {
    type Item = (KcpStream, SocketAddr);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream_rx.poll_recv(cx)
    }
}

////////////////////////////////////////////////////////////////////////////////

struct Session {
    conv: u32,
    session_id: Bytes,
    peer_addr: SocketAddr,
    sender: Sender<BytesMut>,
    stream_permit: Option<OwnedPermit<(KcpStream, SocketAddr)>>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
}

enum Message {
    Connect(KcpStream),
    Disconnect { conv: u32 },
}

struct Task {
    config: Arc<KcpConfig>,
    conv_cache: ConvCache,
    stream_tx: Sender<(KcpStream, SocketAddr)>,
    msg_tx: UnboundedSender<Message>,
    msg_rx: UnboundedReceiver<Message>,
    pkt_tx: UnboundedSender<(Bytes, SocketAddr)>,
    pkt_rx: UnboundedReceiver<(Bytes, SocketAddr)>,
    token: CancellationToken,
    is_closing: bool,

    conv_map: LinkedHashMap<u32, Session>,
    sid_map: LinkedHashMap<Bytes, u32>,
}

impl Task {
    fn new(
        config: Arc<KcpConfig>,
        conv_cache: Option<ConvCache>,
        stream_tx: Sender<(KcpStream, SocketAddr)>,
        token: CancellationToken,
    ) -> Self {
        let (msg_tx, msg_rx) = unbounded_channel();
        let (pkt_tx, pkt_rx) = unbounded_channel();
        Self {
            config,
            conv_cache: conv_cache.unwrap_or_else(|| ConvCache::new(0, LISTENER_CONV_TIMEOUT)),
            stream_tx,
            msg_tx,
            msg_rx,
            pkt_tx,
            pkt_rx,
            token,
            is_closing: false,
            conv_map: LinkedHashMap::new(),
            sid_map: LinkedHashMap::new(),
        }
    }

    async fn run(mut self, udp: UdpSocket) {
        let mut transport = UdpFramed::new(udp, BytesCodec::new());

        loop {
            if self.is_closing {
                // Try to drain all connection messages.
                match self.msg_rx.try_recv() {
                    Ok(msg) => self.process_msg(msg).await,
                    Err(_) if self.conv_map.is_empty() => break,
                    _ => (),
                }
            }

            select! {
                x = transport.next() => {
                    let mut recved = x;
                    for _ in 0..LISTENER_TASK_LOOP {
                        match recved {
                            Some(Ok((packet, addr))) => {
                                if let Some(session) = self.get_session(&packet, &addr) {
                                    let _ = session.sender.send(packet.clone()).await;
                                }
                            }
                            Some(Err(_)) => break,
                            None => {
                                self.is_closing = true;
                                self.token.cancel();
                                break;
                            }
                        }

                        // Try to receive more.
                        match poll_immediate(transport.next()).await {
                            Some(x) => recved = x,
                            _ => break,
                        }
                    }
                }

                Some(item) = self.pkt_rx.recv() => {
                    let _ = transport.feed(item).await;
                    // Try send more.
                    self.try_send(&mut transport, LISTENER_TASK_LOOP).await;
                }

                Some(msg) = self.msg_rx.recv() => self.process_msg(msg).await,

                _ = self.token.cancelled(), if !self.is_closing => {
                    self.is_closing = true;
                }
            }
        }

        self.msg_rx.close();
        self.pkt_rx.close();
        self.try_send(&mut transport, usize::MAX).await;
    }

    async fn process_msg(&mut self, msg: Message) {
        match msg {
            Message::Connect(stream) => {
                if let Some(session) = self.conv_map.get_mut(&stream.conv()) {
                    if let Some(task) = session.task.take() {
                        let _ = task.await;
                    }
                    if let Some(permit) = session.stream_permit.take() {
                        permit.send((stream, session.peer_addr));
                    }
                }
            }
            Message::Disconnect { conv } => {
                if let Some(session) = self.conv_map.remove(&conv) {
                    self.kill_session(session).await;
                }
            }
        }
    }

    async fn try_send<S: Sink<(Bytes, SocketAddr)> + Unpin>(&mut self, sink: &mut S, max: usize) {
        for _ in 0..max {
            match self.pkt_rx.try_recv() {
                Ok(item) => {
                    let _ = sink.feed(item).await;
                }
                _ => break,
            }
        }
        let _ = sink.flush().await;
    }

    /// Get session by a received packet.
    fn get_session(&mut self, packet: &[u8], peer_addr: &SocketAddr) -> Option<&Session> {
        // Find session by conv.
        let pkt_conv = match Kcp::read_conv(packet) {
            Some(x) => match self.conv_map.get(&x) {
                Some(s) if &s.peer_addr == peer_addr => return self.conv_map.get(&x),
                Some(_) => return None,
                _ => x,
            },
            _ => return None,
        };

        // Try to accept a new connection.
        let session_id = match KcpStream::read_session_id(packet, &self.config.session_key) {
            Some(x) => x,
            _ => return None,
        };

        if let Some(&conv) = self.sid_map.get(session_id) {
            if conv == pkt_conv || pkt_conv == Kcp::SYN_CONV {
                match self.conv_map.get(&conv) {
                    x @ Some(s) if &s.peer_addr == peer_addr => return x,
                    _ => (),
                }
            }
            None
        } else if self.is_closing
            || pkt_conv != Kcp::SYN_CONV
            || session_id.len() != self.config.session_id_len
        {
            // It's not a SYN handshake packet.
            None
        } else {
            // Get permit in the backlog limitation.
            let stream_permit = match self.stream_tx.clone().try_reserve_owned() {
                Ok(x) => x,
                _ => return None,
            };

            // New KCP conv.
            let conv = self.conv_cache.allocate(|x| self.conv_map.contains_key(x));

            let (sender, receiver) = channel(self.config.snd_wnd as usize);
            let token = self.token.child_token();

            let session_id = Bytes::copy_from_slice(session_id);
            self.sid_map.insert(session_id.clone(), conv);
            self.conv_map.insert(
                conv,
                Session {
                    conv,
                    session_id,
                    peer_addr: *peer_addr,
                    sender,
                    token: token.clone(),
                    stream_permit: Some(stream_permit),
                    task: Some(tokio::spawn(Self::accept_stream(
                        self.config.clone(),
                        conv,
                        *peer_addr,
                        receiver,
                        self.pkt_tx.clone(),
                        self.msg_tx.clone(),
                        token,
                    ))),
                },
            );
            self.conv_map.get(&conv)
        }
    }

    async fn accept_stream(
        config: Arc<KcpConfig>,
        conv: u32,
        peer_addr: SocketAddr,
        receiver: Receiver<BytesMut>,
        pkt_tx: UnboundedSender<(Bytes, SocketAddr)>,
        msg_tx: UnboundedSender<Message>,
        token: CancellationToken,
    ) {
        let disconnect = UnboundedSink::new(msg_tx.clone())
            .with(move |conv: u32| ready(Ok::<_, io::Error>(Message::Disconnect { conv })));
        if let Ok(stream) = KcpStream::accept(
            config,
            conv,
            UdpMpscStream::new(Some(pkt_tx), receiver, peer_addr),
            disconnect,
            Some(token),
        )
        .await
        {
            let _ = msg_tx.send(Message::Connect(stream));
        }
    }

    async fn kill_session(&mut self, mut session: Session) {
        // Add the killed conv to cache to avoid conv confliction.
        self.conv_cache.add(session.conv);
        self.sid_map.remove(&session.session_id);
        if let Some(task) = session.task.take() {
            session.token.cancel();
            let _ = task.await;
        }
    }
}
