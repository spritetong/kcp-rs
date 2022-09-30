#![allow(dead_code)]

pub mod config;
pub mod protocol;

pub use crate::config::{KcpConfig, KcpNoDelayConfig};

#[cfg(feature = "stream")]
pub mod transport;

#[cfg(feature = "stream")]
pub mod stream;
#[cfg(feature = "stream")]
pub use crate::stream::{kcp_sys_shutdown, KcpStream};

#[cfg(feature = "udp")]
pub mod udp;
#[cfg(feature = "udp")]
pub use crate::udp::KcpUdpStream;
