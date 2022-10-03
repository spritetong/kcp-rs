use futures::{Sink, SinkExt, Stream};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedSender};
use tokio_util::sync::PollSender;

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

    #[inline]
    pub fn get_sink(&self) -> &PollSender<S> {
        &self.sink
    }

    #[inline]
    pub fn get_sink_mut(&mut self) -> &mut PollSender<S> {
        &mut self.sink
    }

    #[inline]
    pub fn get_stream(&self) -> &Receiver<R> {
        &self.stream
    }

    #[inline]
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

pub struct UnboundedSink<T>(Option<UnboundedSender<T>>);

impl<T> UnboundedSink<T> {
    pub fn new(sender: UnboundedSender<T>) -> Self {
        Self(Some(sender))
    }

    #[inline]
    pub fn get_ref(&self) -> &Option<UnboundedSender<T>> {
        &self.0
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut Option<UnboundedSender<T>> {
        &mut self.0
    }

    #[inline]
    pub fn into_inner(self) -> Option<UnboundedSender<T>> {
        self.0
    }
}

impl<T> Sink<T> for UnboundedSink<T> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.0.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::ErrorKind::NotConnected.into()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.0.is_some() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::ErrorKind::NotConnected.into()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        if let Some(sender) = &self.0 {
            if sender.send(item).is_ok() {
                return Ok(());
            }
        }
        Err(io::ErrorKind::NotConnected.into())
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.0.take();
        Poll::Ready(Ok(()))
    }
}
