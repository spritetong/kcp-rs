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
        mut transport: T,
        reset_packet: BytesMut,
        half_close_timeout: Duration,
    ) where
        T: Sink<Si> + Stream<Item = BytesMut> + Send + Unpin + 'static,
        Si: From<BytesMut> + Send + Unpin + 'static,
    {
        let this = HALFCLOSE_POOL.deref();

        // Create task.
        let mut task = HalfClose {
            reset_packet,
            repeat: (half_close_timeout.as_secs() as usize).max(1),
            token: this.token.child_token(),
            notify: this.notify.clone(),
            counter: this.task_count.clone(),
            _phantom: Default::default(),
        };
        task.counter.fetch_add(1, Ordering::Relaxed);

        // Send the reset packet directly.
        task.repeat -= 1;
        if task.send(&mut transport).await.is_err() || task.repeat == 0 {
            return;
        }

        if this.token.is_cancelled() {
            warn!("The KCP stream half-close pool has been shut down.");
            return;
        }
        tokio::spawn(task.run(transport));
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
    reset_packet: BytesMut,
    repeat: usize,
    token: CancellationToken,
    notify: Arc<Notify>,
    counter: Arc<AtomicUsize>,
    _phantom: PhantomData<(T, Si)>,
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
    async fn run(mut self, mut transport: T) {
        let mut timer = interval(Duration::from_secs(1));
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut is_closed = false;
        while self.repeat > 0 {
            select! {
                biased;
                _ = self.token.cancelled() => break,
                _ = timer.tick() => {
                    if self.send(&mut transport).await.is_err() {
                        break;
                    }
                    self.repeat -= 1;
                }
                x = transport.next(), if !is_closed => {
                    if x.is_none() {
                        is_closed = true;
                    }
                }
            }
        }

        // Drop transport before self.
        drop(transport);
        drop(self);
    }

    async fn send(&mut self, transport: &mut T) -> Result<(), ()> {
        select! {
            biased;
            x = transport.send(self.reset_packet.clone().into()) => x.map_err(|_| ()),
            _ = self.token.cancelled() => Err(()),
        }
    }
}
