use crate::kcp::*;
use crate::*;

pub struct KcpStream {
    config: Arc<KcpConfig>,
    conv: u32,
    msg_sink: PollSender<Message>,
    data_rx: Receiver<Data>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum Data {
    Connected { conv: u32 },
    Disconnect(io::ErrorKind),
    Frame(Bytes),
}

#[derive(Debug)]
enum Message {
    Connect,
    Accept { conv: u32 },
    Flush,
    Frame(Bytes),
}

impl KcpStream {
    pub const SYN_CONV: u32 = 0xFFFF_FFFE;

    pub(crate) fn new<S, R, D>(
        config: Arc<KcpConfig>,
        sink: S,
        stream: R,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> Self
    where
        S: Sink<Bytes> + Send + Unpin + 'static,
        S::Error: Display,
        R: Stream + Send + Unpin + 'static,
        R::Item: Into<Bytes> + Send + 'static,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        let token = token.unwrap_or_else(CancellationToken::new);
        let (msg_tx, msg_rx) = channel(config.snd_wnd.max(8) as usize);
        let (data_tx, data_rx) = channel(config.rcv_wnd.max(16) as usize);

        let task = Task::new(
            config.clone(),
            sink,
            stream,
            disconnect,
            msg_rx,
            data_tx,
            token.clone(),
        );
        Self {
            config,
            conv: 0,
            msg_sink: PollSender::new(msg_tx),
            data_rx,
            token,
            task: Some(tokio::spawn(task.run())),
        }
    }

    pub async fn connect<S, R, D>(
        config: Arc<KcpConfig>,
        sink: S,
        stream: R,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> io::Result<Self>
    where
        S: Sink<Bytes> + Send + Unpin + 'static,
        S::Error: Display,
        R: Stream + Send + Unpin + 'static,
        R::Item: Into<Bytes> + Send + 'static,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        Self::new(config.clone(), sink, stream, disconnect, token)
            .wait_connection(Message::Connect)
            .await
    }

    pub async fn accept<S, R, D>(
        config: Arc<KcpConfig>,
        conv: u32,
        sink: S,
        stream: R,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> io::Result<Self>
    where
        S: Sink<Bytes> + Send + Unpin + 'static,
        S::Error: Display,
        R: Stream + Send + Unpin + 'static,
        R::Item: Into<Bytes> + Send + 'static,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        if conv == 0 || conv == Self::SYN_CONV {
            error!("Invalid conv 0x{:08X}", Self::SYN_CONV);
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Self::new(config.clone(), sink, stream, disconnect, token)
            .wait_connection(Message::Accept { conv })
            .await
    }

    #[inline]
    pub fn config(&self) -> Arc<KcpConfig> {
        self.config.clone()
    }

    #[inline]
    pub fn conv(&self) -> u32 {
        self.conv
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.msg_sink
            .send(Message::Flush)
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    /// Generate a random conv.
    pub fn rand_conv() -> u32 {
        loop {
            let conv = rand::random();
            if conv != 0 && conv != Self::SYN_CONV {
                break conv;
            }
        }
    }

    async fn wait_connection(mut self, msg: Message) -> io::Result<Self> {
        self.msg_sink.send(msg).await.unwrap();
        let rst = match tokio::time::timeout(self.config.connect_timeout, self.data_rx.recv()).await
        {
            Ok(Some(Data::Connected { conv })) => {
                trace!("Connect conv {}", conv);
                self.conv = conv;
                return Ok(self);
            }
            Ok(Some(Data::Disconnect(err))) => {
                trace!("Disconnect conv {}: {}", self.conv, err);
                Err(err.into())
            }
            Ok(_) => Err(io::ErrorKind::NotConnected.into()),
            _ => Err(io::ErrorKind::TimedOut.into()),
        };
        self.close().await.ok();
        rst
    }

    fn try_close(&mut self) {
        if !self.token.is_cancelled() {
            self.token.cancel();
            self.data_rx.close();
        }
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        self.try_close();
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Copy, Default, AsBytes, FromBytes)]
#[repr(C)]
struct SynData {
    key: KcpSessionKey,
    id: KcpSessionKey,
}

struct Task<S, R, D> {
    kcp: Kcp,
    config: Arc<KcpConfig>,
    sink: S,
    stream: R,
    disconnect: D,
    msg_rx: Receiver<Message>,
    data_tx: Sender<Data>,
    token: CancellationToken,
    rx_buf: BytesMut,
    tx_frame: Option<Bytes>,

