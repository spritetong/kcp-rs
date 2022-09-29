use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

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
