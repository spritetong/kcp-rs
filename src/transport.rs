use ::bytes::BytesMut;
use ::futures::{Sink, SinkExt, Stream};
use ::std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use ::tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, Sender},
};
use ::tokio_util::sync::{PollSendError, PollSender};

pub struct KcpChannelTransport<S, R> {
    sink: PollSender<S>,
    stream: Receiver<R>,
}

impl<S: Send + 'static, R: Send + 'static> KcpChannelTransport<S, R> {
    pub fn new(sink: Sender<S>, stream: Receiver<R>) -> Self {
        Self {
            sink: PollSender::new(sink),
            stream,
        }
    }
}

impl<S: Send + 'static, R: Send + 'static> Sink<S> for KcpChannelTransport<S, R> {
    type Error = PollSendError<S>;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().sink.poll_ready_unpin(cx)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: S) -> Result<(), Self::Error> {
        self.get_mut().sink.start_send_unpin(item)
    }

    #[inline]
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().sink.poll_close_unpin(cx)
    }
}

impl<S: Send + 'static, R: Send + 'static> Stream for KcpChannelTransport<S, R> {
    type Item = R;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_recv(cx)
    }
}

////////////////////////////////////////////////////////////////////////////////

#[cfg(feature = "udp")]
use ::tokio_util::{codec::BytesCodec, udp::UdpFramed};
#[cfg(feature = "udp")]
use ::futures::StreamExt;

#[cfg(feature = "udp")]
pub struct KcpUdpTransport {
    udp: UdpFramed<BytesCodec, UdpSocket>,
    peer_addr: SocketAddr,
}

#[cfg(feature = "udp")]
impl KcpUdpTransport {
    pub fn new(udp: UdpSocket, peer_addr: SocketAddr) -> Self {
        Self {
            peer_addr,
            udp: UdpFramed::new(udp, BytesCodec::new()),
        }
    }
}

#[cfg(feature = "udp")]
impl Sink<BytesMut> for KcpUdpTransport {
    type Error = io::Error;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(BytesMut, SocketAddr)>::poll_ready(Pin::new(&mut self.get_mut().udp), cx)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(BytesMut, SocketAddr)>::poll_flush(Pin::new(&mut self.get_mut().udp), cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: BytesMut) -> Result<(), Self::Error> {
        let this = self.get_mut();
        Sink::<(BytesMut, SocketAddr)>::start_send(Pin::new(&mut this.udp), (item, this.peer_addr))
    }

    #[inline]
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<(BytesMut, SocketAddr)>::poll_close(Pin::new(&mut self.get_mut().udp), cx)
    }
}

#[cfg(feature = "udp")]
impl Stream for KcpUdpTransport {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.udp.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok((packet, addr)))) => {
                    if addr == self.peer_addr {
                        return Poll::Ready(Some(packet));
                    }
                }
                Poll::Ready(Some(Err(_))) => (),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