    states: u32,
    last_io_time: u32,
    session_id: KcpSessionKey,
}

impl<S, R, D> Task<S, R, D> {
    const RESET: u32 = 0x04;
    const CLOSED: u32 = 0x08;

    const SYN_HANDSHAKE: u32 = 0x01;
    const CLIENT: u32 = 0x40;
    const FLUSH: u32 = 0x80;

    const CMD_MASK: u8 = 0x57;
    const KCP_SYN: u8 = 0x80;
    const KCP_FIN: u8 = 0x20;
    const KCP_RESET: u8 = 0x08;

    const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);
}

impl<S, R, D> Task<S, R, D>
where
    S: Sink<Bytes> + Unpin,
    S::Error: Display,
    R: Stream + Unpin,
    R::Item: Into<Bytes>,
    D: Sink<u32> + Send + Unpin,
{
    fn new(
        config: Arc<KcpConfig>,
        sink: S,
        stream: R,
        disconnect: D,
        msg_rx: Receiver<Message>,
        data_tx: Sender<Data>,
        token: CancellationToken,
    ) -> Self {
        Self {
            kcp: Kcp::new(0),
            config,
            sink,
            stream,
            disconnect,
            msg_rx,
            data_tx,
            token,
            rx_buf: BytesMut::new(),
            tx_frame: None,
            states: 0,
            last_io_time: 0,
            session_id: Default::default(),
        }
    }

    async fn run(mut self) {
        self.kcp.initialize();
        self.kcp_apply_config();
        // Prepare for SYN handshake.
        self.kcp.set_nodelay(true, 100, 0, false);
        self.last_io_time = self.kcp.current();

        let data_tx = self.data_tx.clone();
        loop {
            if self.kcp.has_ouput() {
                select! {
                    biased;
                    _ = Self::kcp_output(&mut self.kcp, &mut self.sink) => (),
                    _ = self.token.cancelled() => break,
                }
            }

            if self.kcp.is_dead_link()
                || self.kcp.duration_since(self.last_io_time)
                    >= (self.config.session_expire.as_secs() * 1000) as u32
            {
                // TODO:
                break;
            }

            let current = self.kcp.get_system_time();
            let interval = self.kcp.check(current).wrapping_sub(current).min(2000);
            if interval == 0 {
                self.kcp.update(current);
                continue;
            }

            select! {
                Some(msg) = self.msg_rx.recv(), if !self.kcp.is_send_queue_full() => {
                    self.process_msg(msg);
                    // Try to process more.
                    while !self.kcp.is_send_queue_full() {
                        match self.msg_rx.try_recv() {
                            Ok(msg) => self.process_msg(msg),
                            _ => break,
                        }
                    }
                    // Try to flush.
                    self.kcp_flush();
                }

                Ok(permit) = data_tx.reserve(), if self.tx_frame.is_some() => {
                    if let Some(data) = self.tx_frame.take() {
                        permit.send(Data::Frame(data));
                    }
                    while let Some(data) = self.kcp.recv_bytes() {
                        match data_tx.try_reserve() {
                            Ok(permit) => permit.send(Data::Frame(data)),
                            _ => {
                                // Save the data frame.
                                self.tx_frame = Some(data);
                                break;
                            }
                        }
                    }
                }

                x = self.stream.next() => {
                    match x {
                         Some(data) => {
                            self.kcp_input(data.into());
                            // Try to receive more.
                            poll_fn(|cx| {
                                for _ in 1..self.config.rcv_wnd {
                                    match self.stream.poll_next_unpin(cx) {
                                        Poll::Ready(Some(data)) => self.kcp_input(data.into()),
                                        _ => break,
                                    }
                                }
                                Poll::Ready(())
                            }).await;

                            // SYN handshake.
                            if self.test_state(Self::SYN_HANDSHAKE) {
                                self.syn_handshake_recv();
                            }

                            // Try to fetch a frame.
                            if self.tx_frame.is_none() {
                                self.tx_frame = self.kcp.recv_bytes();
                            }
                            // Try to flush.
                            self.kcp_flush();
                         }
                         _ => break,
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(interval as u64)) => (),
                _ = self.token.cancelled() => break,
            }
        }

        self.msg_rx.close();

        // Report disconnect.
        if tokio::time::timeout(
            Self::DISCONNECT_TIMEOUT,
            self.disconnect.send(self.kcp.conv()),
        )
        .await
        .is_err()
        {
            error!(
                "timeout to send disonnect message, conv {}",
                self.kcp.conv()
            );
        }
    }

    #[inline]
    fn test_state(&self, state: u32) -> bool {
        self.states & state != 0
    }

    fn syn_handshake_send(&mut self) {
        self.states |= Self::FLUSH;
        self.kcp
            .send(
                SynData {
                    key: self.config.session_key,
                    id: self.session_id,
                }
                .as_bytes(),
            )
            .unwrap();
    }

    fn syn_handshake_recv(&mut self) {
        // Check SYN data.
        while let Some(data) = self.kcp.recv_bytes() {
            // Only receive SYN frame for the server endpoint.
            if let Some(syn) = SynData::read_from(data.deref()) {
                // Key must be consistent.
                if self.config.session_key == syn.key {
                    if self.test_state(Self::CLIENT) {
                        // Verify the session ID for the client endpoint.
                        if self.session_id != syn.id {
                            continue;
                        }
                    } else {
                        self.session_id = syn.id;
                        self.syn_handshake_send();
                    }

                    // Connection has been established.
                    self.states &= !Self::SYN_HANDSHAKE;
                    self.kcp_apply_nodelay();
                    self.data_tx
                        .try_send(Data::Connected {
                            conv: self.kcp.conv(),
                        })
                        .unwrap();
                    break;
                }
            }
        }
    }

    fn syn_handshake_input(&mut self, data: Bytes) -> io::Result<()> {
        // Switch kcp's conv to try to accept the data packet.
        if let Some(conv) = Kcp::read_conv(data.deref()) {
            if self.test_state(Self::CLIENT) {
                if self.test_state(Self::SYN_HANDSHAKE) && conv != KcpStream::SYN_CONV {
                    // For the client endpoint.
                    // Try to accept conv from the server.
                    self.kcp.set_conv(conv);
                    if self.kcp.input(&data).is_err() || self.kcp.get_waitsnd() > 0 {
                        // Restore conv if failed.
                        self.kcp.set_conv(KcpStream::SYN_CONV);
                    }
                }
                return Ok(());
            } else if conv == KcpStream::SYN_CONV {
                // For the server endpoint.
                let mine = self.kcp.conv();
                // Switch conv temporarily.
                self.kcp.set_conv(conv);
                self.kcp.input(&data).ok();
                // Restore conv.
                self.kcp.set_conv(mine);
                return Ok(());
            }
        }
        Err(io::ErrorKind::InvalidInput.into())
    }

    fn process_msg(&mut self, msg: Message) {
        match msg {
            Message::Connect => {
                self.states = Self::SYN_HANDSHAKE | Self::CLIENT;
                // Create random session ID for client.
                self.session_id = rand::random();
                // Send SYN packet.
                self.kcp.set_conv(KcpStream::SYN_CONV);
                self.syn_handshake_send();
            }
            Message::Accept { conv } => {
                self.states = Self::SYN_HANDSHAKE;
                self.kcp.set_conv(conv);
            }
            Message::Flush => self.states |= Self::FLUSH,
            Message::Frame(data) => match self.kcp.send(data.deref()) {
                Ok(_) => {
                    // Flush if no delay or the number of not-sent buffers > 1.
                    if self.config.nodelay.nodelay || self.kcp.nsnd_que() > 1 {
                        self.states |= Self::FLUSH;
                    }
                }
                _ => error!(
                    "Too big frame size: {} > {}",
                    data.len(),
                    Kcp::max_frame_size(self.config.mtu)
                ),
            },
        }
    }

    fn kcp_apply_config(&mut self) {
        self.kcp.set_stream(self.config.stream);
        self.kcp.set_mtu(self.config.mtu).unwrap();
        self.kcp
            .set_wndsize(self.config.snd_wnd, self.config.rcv_wnd);
        self.kcp_apply_nodelay();

        // Resize buffer.
        let size = self.config.mtu as usize * 3;
        if self.rx_buf.len() < size {
            self.rx_buf.resize(size, 0);
        }
    }

    fn kcp_apply_nodelay(&mut self) {
        self.kcp.set_nodelay(
            self.config.nodelay.nodelay,
            self.config.nodelay.interval,
            self.config.nodelay.resend,
            self.config.nodelay.nc,
        );
    }

    fn kcp_flush(&mut self) {
        if self.test_state(Self::FLUSH) {
            self.states ^= Self::FLUSH;
            self.kcp.update(self.kcp.get_system_time());
            self.kcp.flush();
            self.last_io_time = self.kcp.current();
        }
    }

    fn kcp_input(&mut self, data: Bytes) {
        match self.kcp.input(&data) {
            Ok(_) => self.states |= Self::FLUSH,
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    // SYN handshake.
                    if self.syn_handshake_input(data).is_ok() {
                        return;
                    }
                    trace!(
                        "conv does not match: {} != {}",
                        Kcp::read_conv(&self.rx_buf).unwrap_or(0),
                        self.kcp.conv()
                    );
                }
                io::ErrorKind::InvalidData => {
                    trace!("packet parse error");
                }
                _ => unreachable!(),
            },
        }
    }

