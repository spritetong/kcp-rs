#![allow(dead_code)]

pub mod config;
pub mod kcp;
pub mod stream;
pub mod udp;

pub use crate::config::{KcpConfig, KcpNoDelayConfig, KcpSessionKey};
pub use crate::stream::KcpStream;
pub use crate::udp::KcpUdpStream;

use ::bytes::{Bytes, BytesMut};
use ::futures::{
    future::{poll_fn, ready},
    ready, FutureExt, Sink, SinkExt, Stream, StreamExt,
};
use ::hashlink::LinkedHashMap;
use ::log::{error, trace, warn};
use ::std::{
    collections::VecDeque,
    fmt::Display,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use ::tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, ToSocketAddrs, UdpSocket},
    select,
    sync::mpsc::{channel, Receiver, Sender},
    task::JoinHandle,
};
use ::tokio_stream::wrappers::ReceiverStream;
use ::tokio_util::{
    codec::BytesCodec,
    sync::{CancellationToken, PollSendError, PollSender},
    udp::UdpFramed,
};
use ::zerocopy::{AsBytes, FromBytes};
