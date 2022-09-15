use crate::kcp::*;
use crate::*;
use futures::stream::TryStreamExt;
use std::fmt::Display;
use bytestring::ByteString;
use futures::future::poll_fn;

pub struct KcpStream {
    config: Arc<KcpConfig>,
    peer_addr: ByteString,
    conv: u32,
    msg_sink: PollSender<Message>,
    data_rx: Receiver<Data>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
    is_running: bool,
}

#[derive(Debug)]
enum Data {
    Connect { conv: u32 },
    Frame(Bytes),
}

#[derive(Debug)]
enum Message {
    Frame(Bytes),
    Flush,
}

impl KcpStream {
    pub(crate) fn new<S, R>(
        config: Arc<KcpConfig>,
        peer_addr: String,
        sink: S,
        stream: R,
        token: Option<CancellationToken>,
    ) -> Self
    where
        S: Sink<Bytes> + Send + Unpin + 'static,
        S::Error: Display,
        R: Stream<Item = BytesMut> + Send + Unpin + 'static,
    {
        let token = token.unwrap_or_else(CancellationToken::new);
        let (msg_tx, msg_rx) = channel(config.snd_wnd as usize);
        let (data_tx, data_rx) = channel(config.rcv_wnd as usize);

        let task = Task::new(config.clone(), sink, stream, msg_rx, data_tx, token.clone());
        Self {
            config,
            peer_addr,
            conv: 0,
            msg_sink: PollSender::new(msg_tx),
            data_rx,
            token,
            task: Some(tokio::spawn(task.run())),
            is_running: true,
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        self.msg_sink
            .send(Message::Flush)
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    fn try_close(&mut self) {
        if self.is_running {
            self.is_running = false;
            self.token.cancel();
            self.data_rx.close();
        }
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        self.try_close();
        self.task.take();
    }
}

struct Task<S, R> {
    kcp: Kcp,
    config: Arc<KcpConfig>,
    sink: S,
    stream: R,
    msg_rx: Receiver<Message>,
    data_tx: Sender<Data>,
    token: CancellationToken,
    rx_buf: BytesMut,
    tx_frame: Option<Bytes>,
    states: u32,
}

impl<S, R> Task<S, R>
where
    S: Sink<Bytes> + Send + Unpin,
    S::Error: Display,
    R: Stream<Item = BytesMut> + Send + Unpin,
{
    const FLUSH: u32 = 0x01;

    fn new(
        config: Arc<KcpConfig>,
        sink: S,
        stream: R,
        msg_rx: Receiver<Message>,
        data_tx: Sender<Data>,
        token: CancellationToken,
    ) -> Self {
        let kcp = Kcp::new(0);
        Self {
            kcp,
            config,
            sink,
            stream,
            msg_rx,
            data_tx,
            token,
            rx_buf: BytesMut::new(),
            tx_frame: None,
            states: 0,
        }
    }

    async fn run(mut self) {
        // TODO: enable all KCP logs.
        //self.kcp.logmask = i32::MAX;

        self.kcp.initialize();
        self.kcp_apply_config();

        let data_tx = self.data_tx.clone();
        loop {
            if self.kcp.has_ouput() {
                self.kcp_output().await;
            }

            let current = self.kcp.current();
            let interval = self.kcp.check(current).wrapping_sub(current);
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

                v = self.stream.next() => {
                    match v {
                         Some(data) => {
                            self.kcp_input(data);
                            // Try to receive more.
                            poll_fn(|cx| {
                                for _ in 1..self.config.rcv_wnd {
                                    match self.stream.poll_next_unpin(cx) {
                                        Poll::Ready(Some(data)) => self.kcp_input(data),
                                        _ => break,
                                    }
                                }
                                Poll::Ready(())
                            }).await;
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

        self.token.cancel();
        self.msg_rx.close();
    }

    /// Return the frame size if the mssage is a data frame.
    fn process_msg(&mut self, msg: Message) {
        match msg {
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
            Message::Flush => self.states |= Self::FLUSH,
        }
    }

    fn kcp_apply_config(&mut self) {
        self.kcp.set_stream(self.config.stream);
        self.kcp.set_mtu(self.config.mtu).unwrap();
        self.kcp
            .set_wndsize(self.config.snd_wnd, self.config.rcv_wnd);
        self.kcp.set_nodelay(
            self.config.nodelay.nodelay,
            self.config.nodelay.interval,
            self.config.nodelay.resend,
            self.config.nodelay.nc,
        );

        // Resize buffer.
        let size = self.config.mtu as usize * 3;
        if self.rx_buf.len() < size {
            self.rx_buf.resize(size, 0);
        }
    }

    fn kcp_flush(&mut self) {
        if self.states & Self::FLUSH != 0 {
            self.states ^= Self::FLUSH;
            self.kcp.update(self.kcp.current());
            self.kcp.flush();
        }
    }

    fn kcp_input(&mut self, data: BytesMut) {
        match self.kcp.input(&data) {
            Ok(_) => self.states |= Self::FLUSH,
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
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

    async fn kcp_output(&mut self) {
        while let Some(data) = self.kcp.pop_output() {
            if let Err(e) = self.sink.feed(data).await {
                // clear all output buffers on error
                while self.kcp.pop_output().is_some() {}
                warn!("send to sink: {}", e);
                break;
            }
        }
        self.sink.flush().await.ok();
    }
}

impl Stream for KcpStream {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Some(data) = ready!(self.data_rx.poll_recv(cx)) {
            match data {
                Data::Connect { conv } => {
                    self.conv = conv;
                }
                Data::Frame(data) => return Poll::Ready(Some(data)),
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
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace")).init();

        let addr1: SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:4322".parse().unwrap();

        let udp1 = Arc::new(UdpSocket::bind(addr1).await.unwrap());
        let stream1 =
            tokio_util::udp::UdpFramed::new(udp1.clone(), tokio_util::codec::BytesCodec::new())
                .filter_map(|x| futures::future::ready(x.ok().map(|(x, _)| x)));
        let sink1 = tokio_util::udp::UdpFramed::new(udp1, tokio_util::codec::BytesCodec::new())
            .with(move |x: Bytes| futures::future::ready(io::Result::Ok((x, addr2))));

        let udp2 = Arc::new(UdpSocket::bind(addr2).await.unwrap());
        let stream2 =
            tokio_util::udp::UdpFramed::new(udp2.clone(), tokio_util::codec::BytesCodec::new())
                .filter_map(|x| futures::future::ready(x.ok().map(|(x, _)| x)));
        let sink2 = tokio_util::udp::UdpFramed::new(udp2, tokio_util::codec::BytesCodec::new())
            .with(move |x: Bytes| futures::future::ready(io::Result::Ok((x, addr1))));

        let config = Arc::new(KcpConfig {
            nodelay: KcpNoDelayConfig::normal(),
            ..Default::default()
        });
        let mut s1 = KcpStream::new(config.clone(), addr2.to_string().into(), sink1, stream1, None);
        let mut s2 = KcpStream::new(config, addr1.into(), sink2, stream2, None);

        s1.send(Bytes::from_static(b"12345")).await.unwrap();
        println!("{:?}", s2.next().await);

        let frame = Bytes::from(vec![0u8; 300000]);
        let start = Instant::now();
        let mut received = 0;
        while start.elapsed() < Duration::from_secs(10) {
            select! {
                _ = s1.send(frame.clone()) => (),
                Some(v) = s2.next() => {
                    //trace!("received {}", v.len());
                    received += v.len();
                }
            }
        }
        error!("total received {}", received);

        s2.flush().await.unwrap();
        s1.close().await.unwrap();
        s2.close().await.unwrap();
    }
}
