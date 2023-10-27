use futures::{Sink, Stream};
use futures_util::stream::FusedStream;
use pin_project::pin_project;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

#[pin_project]
pub struct DuplexStream<Si, Item, St> {
    #[pin]
    sink: Si,
    #[pin]
    stream: St,
    _phantom: PhantomData<Item>,
}

impl<Si, Item, St> DuplexStream<Si, Item, St> {
    pub fn new(sink: Si, stream: St) -> Self {
        Self {
            sink,
            stream,
            _phantom: Default::default(),
        }
    }

    /// Acquires a reference to the underlying sink that this combinator is
    /// pulling from.
    #[inline]
    pub fn get_sink(&self) -> &Si {
        &self.sink
    }

    /// Acquires a mutable reference to the underlying sink that this
    /// combinator is pulling from.
    #[inline]
    pub fn get_sink_mut(&mut self) -> &mut Si {
        &mut self.sink
    }

    /// Acquires a pinned mutable reference to the underlying sink that this
    /// combinator is pulling from.
    #[inline]
    pub fn get_sink_pin_mut(self: Pin<&mut Self>) -> Pin<&mut Si> {
        self.project().sink
    }

    /// Acquires a reference to the underlying stream that this combinator is
    /// pulling from.
    #[inline]
    pub fn get_stream(&self) -> &St {
        &self.stream
    }

    /// Acquires a mutable reference to the underlying stream that this
    /// combinator is pulling from.
    #[inline]
    pub fn get_stream_mut(&mut self) -> &mut St {
        &mut self.stream
    }

    /// Acquires a pinned mutable reference to the underlying stream that this
    /// combinator is pulling from.
    #[inline]
    pub fn get_stream_pin_mut(self: Pin<&mut Self>) -> Pin<&mut St> {
        self.project().stream
    }

    /// Consumes this combinator, returning the underlying sink and stream.
    #[inline]
    pub fn split_into(self) -> (Si, St) {
        (self.sink, self.stream)
    }
}

impl<Si: Sink<Item>, Item, St: Stream> Sink<Item> for DuplexStream<Si, Item, St> {
    type Error = Si::Error;

    #[inline]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().project().sink.poll_ready(cx)
    }

    #[inline]
    fn start_send(mut self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        self.as_mut().project().sink.start_send(item)
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().project().sink.poll_flush(cx)
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().project().sink.poll_close(cx)
    }
}

// Forwarding impl of Stream from the underlying sink
impl<Si: Sink<Item>, Item, St: Stream> Stream for DuplexStream<Si, Item, St> {
    type Item = St::Item;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().project().stream.poll_next(cx)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

// Forwarding impl of FusedStream from the underlying sink
impl<Si: Sink<Item>, Item, St: FusedStream> FusedStream for DuplexStream<Si, Item, St> {
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated()
    }
}
