use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

pub struct DuplexTransport<Si, Item, St> {
    sink: Si,
    stream: St,
    _phantom: PhantomData<Item>,
}

impl<Si, Item, St> DuplexTransport<Si, Item, St> {
    pub fn new(sink: Si, stream: St) -> Self {
        Self {
            sink,
            stream,
            _phantom: Default::default(),
        }
    }

    pub fn get_sink(&self) -> &Si {
        &self.sink
    }

    pub fn get_sink_mut(&mut self) -> &mut Si {
        &mut self.sink
    }

    pub fn get_stream(&self) -> &St {
        &self.stream
    }

    pub fn get_stream_mut(&mut self) -> &mut St {
        &mut self.stream
    }

    pub fn split_into(self) -> (Si, St) {
        (self.sink, self.stream)
    }
}

impl<Si, Item, St> Sink<Item> for DuplexTransport<Si, Item, St>
where
    Si: Sink<Item> + Unpin,
    Item: Send + Unpin,
    St: Stream + Unpin,
{
    type Error = Si::Error;

    #[inline]
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx)
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_flush_unpin(cx)
    }

    #[inline]
    fn start_send(mut self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item)
    }

    #[inline]
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx)
    }
}

impl<Si, Item, St> Stream for DuplexTransport<Si, Item, St>
where
    Si: Sink<Item> + Unpin,
    Item: Send + Unpin,
    St: Stream + Unpin,
{
    type Item = St::Item;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}
