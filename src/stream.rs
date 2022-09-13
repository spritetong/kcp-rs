use crate::kcp::*;
use crate::*;

pub struct KcpStream {
    peer_addr: SocketAddr,
    conv: u32,
    mtu: u32,
    msg_sink: PollSender<Message>,
    data_rx: Receiver<Data>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
    is_running: bool,
}

#[derive(Debug)]
enum Data {
    Connect { peer_addr: SocketAddr, conv: u32 },
    Frame(Bytes),
}

#[derive(Debug)]
enum Message {
    Frame(Bytes),
    Mtu(u32),
    Flush,
}

impl KcpStream {
    pub(crate) fn new(
        udp: UdpSocket,
        peer_addr: SocketAddr,
        token: Option<CancellationToken>,
    ) -> Self {
        let token = token.unwrap_or_else(CancellationToken::new);
        let kcp = Kcp::new(0);
        let (msg_tx, msg_rx) = channel(kcp.snd_wnd() as usize);
        let (data_tx, data_rx) = channel(kcp.rcv_wnd() as usize);

        let mtu = kcp.mtu();
        let task = Task {
            kcp,
            udp,
            peer_addr,
            msg_rx,
            data_tx,
            token: token.clone(),
            rx_buf: BytesMut::new(),
            tx_frame: None,
            flags: 0,
        };

        Self {
            peer_addr,
            conv: 0,
            mtu,
            msg_sink: PollSender::new(msg_tx),
            data_rx,
            token,
            task: Some(tokio::spawn(task.run())),
            is_running: true,
        }
    }

    pub async fn set_mtu(&mut self, mtu: u32) -> io::Result<()> {
        self.msg_sink
            .send(Message::Mtu(mtu))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
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

struct Task {
    kcp: Kcp,
    udp: UdpSocket,
    peer_addr: SocketAddr,
    msg_rx: Receiver<Message>,
    data_tx: Sender<Data>,
    token: CancellationToken,
    rx_buf: BytesMut,
    tx_frame: Option<Bytes>,
    flags: u32,
}

impl Task {
    const F_FLUSH: u32 = 0x01;

    async fn run(mut self) {
        // TODO: enable all KCP logs.
        //self.kcp.logmask = i32::MAX;
        //self.kcp.set_nodelay(true, 10, 2, true);
        self.kcp.set_stream(true);

        self.kcp.initialize();
        self.kcp_mtu_changed();

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
                                self.tx_frame = Some(data);
                                break;
                            }
                        }
                    }
                }

                v = self.udp.recv_from(&mut self.rx_buf) => {
                    match v {
                         Ok((size, addr)) if addr == self.peer_addr => {
                            self.kcp_input(size);
                            let mut recveived = 1;
                            // Try to receive more.
                            while recveived < self.kcp.rcv_wnd() {
                                match self.udp.try_recv_from(&mut self.rx_buf) {
                                    Ok((size, addr)) if addr == self.peer_addr => {
                                        self.kcp_input(size);
                                        recveived += 1;
                                    }
                                    Ok(_) => (),
                                    Err(_) => break,
                                }
                            }
                            // Try to fetch a frame.
                            if self.tx_frame.is_none() {
                                self.tx_frame = self.kcp.recv_bytes();
                            }
                            // Try to flush.
                            self.kcp_flush();
                         }
                         Ok(_) => (),
                         Err(e) => error!("recv from error: {}", e),
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
                    if self.kcp.nodelay() != 0 || self.kcp.nsnd_que() > 1 {
                        self.flags |= Self::F_FLUSH;
                    }
                }
                _ => error!(
                    "Too big frame size: {} > {}",
                    data.len(),
                    Kcp::max_frame_size(self.kcp.mtu())
                ),
            },
            Message::Flush => self.flags |= Self::F_FLUSH,
            Message::Mtu(mtu) => match self.kcp.set_mtu(mtu) {
                Ok(_) => self.kcp_mtu_changed(),
                Err(e) => error!("Set MTU error: {}", e),
            },
        }
    }

    fn kcp_mtu_changed(&mut self) {
        if self.rx_buf.len() < self.kcp.mtu() as usize {
            self.rx_buf.resize(self.kcp.mtu() as usize, 0);
        }
    }

    fn kcp_flush(&mut self) {
        if self.flags & Self::F_FLUSH != 0 {
            self.flags ^= Self::F_FLUSH;
            self.kcp.update(self.kcp.current());
            self.kcp.flush();
        }
    }

    fn kcp_input(&mut self, packet_size: usize) {
        match self.kcp.input(&self.rx_buf[..packet_size]) {
            Ok(_) => self.flags |= Self::F_FLUSH,
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    trace!(
                        "conv not match: {} != {}",
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
            if let Err(e) = self.udp.send_to(&data, self.peer_addr).await {
                // clear all output buffers on error
                while self.kcp.pop_output().is_some() {}
                warn!("send to {}: {}", &self.peer_addr, e);
                break;
            }
        }
    }
}

impl Stream for KcpStream {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Some(data) = ready!(self.data_rx.poll_recv(cx)) {
            match data {
                Data::Connect { peer_addr, conv } => {
                    self.peer_addr = peer_addr;
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

        let udp1 = UdpSocket::bind(addr1).await.unwrap();
        let udp2 = UdpSocket::bind(addr2).await.unwrap();

        let mut s1 = KcpStream::new(udp1, addr2, None);
        let mut s2 = KcpStream::new(udp2, addr1, None);

        s1.send(Bytes::from_static(b"12345")).await.unwrap();
        s1.flush().await.unwrap();
        println!("{:?}", s2.next().await);

        let frame = Bytes::from(vec![0u8; 300]);
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
