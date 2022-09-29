pub use crate::config::*;
use crate::protocol::Kcp;

use ::bytes::{BufMut, Bytes, BytesMut};
use ::futures::{future::poll_fn, ready, FutureExt, Sink, SinkExt, Stream, StreamExt};
use ::log::{error, trace, warn};
use ::std::{
    fmt::Display,
    io,
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use ::tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    select,
    sync::mpsc::{channel, error::TryRecvError, Receiver, Sender},
    task::JoinHandle,
};
use ::tokio_util::sync::{CancellationToken, PollSender};

pub struct KcpStream {
    config: Arc<KcpConfig>,
    conv: u32,
    input_sink: PollSender<Bytes>,
    output_rx: Receiver<Output>,
    token: CancellationToken,
    task: Option<JoinHandle<()>>,
    // for AsyncRead
    read_buf: Option<Bytes>,
}

impl KcpStream {
    pub const SYN_CONV: u32 = 0xFFFF_FFFE;

    /// Check if a conv is valid.
    #[inline]
    pub fn is_valid_conv(conv: u32) -> bool {
        conv != 0 && conv < Self::SYN_CONV
    }

    /// Generate a random conv.
    pub fn rand_conv() -> u32 {
        loop {
            let conv = rand::random();
            if Self::is_valid_conv(conv) {
                break conv;
            }
        }
    }

    #[inline]
    pub fn read_conv(packet: &[u8]) -> Option<u32> {
        Kcp::read_conv(packet)
    }

    /// Read the session id from the SYN handshake packet.
    pub fn read_session_id<'a>(packet: &'a [u8], session_key: &[u8]) -> Option<&'a [u8]> {
        Kcp::read_payload_data(packet).and_then(|x| x.strip_prefix(session_key))
    }
}

impl KcpStream {
    pub async fn connect<T, Si, D>(
        config: Arc<KcpConfig>,
        transport: T,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> io::Result<Self>
    where
        T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin + 'static,
        Si: From<BytesMut> + Send + 'static,
        <T as Sink<Si>>::Error: Display,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        Self::new(config.clone(), Self::SYN_CONV, transport, disconnect, token)
            .wait_connection()
            .await
    }

    pub async fn accept<T, Si, D>(
        config: Arc<KcpConfig>,
        conv: u32,
        transport: T,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> io::Result<Self>
    where
        T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin + 'static,
        Si: From<BytesMut> + Send + 'static,
        <T as Sink<Si>>::Error: Display,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        if !KcpStream::is_valid_conv(conv) {
            error!("Invalid conv 0x{:08X}", conv);
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Self::new(config.clone(), conv, transport, disconnect, token)
            .wait_connection()
            .await
    }

    fn new<T, Si, D>(
        config: Arc<KcpConfig>,
        conv: u32,
        transport: T,
        disconnect: D,
        token: Option<CancellationToken>,
    ) -> Self
    where
        T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin + 'static,
        Si: From<BytesMut> + Send + 'static,
        <T as Sink<Si>>::Error: Display,
        D: Sink<u32> + Send + Unpin + 'static,
    {
        let token = token.unwrap_or_else(CancellationToken::new);
        let (input_tx, input_rx) = channel(config.snd_wnd.max(8) as usize);
        let (output_tx, output_rx) = channel(config.rcv_wnd.max(16) as usize);

        Self {
            config: config.clone(),
            conv: 0,
            input_sink: PollSender::new(input_tx),
            output_rx,
            token: token.clone(),
            task: Some(tokio::spawn(
                Task {
                    kcp: Kcp::new(conv),
                    config,
                    input_rx,
                    token,
                    rx_buf: BytesMut::new(),
                    hs: Handshake::Syn,
                    flags: 0,
                    hs_end_time: 0,
                    last_io_time: 0,
                    session_id: Default::default(),
                }
                .run(output_tx, transport, disconnect),
            )),
            read_buf: None,
        }
    }

    #[inline]
    pub fn config(&self) -> Arc<KcpConfig> {
        self.config.clone()
    }

    #[inline]
    pub fn conv(&self) -> u32 {
        self.conv
    }

    /// Shutdown the input channel without awaiting.
    pub fn shutdown_immediately(&mut self) {
        self.input_sink.abort_send();
        self.input_sink.close();
    }
}

impl KcpStream {
    async fn wait_connection(mut self) -> io::Result<Self> {
        let rst =
            match tokio::time::timeout(self.config.connect_timeout, self.output_rx.recv()).await {
                Ok(Some(Output::Connected { conv })) => {
                    trace!("Connect conv {}", conv);
                    self.conv = conv;
                    return Ok(self);
                }
                Ok(_) => Err(io::ErrorKind::NotConnected.into()),
                _ => Err(io::ErrorKind::TimedOut.into()),
            };
        self.close().await.ok();
        rst
    }

