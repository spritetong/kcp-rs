// ! This file is referenced from https://github.com/Matrix-Zhang/tokio_kcp

use ::bytes::Bytes;
use ::std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct KcpNoDelayConfig {
    /// Enable nodelay
    pub nodelay: bool,
    /// Internal update interval (ms)
    pub interval: u32,
    /// ACK number to enable fast resend
    pub resend: u32,
    /// Disable congetion control
    pub nc: bool,
}

impl Default for KcpNoDelayConfig {
    fn default() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 100,
            resend: 0,
            nc: false,
        }
    }
}

impl KcpNoDelayConfig {
    /// Get a fastest configuration
    ///
    /// 1. Enable NoDelay
    /// 2. Set ticking interval to be 10ms
    /// 3. Set fast resend to be 2
    /// 4. Disable congestion control
    pub fn fastest() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: true,
            interval: 10,
            resend: 2,
            nc: true,
        }
    }

    /// Get a normal configuration
    ///
    /// 1. Disable NoDelay
    /// 2. Set ticking interval to be 40ms
    /// 3. Disable fast resend
    /// 4. Enable congestion control
    pub fn normal() -> KcpNoDelayConfig {
        KcpNoDelayConfig {
            nodelay: false,
            interval: 40,
            resend: 0,
            nc: false,
        }
    }
}

/// Kcp Config
#[derive(Debug, Clone)]
pub struct KcpConfig {
    /// Max Transmission Unit
    pub mtu: u32,
    /// nodelay
    pub nodelay: KcpNoDelayConfig,
    /// send window size
    pub snd_wnd: u32,
    /// recv window size
    pub rcv_wnd: u32,
    /// Stream mode
    pub stream: bool,
    /// Connect timeout, default is 15 seconds
    pub connect_timeout: Duration,
    /// Session key
    pub session_key: Bytes,
    /// Length of session ID
    pub session_id_len: usize,
    /// Session expire duration, default is 90 seconds
    pub session_expire: Duration,
}

impl Default for KcpConfig {
    fn default() -> KcpConfig {
        KcpConfig {
            mtu: 1400,
            nodelay: KcpNoDelayConfig::normal(),
            snd_wnd: 32,
            rcv_wnd: 256,
            stream: true,
            connect_timeout: Duration::from_secs(15),
            session_key: Bytes::new(),
            session_id_len: 16,
            session_expire: Duration::from_secs(90),
        }
    }
}

impl KcpConfig {
    pub fn random_session_id(&self) -> Vec<u8> {
        std::iter::repeat_with(|| rand::random::<u8>())
            .take(self.session_id_len)
            .collect()        
    }
}
