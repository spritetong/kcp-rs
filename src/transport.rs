use ::futures::{Sink, SinkExt, Stream};
use ::std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use ::tokio::sync::mpsc::{Receiver, Sender};
use ::tokio_util::sync::PollSender;

pub struct MpscTransport<S, R> {
    sink: PollSender<S>,
    stream: Receiver<R>,
}

impl<S, R> MpscTransport<S, R>
where
    S: Send + 'static,
    R: Send,
{
    pub fn new(sink: Sender<S>, stream: Receiver<R>) -> Self {
        Self {
            sink: PollSender::new(sink),
            stream,
        }
    }

    pub fn get_sink(&self) -> &PollSender<S> {
        &self.sink
    }

    pub fn get_sink_mut(&mut self) -> &mut PollSender<S> {
        &mut self.sink
    }

    pub fn get_stream(&self) -> &Receiver<R> {
        &self.stream
    }

    pub fn get_stream_mut(&mut self) -> &mut Receiver<R> {
        &mut self.stream
    }
}

impl<S, R> Sink<S> for MpscTransport<S, R>
where
    S: Send + 'static,
    R: Send,
{
    type Error = <PollSender<S> as Sink<S>>::Error;

    #[inline]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn start_send(mut self: Pin<&mut Self>, item: S) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item)
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx)
    }
}

impl<S, R> Stream for MpscTransport<S, R>
where
    S: Send + 'static,
    R: Send,
{
    type Item = R;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_recv(cx)
    }
}

////////////////////////////////////////////////////////////////////////////////

pub struct MergeTransport<S, Si, R> {
    sink: S,
    stream: R,
    _phantom: PhantomData<Si>,
}

impl<S, Si, R> MergeTransport<S, Si, R> {
    pub fn new(sink: S, stream: R) -> Self {
        Self {
            sink,
            stream,
            _phantom: Default::default(),
        }
    }

    pub fn get_sink(&self) -> &S {
        &self.sink
    }

    pub fn get_sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    pub fn get_stream(&self) -> &R {
        &self.stream
    }

    pub fn get_stream_mut(&mut self) -> &mut R {
        &mut self.stream
    }

    pub fn split_into(self) -> (S, R) {
        (self.sink, self.stream)
    }
}

impl<S, Si, R> Sink<Si> for MergeTransport<S, Si, R>
where
    S: Sink<Si> + Unpin,
    Si: Send + Unpin,
    R: Stream + Unpin,
{
    type Error = S::Error;

    #[inline]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx)
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_flush_unpin(cx)
    }

    #[inline]
    fn start_send(mut self: Pin<&mut Self>, item: Si) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item)
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx)
    }
}

impl<S, Si, R> Stream for MergeTransport<S, Si, R>
where
    S: Sink<Si> + Unpin,
    Si: Send + Unpin,
    R: Stream + Unpin,
{
    type Item = R::Item;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

////////////////////////////////////////////////////////////////////////////////

#[cfg(feature = "udp")]
use ::bytes::{Bytes, BytesMut};
#[cfg(feature = "udp")]
use ::futures::StreamExt;
#[cfg(feature = "udp")]
use ::std::{io, net::SocketAddr};
#[cfg(feature = "udp")]
use ::tokio::net::UdpSocket;
#[cfg(feature = "udp")]
use ::tokio_util::{codec::BytesCodec, udp::UdpFramed};

#[cfg(feature = "udp")]
pub struct UdpTransport {
    udp: UdpFramed<BytesCodec, UdpSocket>,
    peer_addr: SocketAddr,
}

#[cfg(feature = "udp")]
impl UdpTransport {
    pub fn new(udp: UdpSocket, peer_addr: SocketAddr) -> Self {
        Self {
            peer_addr,
            udp: UdpFramed::new(udp, BytesCodec::new()),
        }
    }
}

#[cfg(feature = "udp")]
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