    async fn kcp_output(kcp: &mut Kcp, sink: &mut S) {
        while let Some(data) = kcp.pop_output() {
            if let Err(e) = sink.feed(data).await {
                // clear all output buffers on error
                while kcp.pop_output().is_some() {}
                warn!("send to sink: {}", e);
                break;
            }
        }
        sink.flush().await.ok();
    }
}

impl Stream for KcpStream {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Some(data) = ready!(self.data_rx.poll_recv(cx)) {
            match data {
                Data::Disconnect(err) => {
                    trace!("Disconnect conv {}: {}", self.conv, err);
                }
                Data::Frame(data) => return Poll::Ready(Some(data)),
                _ => (),
            }
        }
        Poll::Ready(None)
    }
}

impl Sink<Bytes> for KcpStream {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut()
            .msg_sink
            .poll_ready_unpin(cx)
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.get_mut()
            .msg_sink
            .start_send_unpin(Message::Frame(item))
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let _ = ready!(this.msg_sink.poll_close_unpin(cx));
        this.try_close();
        if let Some(task) = this.task.as_mut() {
            let _ = ready!(task.poll_unpin(cx));
            this.task.take();
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message() {
        println!(
            "{} {} {}",
            std::mem::size_of::<Data>(),
            std::mem::size_of::<Bytes>(),
            std::mem::size_of::<SocketAddr>(),
        );
    }

    #[tokio::test]
    async fn test_stream() {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace"))
            .format_timestamp_micros()
            .is_test(true)
            .try_init()
            .ok();

        let addr1: SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:4322".parse().unwrap();

        let (sink, stream) =
            UdpFramed::new(UdpSocket::bind(addr1).await.unwrap(), BytesCodec::new()).split();
        let stream1 = stream.filter_map(|x| ready(x.ok().map(|(x, _)| x)));
        let sink1 = sink.with(move |x: Bytes| ready(io::Result::Ok((x, addr2))));

        let udp2 = Arc::new(UdpSocket::bind(addr2).await.unwrap());
        let stream2 = UdpFramed::new(udp2.clone(), BytesCodec::new())
            .filter_map(|x| ready(x.ok().map(|(x, _)| x)));
        let sink2 = UdpFramed::new(udp2, BytesCodec::new())
            .with(move |x: Bytes| ready(io::Result::Ok((x, addr1))));

        let config = Arc::new(KcpConfig {
            nodelay: KcpNoDelayConfig::normal(),
            session_key: rand::random(),
            ..Default::default()
        });

        let (s1, s2) = tokio::join!(
            KcpStream::accept(
                config.clone(),
                KcpStream::rand_conv(),
                sink1,
                stream1,
                futures::sink::drain(),
                None,
            ),
            KcpStream::connect(config, sink2, stream2, futures::sink::drain(), None),
        );
        let mut s1 = Box::new(s1.unwrap());
        let mut s2 = Box::new(s2.unwrap());

        s1.send(Bytes::from_static(b"12345")).await.unwrap();
        println!("{:?}", s2.next().await);

        let frame = Bytes::from(vec![0u8; 300000]);
        let start = std::time::Instant::now();
        let mut received = 0;
        while start.elapsed() < Duration::from_secs(10) {
            select! {
                _ = s1.send(frame.clone()) => (),
                Some(x) = s2.next() => {
                    //trace!("received {}", x.len());
                    received += x.len();
                }
            }
        }
        error!("total received {}", received);

        s2.flush().await.unwrap();
        s1.close().await.unwrap();
        s2.close().await.unwrap();
    }
}
