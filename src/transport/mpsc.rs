use futures::{Sink, SinkExt, Stream};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{Receiver, Sender};
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
