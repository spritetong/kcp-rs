pub use crate::stream::*;
use crate::transport::*;

use ::bytes::{Bytes, BytesMut};
use ::futures::{
    future::{poll_fn, ready},
    Sink, SinkExt, Stream, StreamExt,
};
use ::hashlink::LinkedHashMap;
use ::std::{
    collections::VecDeque,
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
    ) -> io::Result<Self> {
        let udp = UdpSocket::bind(addr).await?;
        Self::socket_listen(config, udp, backlog)
    }

    pub fn socket_listen(
        config: Arc<KcpConfig>,
        udp: UdpSocket,
        backlog: usize,
    ) -> io::Result<Self> {
        let token = CancellationToken::new();
        let backlog = backlog.max(8);
        let (stream_tx, stream_rx) = channel(backlog);
        let (msg_tx, msg_rx) = unbounded_channel();
        let (pkt_tx, pkt_rx) = unbounded_channel();
        let task = Task::new(
            config.clone(),
            backlog,
            stream_tx,
            msg_tx,
            pkt_tx,
            token.clone(),
        );
        Ok(Self {
            config,
            stream_rx,
            token,
            task: Some(tokio::spawn(task.run(udp, msg_rx, pkt_rx))),
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
            UdpTransport::new(udp, addr),
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
    backlog: usize,
    stream_tx: Sender<(KcpStream, SocketAddr)>,
    msg_tx: UnboundedSender<Message>,
    pkt_tx: UnboundedSender<(Bytes, SocketAddr)>,
    token: CancellationToken,

    rx_queue: VecDeque<(BytesMut, SocketAddr)>,
    conv_map: LinkedHashMap<u32, Session>,
    sid_map: LinkedHashMap<Bytes, u32>,
    accept_task_count: usize,
}

impl Task {
    const PACKET_QUEUE_LOOP: usize = 1024;

    fn new(
        config: Arc<KcpConfig>,
        backlog: usize,
        stream_tx: Sender<(KcpStream, SocketAddr)>,
        msg_tx: UnboundedSender<Message>,
        pkt_tx: UnboundedSender<(Bytes, SocketAddr)>,
        token: CancellationToken,
    ) -> Self {
        Self {
            config,
            backlog,
            stream_tx,
            msg_tx,
            pkt_tx,
            token,
            rx_queue: VecDeque::with_capacity(Self::PACKET_QUEUE_LOOP),
            conv_map: LinkedHashMap::new(),
            sid_map: LinkedHashMap::new(),
            accept_task_count: 0,
        }
    }

    async fn run(
        mut self,
        udp: UdpSocket,
        mut msg_rx: UnboundedReceiver<Message>,
        mut pkt_rx: UnboundedReceiver<(Bytes, SocketAddr)>,
    ) {
        let (mut udp_sink, mut udp_stream) = UdpFramed::new(udp, BytesCodec::new()).split();
        let token = self.token.clone();

        loop {
            select! {
                Some(x) = udp_stream.next(), if self.rx_queue.is_empty() => {
                    if let Ok(x) = x {
                        self.rx_queue.push_back(x);
                    }
                    // Try to receive more.
                    poll_fn(|cx| {
                        for _ in 0..self.rx_queue.capacity() - self.rx_queue.len() {
                            match udp_stream.poll_next_unpin(cx) {
                                Poll::Ready(Some(x)) => {
                                    if let Ok(x) = x {
                                        self.rx_queue.push_back(x);
                                    }
                                }
                                _ => break,
                            }
                        }
                        Poll::Ready(())
                    }).await;
                }

                _ = self.recv_packet(), if !self.rx_queue.is_empty() => (),

                Some(msg) = msg_rx.recv() => {
                    match msg {
                        Message::Connect(stream) => {
                            if let Some(session) = self.conv_map.get_mut(&stream.conv()) {
                                if let Some(task) = session.task.take() {
                                    self.accept_task_count -= 1;
                                    let _ = task.await;
                                }
                                if let Some(permit) = session.stream_permit.take() {
                                    permit.send((stream, session.peer_addr));
                                }
                            }
                        }
                        Message::Disconnect { conv } => {
                            if let Some(session) = self.conv_map.remove(&conv) {
                                self.remove_session(session).await;
                            }
                        }
                    }
                }

                Some(item) = pkt_rx.recv() => {
                    let _ = udp_sink.feed(item).await;
                    // Try send more.
                    Self::send_pending_packets(&mut pkt_rx, &mut udp_sink, Self::PACKET_QUEUE_LOOP).await;
                }

                _ = token.cancelled() => break,
            }
        }

        msg_rx.close();
        Self::send_pending_packets(&mut pkt_rx, &mut udp_sink, usize::MAX).await;

        // Close all sessions.
        while let Some((_, session)) = self.conv_map.pop_front() {
            self.remove_session(session).await;
        }

        pkt_rx.close();
        Self::send_pending_packets(&mut pkt_rx, &mut udp_sink, usize::MAX).await;
    }

    async fn send_pending_packets<S: Sink<(Bytes, SocketAddr)> + Unpin>(
        pkt_rx: &mut UnboundedReceiver<(Bytes, SocketAddr)>,
        udp_sink: &mut S,
        max: usize,
    ) {
        for _ in 0..max {
            match pkt_rx.try_recv() {
                Ok(item) => {
                    let _ = udp_sink.feed(item).await;
                }
                _ => break,
            }
        }
        let _ = udp_sink.flush().await;
    }

    /// This function is cancel-safe.
    async fn recv_packet(&mut self) {
        while let Some((packet, peer_addr)) = self.rx_queue.front() {
            #[allow(clippy::never_loop)]
            let session = loop {
                // Find session by conv.
                let pkt_conv = match KcpStream::read_conv(packet) {
                    Some(x) => x,
                    _ => break None,
                };
                if let x @ Some(_) = self.conv_map.get(&pkt_conv) {
                    break x;
                }

                // Try to accept a new connection.
                let session_id = match KcpStream::read_session_id(packet, &self.config.session_key)
                {
                    Some(x) => x,
                    _ => break None,
                };

                let conv = if let Some(&conv) = self.sid_map.get(session_id) {
                    if conv != pkt_conv && pkt_conv != KcpStream::SYN_CONV {
                        // conv is inconsistent.
                        break None;
                    }
                    conv
                } else {
                    if pkt_conv != KcpStream::SYN_CONV
                        || session_id.len() != self.config.session_id_len
                    {
                        // It's not a SYN handshake packet.
                        break None;
                    }

                    // Get permit for the backlog limitation.
                    let stream_permit = match self.stream_tx.clone().try_reserve_owned() {
                        Ok(x) => x,
                        _ => break None,
                    };

                    // TODO: new conv
                    let conv = loop {
                        let x = KcpStream::rand_conv();
                        if !self.conv_map.contains_key(&x) {
                            break x;
                        }
                    };

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
                    self.accept_task_count += 1;
                    conv
                };
                break self.conv_map.get(&conv);
            };

            // Remove the packet.
            if let Some(session) = session {
                if &session.peer_addr == peer_addr {
                    let _ = session.sender.send(packet.clone()).await;
                }
            }

            self.rx_queue.pop_front();
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
            UdpMpscTransport::new(Some(pkt_tx), receiver, peer_addr),
            disconnect,
            Some(token),
        )
        .await
        {
            let _ = msg_tx.send(Message::Connect(stream));
        }
    }

    async fn remove_session(&mut self, mut session: Session) {
        self.sid_map.remove(&session.session_id);
        if let Some(task) = session.task.take() {
            self.accept_task_count -= 1;
            session.token.cancel();
            let _ = task.await;
        }
    }
}
