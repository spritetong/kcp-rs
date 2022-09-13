#![allow(dead_code)]
#![allow(unused_imports)]

pub mod config;
pub mod kcp;
pub mod listener;
pub mod stream;

use ::bytes::{Buf, BufMut, Bytes, BytesMut};
use ::crossbeam::atomic::AtomicCell;
use ::futures::{ready, FutureExt, Sink, SinkExt, Stream, StreamExt};
use ::log::{debug, error, trace, warn};
use ::std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::{Deref, DerefMut},
    pin::Pin,
    slice, str,
    sync::atomic::{AtomicU32, Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use ::tokio::{
    net::UdpSocket,
    select,
    sync::mpsc::{channel, Receiver, Sender},
    task::JoinHandle,
};
use ::tokio_stream::wrappers::ReceiverStream;
use ::tokio_util::sync::{CancellationToken, PollSendError, PollSender};
