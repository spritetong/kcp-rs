use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream, StreamExt};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, UnboundedSender},
};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

pub struct UdpTransport {
    udp: UdpFramed<BytesCodec, UdpSocket>,
    peer_addr: SocketAddr,
}

impl UdpTransport {
    pub fn new(udp: UdpSocket, peer_addr: SocketAddr) -> Self {
        Self {
            peer_addr,
            udp: UdpFramed::new(udp, BytesCodec::new()),
        }
    }
}

impl<T: Into<Bytes>> Sink<T> for UdpTransport {
    type Error = io::Error;

    #[inline]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(Bytes, SocketAddr)>::poll_ready(Pin::new(&mut self.udp), cx)
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(Bytes, SocketAddr)>::poll_flush(Pin::new(&mut self.udp), cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.get_mut();
        Sink::<(Bytes, SocketAddr)>::start_send(
            Pin::new(&mut this.udp),
            (item.into(), this.peer_addr),
        )
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(Bytes, SocketAddr)>::poll_close(Pin::new(&mut self.udp), cx)
    }
}

#[cfg(feature = "udp")]
impl Stream for UdpTransport {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.udp.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok((packet, addr)))) => {
                    if addr == self.peer_addr {
                        return Poll::Ready(Some(packet));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    log::error!("UDP read error: {}", e);
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

pub struct UdpMpscTransport<T> {
    sink: Option<UnboundedSender<(T, SocketAddr)>>,
    stream: Receiver<BytesMut>,
    peer_addr: SocketAddr,
}

impl<T> UdpMpscTransport<T> {
    pub fn new(
        sink: Option<UnboundedSender<(T, SocketAddr)>>,
        stream: Receiver<BytesMut>,
        peer_addr: SocketAddr,
    ) -> Self {
        Self {
            sink,
            stream,
            peer_addr,
        }
    }

    #[inline]
    pub fn get_sink(&self) -> &Option<UnboundedSender<(T, SocketAddr)>> {
        &self.sink
    }

    #[inline]
    pub fn get_sink_mut(&mut self) -> &mut Option<UnboundedSender<(T, SocketAddr)>> {
        &mut self.sink
    }

    #[inline]
    pub fn get_stream(&self) -> &Receiver<BytesMut> {
        &self.stream
    }

    #[inline]
    pub fn get_stream_mut(&mut self) -> &mut Receiver<BytesMut> {
        &mut self.stream
    }
}

impl<T: Into<Bytes>> Sink<T> for UdpMpscTransport<T> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.sink.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::ErrorKind::NotConnected.into()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.sink.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::ErrorKind::NotConnected.into()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        if let Some(sink) = &self.sink {
            sink.send((item, self.peer_addr))
                .map_err(|_| io::ErrorKind::NotConnected.into())
        } else {
            Err(io::ErrorKind::NotConnected.into())
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.sink.take();
        Poll::Ready(Ok(()))
    }
}

impl<T> Stream for UdpMpscTransport<T> {
    type Item = BytesMut;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_recv(cx)
    }
}
