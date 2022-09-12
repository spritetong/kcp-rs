use futures::{FutureExt, SinkExt, StreamExt};

use crate::kcp::*;
use crate::*;

pub struct KcpStream {
    peer_addr: SocketAddr,
    conv: u32,
    mtu: u32,
    msg_sink: PollSender<Message>,
    data_stream: ReceiverStream<Data>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
    is_closed: bool,
}

enum Data {
    Connect { peer_addr: SocketAddr, conv: u32 },
    Frame(Bytes),
}

enum Message {
    Frame(Bytes),
    Mtu(u32),
    Flush,
}

impl KcpStream {
    pub fn new(udp: UdpSocket, peer_addr: SocketAddr, token: Option<CancellationToken>) -> Self {
        let token = token.unwrap_or_else(CancellationToken::new);
        let kcp = Kcp::new(0);
        let (msg_tx, msg_rx) = channel(kcp.snd_wnd as usize);
        let (data_tx, data_rx) = channel(kcp.rcv_wnd as usize);

        let mtu = kcp.mtu;
        let task = Task {
            kcp,
            udp,
            peer_addr,
            msg_rx,
            data_tx,
            token: token.clone(),
            rx_buf: BytesMut::new(),
            tx_frame: None,
        };

        Self {
            peer_addr,
            conv: 0,
            mtu,
            msg_sink: PollSender::new(msg_tx),
            data_stream: data_rx.into(),
            token,
            task: Some(tokio::spawn(task.run())),
            is_closed: false,
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
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        if !self.is_closed {
            self.is_closed = true;
            self.token.cancel();
            self.data_stream.close();
        }
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
}

impl Task {
    async fn run(mut self) {
        // TODO: enable all KCP logs.
        self.kcp.logmask = i32::MAX;
        self.kcp.initialize();
        self.kcp_mut_changed();

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
                    let mut sent = 0;
                    if self.process_msg(msg) > 0 {
                        sent += 1;
                    }
                    // Try to process more.
                    while !self.kcp.is_send_queue_full() {
                        if let Ok(msg) = self.msg_rx.try_recv() {
                            if self.process_msg(msg) > 0 {
                                sent += 1;
                            }
                        }
                    }
                    if sent > 0 {
                        self.kcp.update(self.kcp.current());
                        self.kcp.flush();
                    }
                }

                Ok(permit) = data_tx.reserve(), if self.tx_frame.is_some() => {
                    permit.send(Data::Frame(self.tx_frame.take().unwrap()));
                    loop {
                        self.tx_frame = self.kcp.recv_bytes();
                        if self.tx_frame.is_none() {
                            break;
                        }
                        match data_tx.try_reserve() {
                            Ok(permit) => permit.send(Data::Frame(self.tx_frame.take().unwrap())),
                            _ => break,
                        }
                    }
                }

                v = self.udp.recv_from(&mut self.rx_buf) => {
                    match v {
                         Ok((size, addr)) if addr == self.peer_addr => {
                            self.kcp_input(size);
                            let mut recveived = 1;
                            // Try to receive more.
                            while recveived < self.kcp.rcv_wnd {
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

                            self.kcp.update(self.kcp.current());
                            self.kcp.flush();
                         }
                         Ok(_) => (),
                         Err(e) => error!("recv from error: {}", e),
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(interval as u64)) => continue,
                _ = self.token.cancelled() => break,
            }
        }

        self.token.cancel();
        self.msg_rx.close();
    }

    fn kcp_mut_changed(&mut self) {
        if self.rx_buf.len() < self.kcp.mtu as usize {
            self.rx_buf.resize(self.kcp.mtu as usize, 0);
        }
    }

    /// return data size
    fn process_msg(&mut self, msg: Message) -> usize {
        match msg {
            Message::Frame(data) => match self.kcp.send(data.deref()) {
                Ok(_) => return data.len(),
                _ => error!(
                    "Too big frame size: {} > {}",
                    data.len(),
                    Kcp::max_frame_size(self.kcp.mtu)
                ),
            },
            Message::Flush => return usize::MAX,
            Message::Mtu(mtu) => match self.kcp.set_mtu(mtu) {
                Ok(_) => self.kcp_mut_changed(),
                Err(e) => error!("Set MTU error: {}", e),
            },
        }
        0
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

    fn kcp_input(&mut self, packet_size: usize) {
        if let Err(e) = self.kcp.input(&self.rx_buf[..packet_size]) {
            match e.kind() {
                io::ErrorKind::NotFound => {
                    trace!(
                        "conv not match: {} != {}",
                        Kcp::read_conv(&self.rx_buf).unwrap_or(0),
                        self.kcp.conv
                    );
                }
                io::ErrorKind::InvalidData => {
                    trace!("packet parse error");
                }
                _ => unreachable!(),
            }
        }
    }
}

impl Stream for KcpStream {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.data_stream.poll_next_unpin(cx)) {
                Some(data) => match data {
                    Data::Connect { peer_addr, conv } => {
                        self.peer_addr = peer_addr;
                        self.conv = conv;
                    }
                    Data::Frame(data) => return Poll::Ready(Some(data)),
                },
                None => return Poll::Ready(None),
            }
        }
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
        // Do nothing
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
        if !this.is_closed {
            this.is_closed = true;
            this.token.cancel();
            this.data_stream.close();
        }
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
        env_logger::init();

        let addr1: SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:4322".parse().unwrap();

        let udp1 = UdpSocket::bind(addr1).await.unwrap();
        let udp2 = UdpSocket::bind(addr2).await.unwrap();

        let mut s1 = KcpStream::new(udp1, addr2, None);
        let mut s2 = KcpStream::new(udp2, addr1, None);

        error!("before send");
        assert!(s1.send(Bytes::from_static(b"12345")).await.is_ok());
        error!("after send");
        println!("{:?}", s2.next().await);

        assert!(s1.close().await.is_ok());
        assert!(s2.close().await.is_ok());
    }
}