    fn try_close(&mut self) {
        self.token.cancel();
        self.output_rx.close();
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        self.try_close();
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug)]
enum Output {
    Connected { conv: u32 },
    Frame(Bytes),
}

#[derive(Clone, Copy, PartialOrd, PartialEq, Eq, Debug)]
enum Handshake {
    Syn,
    Connected,
    FinPending,
    FinSent,
    FinWaitPeer,
    Disconnected,
}

struct Task {
    kcp: Kcp,
    config: Arc<KcpConfig>,
    input_rx: Receiver<Bytes>,
    token: CancellationToken,
    rx_buf: BytesMut,

    hs: Handshake,
    flags: u32,
    hs_end_time: u32,
    last_io_time: u32,
    session_id: Vec<u8>,
}

impl Task {
    const CLIENT: u32 = 0x01;
    const FLUSH: u32 = 0x02;
    const FIN_RECVED: u32 = 0x04;
    const INPUT_CLOSED: u32 = 0x08;

    const CMD_MASK: u8 = 0x57;
    const KCP_SYN: u8 = 0x80;
    const KCP_FIN: u8 = 0x20;
    const KCP_RESET: u8 = 0x08;

    const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);
}

impl Task {
    async fn run<T, Si, D>(mut self, output_tx: Sender<Output>, mut transport: T, mut disconnect: D)
    where
        T: Sink<Si> + Stream<Item = BytesMut> + Unpin,
        Si: From<BytesMut> + Send,
        <T as Sink<Si>>::Error: Display,
        D: Sink<u32> + Send + Unpin,
    {
        self.kcp.initialize();
        self.kcp_apply_config();

        // Prepare for SYN handshake.
        self.kcp.set_nodelay(true, 100, 0, false);
        self.last_io_time = self.kcp.current();
        if self.kcp.conv() == KcpStream::SYN_CONV {
            self.flags |= Self::CLIENT;
            // Create random session ID for handshake.
            self.session_id = self.config.random_session_id();
            // Send SYN packet.
            self.kcp.set_conv(KcpStream::SYN_CONV);
            self.handshake_send();
        }

        let mut tx_frame: Option<Bytes> = None;
        loop {
            if self.kcp.has_ouput() {
                select! {
                    biased;
                    _ = Self::kcp_output(&mut self.kcp, &mut transport, self.hs) => (),
                    _ = self.token.cancelled() => break,
                }
            }

            if self.hs == Handshake::Disconnected {
                break;
            }

            if self.kcp.is_dead_link()
                || self.kcp.duration_since(self.last_io_time)
                    >= (self.config.session_expire.as_secs() * 1000) as u32
            {
                // TODO:
                break;
            }

            let current = self.kcp.get_system_time();
            let mut interval = self.kcp.check(current).wrapping_sub(current).min(60000);
            if self.is_fin_handshake() {
                let timeout = self.hs_end_time.wrapping_sub(current);
                if timeout as i32 > 0 {
                    interval = interval.min(timeout);
                } else {
                    self.set_handshake(Handshake::Disconnected);
                }
            }
            if interval == 0 {
                self.kcp.update(current);
                continue;
            }

            select! {
                x = self.input_rx.recv(), if !self.has(Self::INPUT_CLOSED)
                        && !self.kcp.is_send_queue_full() => {
                    let mut closed = false;
                    match x {
                        Some(frame) => {
                            self.process_input(frame);
                            // Try to process more.
                            while !self.kcp.is_send_queue_full() {
                                match self.input_rx.try_recv() {
                                    Ok(frame) => self.process_input(frame),
                                    Err(TryRecvError::Empty) => break,
                                    Err(TryRecvError::Disconnected) => {
                                        closed = true;
                                        break;
                                    }
                                }
                            }
                        }
                        _ => closed = true,
                    }
                    if closed {
                        log::debug!("({}) Input channel closed, starting FIN handshake", self.is_client());
                        self.flags |= Self::INPUT_CLOSED;
                        self.fin_transite_state();
                    }
                    // Try to flush.
                    self.kcp_flush();
                }

                x = output_tx.reserve(), if tx_frame.is_some() => {
                    match x {
                        Ok(permit) => {
                            if let Some(frame) = tx_frame.take() {
                                permit.send(Output::Frame(frame));
                            }
                            while let Some(frame) = self.kcp_recv() {
                                match output_tx.try_reserve() {
                                    Ok(permit) => permit.send(Output::Frame(frame)),
                                    _ => {
                                        // Save the data frame.
                                        tx_frame = Some(frame);
                                        break;
                                    }
                                }
                            }
                        }
                        _ => break,
                    }
                }

                x = transport.next() => {
                    match x {
                        Some(packet) => {
                            self.kcp_input(packet);
                            // Try to receive more.
                            poll_fn(|cx| {
                                for _ in 1..self.config.rcv_wnd {
                                    match transport.poll_next_unpin(cx) {
                                        Poll::Ready(Some(packet)) => self.kcp_input(packet),
                                        _ => break,
                                    }
                                }
                                Poll::Ready(())
                            }).await;

                            // SYN handshake.
                            if self.hs == Handshake::Syn &&
                                    self.syn_handshake_recv(&output_tx).is_err() {
                                self.set_handshake(Handshake::Disconnected);
                            }
                            // FIN handshake.
                            if self.is_fin_handshake() {
                                self.fin_transite_state();
                            }

                            // Try to fetch a frame.
                            if tx_frame.is_none() {
                                tx_frame = self.kcp_recv();
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

        log::debug!("({}) break loop", self.is_client());

        tx_frame.take();
        drop(output_tx);
        self.input_rx.close();
        while self.input_rx.recv().await.is_some() {}

        // TODO: send RESET packet.
        if KcpStream::is_valid_conv(self.kcp.conv()) {
            let mut buf = BytesMut::new();
            self.kcp.write_ack_head(&mut buf, Self::KCP_RESET, 0);
            transport.send(buf.into()).await.ok();
        }

        // Report disconnect.
        if tokio::time::timeout(Self::DISCONNECT_TIMEOUT, disconnect.send(self.kcp.conv()))
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
    fn is_client(&self) -> bool {
        self.flags & Self::CLIENT != 0
    }

    #[inline]
    fn has(&self, state: u32) -> bool {
        self.flags & state != 0
    }

    fn handshake_send(&mut self) {
        self.flags |= Self::FLUSH;
        let mut syn = Vec::<u8>::new();
        syn.put_slice(&self.config.session_key);
        syn.put_slice(&self.session_id);
        self.kcp.send(syn.as_slice()).unwrap();
        self.kcp_flush();
    }

    fn syn_handshake_recv(&mut self, output_tx: &Sender<Output>) -> Result<(), ()> {
        // Check SYN frame.
        let syn = match self.kcp_recv() {
            Some(x) => x,
            _ => return Ok(()),
        };
        // Check the session key and get the session ID.
        if let Some(session_id) = syn
            .strip_prefix(self.config.session_key.deref())
            .and_then(|x| {
                if x.len() == self.config.session_id_len {
                    Some(x)
                } else {
                    None
                }
            })
        {
            // Key must be consistent.
            if self.is_client() {
                // Verify the session ID for the client endpoint.
                if self.session_id != session_id {
                    return Err(());
                }
                self.kcp_apply_nodelay();
            } else {
                self.session_id = session_id.to_vec();
                self.kcp_apply_nodelay();
                self.handshake_send();
            }

            // Connection has been established.
            self.set_handshake(Handshake::Connected);
            return output_tx
                .try_send(Output::Connected {
                    conv: self.kcp.conv(),
                })
                .map_err(|_| ());
        }
        Err(())
    }

    fn syn_handshake_input(&mut self, packet: &[u8]) -> io::Result<()> {
        // Switch kcp's conv to try to accept the packet.
        if let Some(conv) = Kcp::read_conv(packet) {
            if self.is_client() {
                if self.hs == Handshake::Syn && conv != KcpStream::SYN_CONV {
                    // For the client endpoint.
                    // Try to accept conv from the server.
                    self.kcp.set_conv(conv);
                    if self.kcp.input(packet).is_err() || self.kcp.get_waitsnd() > 0 {
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
                self.kcp.input(packet).ok();
                // Restore conv.
                self.kcp.set_conv(mine);
                return Ok(());
            }
        }
        Err(io::ErrorKind::InvalidInput.into())
    }

    #[inline]
    fn is_fin_handshake(&self) -> bool {
        (Handshake::FinPending..Handshake::FinWaitPeer).contains(&self.hs)
    }

    #[inline]
    fn set_handshake(&mut self, hs: Handshake) {
        log::debug!("({}) {:?} -> {:?}", self.is_client(), self.hs, hs);
        self.hs = hs;
    }

    fn fin_transite_state(&mut self) {
        loop {
            self.hs = match self.hs {
                Handshake::Syn => Handshake::Disconnected,
                Handshake::Connected => {
                    self.hs_end_time = self.kcp.get_system_time()
                        + Self::DISCONNECT_TIMEOUT.as_secs() as u32 * 1000;
                    Handshake::FinPending
                }
                Handshake::FinPending => {
                    log::debug!(
                        "({}) in {:?}, input closed: {}, waitsnd: {}",
                        self.is_client(),
                        self.hs,
                        self.has(Self::INPUT_CLOSED),
                        self.kcp.get_waitsnd()
                    );
                    if !self.has(Self::INPUT_CLOSED) || self.kcp.get_waitsnd() > 0 {
                        break;
                    }
                    self.handshake_send();
                    Handshake::FinSent
                }
                Handshake::FinSent => {
                    log::debug!(
                        "({}) in {:?}, waitsnd: {}",
                        self.is_client(),
                        self.hs,
                        self.kcp.get_waitsnd()
                    );
                    if self.kcp.get_waitsnd() > 0 {
                        break;
                    }
                    Handshake::FinWaitPeer
                }
                Handshake::FinWaitPeer => {
                    log::debug!(
                        "({}) in {:?}, waitsnd: {} + {}",
                        self.is_client(),
                        self.hs,
                        self.kcp.nrcv_que(),
                        self.kcp.nrcv_buf(),
                    );
                    // KCP: ensure all frames have been received.
                    if !self.has(Self::FIN_RECVED) || self.kcp.nrcv_que() + self.kcp.nrcv_buf() > 0
                    {
                        break;
                    }
                    Handshake::Disconnected
                }
                Handshake::Disconnected => break,
            };
        }
    }

    fn process_input(&mut self, frame: Bytes) {
        match self.kcp.send(frame.deref()) {
            Ok(_) => {
                // KCP: flush if it's no delay or the number of not-sent buffers is greater than 1.
                if self.config.nodelay.nodelay || self.kcp.nsnd_que() > 1 {
                    self.flags |= Self::FLUSH;
                }
            }
            _ => error!(
                "Too big frame size: {} > {}",
                frame.len(),
                Kcp::max_frame_size(self.config.mtu)
            ),
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

    fn kcp_recv(&mut self) -> Option<Bytes> {
        // KCP: FIN frame is the last one in the stream.
        if self.has(Self::FIN_RECVED) && self.kcp.nrcv_que() == 1 && self.kcp.nrcv_buf() == 0 {
            // Check the session key and ID of the FIN frame.
            if let Some(fin) = self.kcp.recv_bytes() {
                if Some(&self.session_id[..]) == fin.strip_prefix(self.config.session_key.deref()) {
                    log::debug!("({}) Receive FIN frame", self.is_client());
                }
            }
            None
        } else {
            self.kcp.recv_bytes()
        }
    }

    fn kcp_flush(&mut self) {
        if self.has(Self::FLUSH) {
            self.flags ^= Self::FLUSH;
            self.kcp.update(self.kcp.get_system_time());
            self.kcp.flush();
            self.last_io_time = self.kcp.current();
        }
    }

    fn kcp_input(&mut self, mut packet: BytesMut) {
        match self.kcp.input(&mut packet) {
            Ok(_) => self.flags |= Self::FLUSH,
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    // SYN handshake.
                    if self.syn_handshake_input(&packet).is_ok() {
                        return;
                    }
                    trace!(
                        "conv does not match: {} != {}",
                        Kcp::read_conv(&self.rx_buf).unwrap_or(0),
                        self.kcp.conv()
                    );
                }
                io::ErrorKind::InvalidData => {
                    let cmd = Kcp::read_cmd(&packet);
                    if cmd & Self::KCP_RESET != 0 {
                        log::debug!("({}) receive RESET", self.is_client());
                        self.set_handshake(Handshake::Disconnected);
                        return;
                    }
                    if cmd & Self::KCP_FIN != 0 {
                        if !self.has(Self::FIN_RECVED) {
                            self.flags |= Self::FIN_RECVED;
                            // Do not get any more input.
                            self.input_rx.close();
                        }
                        self.fin_transite_state();
                        Kcp::write_cmd(&mut packet, cmd ^ Self::KCP_FIN);
                        if self.kcp.input(&packet).is_ok() {
                            return;
                        }
                    }
                    trace!("packet parse error");
                }
                _ => unreachable!(),
            },
        }
    }

    async fn kcp_output<T, Si>(kcp: &mut Kcp, sink: &mut T, hs: Handshake)
    where
        T: Sink<Si> + Stream<Item = BytesMut> + Unpin,
        Si: From<BytesMut> + Send,
        <T as Sink<Si>>::Error: Display,
    {
        while let Some(mut packet) = kcp.pop_output() {
            if hs == Handshake::FinSent {
                // Set KCP_FIN flag for FIN handshake.
                if Kcp::read_payload_data(&packet).is_some() {
                    let cmd = Kcp::read_cmd(&packet) | Self::KCP_FIN;
                    Kcp::write_cmd(&mut packet, cmd);
                }
            }

            if let Err(e) = sink.feed(packet.into()).await {
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
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.read_buf.is_some() {
            return Poll::Ready(Some(Ok(self.read_buf.take().unwrap())));
        }
        while let Some(x) = ready!(self.output_rx.poll_recv(cx)) {
            match x {
                Output::Frame(frame) => return Poll::Ready(Some(Ok(frame))),
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
            .input_sink
            .poll_ready_unpin(cx)
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.get_mut()
            .input_sink
            .start_send_unpin(item)
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let _ = ready!(this.input_sink.poll_close_unpin(cx));
        this.try_close();
        if let Some(task) = this.task.as_mut() {
            let _ = ready!(task.poll_unpin(cx));
            this.task.take();
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for KcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();

        while this.read_buf.is_none() {
            match this.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    if !chunk.is_empty() {
                        this.read_buf = Some(chunk);
                    }
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }

        if let Some(ref mut chunk) = this.read_buf {
            let len = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..len]);
            if len < chunk.len() {
                let _ = chunk.split_to(len);
            } else {
                this.read_buf = None;
            }
        }

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for KcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = self.get_mut();
        let mut len = 0;

        for chunk in buf.chunks(Kcp::max_frame_size(this.config.mtu) as usize) {
            match this.poll_ready_unpin(cx) {
                Poll::Ready(Ok(_)) => {
                    if let Err(e) = this.start_send_unpin(Bytes::copy_from_slice(chunk)) {
                        if len > 0 {
                            break;
                        }
                        return Poll::Ready(Err(e));
                    }
                    len += chunk.len();
                }
                Poll::Ready(Err(e)) => {
                    if len > 0 {
                        break;
                    }
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    if len > 0 {
                        break;
                    }
                    return Poll::Pending;
                }
            }
        }

        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.poll_close(cx)
    }
}
