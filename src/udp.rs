use crate::protocol::Kcp;
pub use crate::stream::*;

use ::bytes::Bytes;
use ::futures::{
    future::{poll_fn, ready},
    SinkExt, Stream, StreamExt,
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
    sync::mpsc::{channel, Receiver, Sender},
    task::JoinHandle,
};
use ::tokio_stream::wrappers::ReceiverStream;
use ::tokio_util::{
    codec::BytesCodec,
    sync::{CancellationToken, PollSendError, PollSender},
    udp::UdpFramed,
};

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
        let (msg_tx, msg_rx) = channel(backlog);
        let (data_tx, data_rx) = channel(Task::DATA_QUEUE_SIZE);
        let task = Task::new(
            config.clone(),
            backlog,
            stream_tx,
            msg_tx,
            data_tx,
            token.clone(),
        );
        Ok(Self {
            config,
            stream_rx,
            token,
            task: Some(tokio::spawn(task.run(udp, msg_rx, data_rx))),
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

        let (sink, stream) = UdpFramed::new(udp, BytesCodec::new()).split();
        let sink = sink.with(move |x: Bytes| ready(io::Result::Ok((x, addr))));
        let stream = stream.filter_map(|x| ready(x.ok().map(|(x, _)| x)));

        KcpStream::connect(config, sink, stream, futures::sink::drain(), None)
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

struct RxData {
    conv: u32,
    data: Bytes,
}

struct Session {
    conv: u32,
    session_id: KcpSessionKey,
    peer_addr: SocketAddr,
    sender: Sender<Bytes>,
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
    msg_tx: Sender<Message>,
    data_tx: Sender<RxData>,
    token: CancellationToken,

    rx_queue: VecDeque<(Bytes, SocketAddr)>,
    conv_map: LinkedHashMap<u32, Session>,
    sid_map: LinkedHashMap<KcpSessionKey, u32>,
    accept_task_count: usize,
}

macro_rules! tri {
    ($value:expr) => {
        match $value {
            Some(x) => x,
            _ => break,
        }
    };
}

impl Task {
    const DATA_QUEUE_SIZE: usize = 2048;
    const DATA_QUEUE_LOOP: usize = 1024;

    fn new(
        config: Arc<KcpConfig>,
        backlog: usize,
        stream_tx: Sender<(KcpStream, SocketAddr)>,
        msg_tx: Sender<Message>,
        data_tx: Sender<RxData>,
        token: CancellationToken,
    ) -> Self {
        Self {
            config,
            backlog,
            stream_tx,
            msg_tx,
            data_tx,
            token,
            rx_queue: VecDeque::with_capacity(Self::DATA_QUEUE_LOOP),
            conv_map: LinkedHashMap::new(),
            sid_map: LinkedHashMap::new(),
            accept_task_count: 0,
        }
    }

    async fn run(
        mut self,
        udp: UdpSocket,
        mut msg_rx: Receiver<Message>,
        mut data_rx: Receiver<RxData>,
    ) {
        let (mut udp_sink, mut udp_stream) = UdpFramed::new(udp, BytesCodec::new()).split();
        let token = self.token.clone();

        loop {
            select! {
                Some(x) = udp_stream.next(), if self.rx_queue.is_empty() => {
                    if let Ok((data, addr)) = x {
                        self.rx_queue.push_back((data.into(), addr));
                    }
                    // Try to receive more.
                    poll_fn(|cx| {
                        for _ in 0..self.rx_queue.capacity() - self.rx_queue.len() {
                            match udp_stream.poll_next_unpin(cx) {
                                Poll::Ready(Some(x)) => {
                                    if let Ok((data, addr)) = x {
                                        self.rx_queue.push_back((data.into(), addr));
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
                                // TODO:
                                select! {
                                    _ = self.stream_tx.send((stream, session.peer_addr)) => (),
                                    _ = token.cancelled() => (),
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

                Some(RxData { conv, data }) = data_rx.recv() => {
                    if let Some(session) = self.conv_map.get(&conv) {
                        let _ = udp_sink.feed((data, session.peer_addr)).await;
                    }
                    // Try send more.
                    for _ in 0..Self::DATA_QUEUE_LOOP {
                        match data_rx.try_recv() {
                            Ok(RxData { conv, data }) => if let Some(session) = self.conv_map.get(&conv) {
                                let _ = udp_sink.feed((data, session.peer_addr)).await;
                            },
                            _ => break,
                        }
                    }
                    let _ = udp_sink.flush().await;
                }

                _ = token.cancelled() => break,
            }
        }

        msg_rx.close();
        data_rx.close();

        // Close all sessions.
        while let Some((_, session)) = self.conv_map.pop_front() {
            self.remove_session(session).await;
        }
    }

    /// This function is cancel-safe.
    async fn recv_packet(&mut self) {
        while let Some((packet, peer_addr)) = self.rx_queue.front() {
            #[allow(clippy::never_loop)]
            while let Some(pkt_conv) = Kcp::read_conv(packet) {
                let mut session = self.conv_map.get(&pkt_conv);
                if session.is_none() {
                    // Try to accept a new connection.
                    let session_id = tri!(tri!(Kcp::read_payload_data(packet))
                        .strip_prefix(&self.config.session_key)
                        .and_then(|x| KcpSessionKey::try_from(x).ok()));

                    let conv = if let Some(&conv) = self.sid_map.get(&session_id) {
                        if conv != pkt_conv && pkt_conv != KcpStream::SYN_CONV {
                            // conv is inconsistent.
                            break;
                        }
                        conv
                    } else {
                        if pkt_conv != KcpStream::SYN_CONV {
                            // It's not a SYN handshake packet.
                            break;
                        }
                        if self.stream_tx.try_reserve().is_err()
                            || self.accept_task_count >= self.backlog
                        {
                            // TODO: No space to store a new connection.
                            break;
                        }

                        // TODO: new conv
                        let conv = loop {
                            let x = KcpStream::rand_conv();
                            if !self.conv_map.contains_key(&x) {
                                break x;
                            }
                        };

                        let (sender, receiver) = channel(self.config.snd_wnd as usize);
                        let token = self.token.child_token();

                        self.sid_map.insert(session_id, conv);
                        self.conv_map.insert(
                            conv,
                            Session {
                                conv,
                                session_id,
                                peer_addr: *peer_addr,
                                sender,
                                token: token.clone(),
                                task: Some(tokio::spawn(Self::accept_stream(
                                    self.config.clone(),
                                    conv,
                                    receiver,
                                    self.data_tx.clone(),
                                    self.msg_tx.clone(),
                                    token,
                                ))),
                            },
                        );
                        self.accept_task_count += 1;
                        conv
                    };
                    session = self.conv_map.get(&conv);
                }

                if let Some(session) = session {
                    if &session.peer_addr == peer_addr {
                        let _ = session.sender.send(packet.clone()).await;
                    }
                }
                break;
            }

            // Remove the packet.
            self.rx_queue.pop_front();
        }
    }

    async fn accept_stream(
        config: Arc<KcpConfig>,
        conv: u32,
        receiver: Receiver<Bytes>,
        data_tx: Sender<RxData>,
        msg_tx: Sender<Message>,
        token: CancellationToken,
    ) {
        let sink = PollSender::new(data_tx)
            .with(move |x: Bytes| ready(Ok::<_, PollSendError<_>>(RxData { conv, data: x })));
        let disconnect = PollSender::new(msg_tx.clone())
            .with(move |conv: u32| ready(Ok::<_, PollSendError<_>>(Message::Disconnect { conv })));
        let stream = ReceiverStream::new(receiver);
        if let Ok(stream) =
            KcpStream::accept(config, conv, sink, stream, disconnect, Some(token)).await
        {
            let _ = msg_tx.send(Message::Connect(stream)).await;
        }
    }

    async fn remove_session(&mut self, mut session: Session) {
        self.sid_map.remove(&session.session_id);
        if let Some(task) = session.task.take() {
            session.token.cancel();
            self.accept_task_count -= 1;
            let _ = task.await;
        }
    }
}
