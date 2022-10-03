use ::bytes::BytesMut;
use ::futures::{Sink, SinkExt, Stream, StreamExt};
use ::log::warn;
use ::once_cell::sync::Lazy;
use ::std::{
    marker::PhantomData,
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use ::tokio::{
    select,
    sync::Notify,
    time::{interval, MissedTickBehavior},
};
use ::tokio_util::sync::CancellationToken;

pub struct HalfClosePool {
    token: CancellationToken,
    notify: Arc<Notify>,
    task_count: Arc<AtomicUsize>,
}

static HALFCLOSE_POOL: Lazy<HalfClosePool> = Lazy::new(HalfClosePool::default);

impl Default for HalfClosePool {
    fn default() -> Self {
        Self {
            token: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            task_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl HalfClosePool {
    pub async fn create_task<T, Si>(
        transport: T,
        reset_packet: BytesMut,
        half_close_timeout: Duration,
    ) where
        T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin + 'static,
        Si: From<BytesMut> + Send + Unpin + 'static,
    {
        let this = HALFCLOSE_POOL.deref();

        // Create task.
        let mut task = HalfClose {
            transport,
            reset_packet,
            repeat: (half_close_timeout.as_secs() as usize).min(1),
            token: this.token.child_token(),
            notify: this.notify.clone(),
            counter: this.task_count.clone(),
            _phantom: Default::default(),
        };
        task.counter.fetch_add(1, Ordering::Relaxed);

        // Send the reset packet directly.
        task.repeat -= 1;
        if task.send().await.is_err() || task.repeat == 0 {
            return;
        }

        if this.token.is_cancelled() {
            warn!("The KCP stream half-close pool has been shut down.");
            return;
        }
        tokio::spawn(task.run());
    }

    pub async fn shutdown() {
        let this = HALFCLOSE_POOL.deref();
        this.token.cancel();
        while this.task_count.load(Ordering::Relaxed) > 0 {
            this.notify.notified().await;
        }
    }
}

struct HalfClose<T, Si> {
    transport: T,
    reset_packet: BytesMut,
    repeat: usize,
    token: CancellationToken,
    notify: Arc<Notify>,
    counter: Arc<AtomicUsize>,
    _phantom: PhantomData<Si>,
}

impl<T, Si> Drop for HalfClose<T, Si> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
        self.notify.notify_one();
    }
}

impl<T, Si> HalfClose<T, Si>
where
    T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin,
    Si: From<BytesMut> + Send + Unpin,
{
    async fn run(mut self) {
        let mut timer = interval(Duration::from_secs(1));
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        for _ in 0..self.repeat {
            select! {
                _ = timer.tick() => {
                    if self.send().await.is_err() {
                        break;
                    }
                }
                x = self.transport.next() => {
                    if x.is_none() {
                        break;
                    }
                }
                _ = self.token.cancelled() => break,
            }
        }
    }

    async fn send(&mut self) -> Result<(), ()> {
        select! {
            biased;
            x = self.transport.send(self.reset_packet.clone().into()) => x.map_err(|_| ()),
            _ = self.token.cancelled() => Err(()),
        }
    }
}
