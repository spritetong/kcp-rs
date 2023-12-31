use ::bytes::{BufMut, Bytes, BytesMut};
use ::std::{
    collections::VecDeque,
    ffi::CStr,
    io,
    os::raw::{c_char, c_int, c_long, c_void},
    ptr::null_mut,
    slice, str,
    time::Instant,
};

#[path = "ffi.rs"]
mod ffi;
pub use ffi::*;

pub struct Kcp {
    handle: usize,
    time_base: Instant,
    buffer_size: (usize, usize),
    write_buf: BytesMut,
    read_buf: BytesMut,
    output_queue: VecDeque<BytesMut>,
}

impl Drop for Kcp {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { ikcp_release(self.as_mut()) }
            self.handle = 0;
            self.output_queue.clear();
        }
    }
}

impl Kcp {
    #[inline]
    fn as_ref(&self) -> &IKCPCB {
        unsafe { &*(self.handle as *const IKCPCB) }
    }

    #[inline]
    fn as_mut(&mut self) -> &mut IKCPCB {
        unsafe { &mut *(self.handle as *mut IKCPCB) }
    }
}

macro_rules! export_fields {
    ($($field:ident),+ $(,)?) => {
        $(
            #[inline]
            pub fn $field(&self) -> u32 {
                self.as_ref().$field as u32
            }
        )*
    };
}

impl Kcp {
    pub fn new(conv: u32) -> Self {
        Self {
            handle: unsafe { ikcp_create(conv, null_mut()) as *const _ as usize },
            time_base: Instant::now(),
            buffer_size: (32768, 32768),
            write_buf: BytesMut::new(),
            read_buf: BytesMut::new(),
            output_queue: VecDeque::with_capacity(32),
        }
    }

    pub fn get_system_time(&self) -> u32 {
        let elapsed = self.time_base.elapsed();
        (elapsed.as_secs() as u32)
            .wrapping_mul(1000)
            .wrapping_add(elapsed.subsec_millis())
    }

    /// # Warning
    ///
    /// After initialization, self must be ***pinned*** in memory.
    pub fn initialize(&mut self) {
        unsafe extern "C" fn _writelog(log: *const c_char, _kcp: *mut IKCPCB, _user: *mut c_void) {
            log::trace!(
                "{}",
                str::from_utf8_unchecked(CStr::from_ptr(log).to_bytes())
            );
        }

        unsafe extern "C" fn _output(
            buf: *const c_char,
            len: c_int,
            _kcp: *mut IKCPCB,
            user: *mut c_void,
        ) -> c_int {
            let this = &mut *(user as *const _ as *mut Kcp);
            let size = len as usize;
            if this.write_buf.capacity() < size {
                this.write_buf.reserve(this.buffer_size.0.max(size));
            }
            this.write_buf
                .put_slice(slice::from_raw_parts(buf as _, size));
            this.output_queue.push_back(this.write_buf.split_to(size));
            size as c_int
        }

        self.as_mut().user = self as *const _ as _;
        self.as_mut().output = Some(_output);
        self.as_mut().writelog = Some(_writelog);
        self.update(self.get_system_time());
    }

    /// io::ErrorKind::InvalidInput - buffer is too small to contain a frame.
    pub fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match unsafe { ikcp_recv(self.as_mut(), buf.as_mut_ptr() as _, -(buf.len() as c_int)) } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Ok(0),
            -3 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unreachable!(),
        }
    }

    /// io::ErrorKind::InvalidInput - buffer is too small to contain a frame.
    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match unsafe { ikcp_recv(self.as_mut(), buf.as_mut_ptr() as _, buf.len() as c_int) } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Ok(0),
            -3 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unreachable!(),
        }
    }

    pub fn recv_bytes(&mut self) -> Option<Bytes> {
        let size = self.peek_size();
        if size > 0 {
            if self.read_buf.capacity() < size {
                self.read_buf.reserve(self.buffer_size.1.max(size));
            }
            unsafe { self.read_buf.set_len(size) };
            let mut buf = self.read_buf.split_to(size);
            let _ = self.recv(&mut buf);
            Some(buf.freeze())
        } else {
            None
        }
    }

    #[inline]
    pub fn peek_size(&self) -> usize {
        unsafe { ikcp_peeksize(self.as_ref()).max(0) as usize }
    }

    /// io::ErrorKind::InvalidInput - frame is too large.
    pub fn send(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        match unsafe { ikcp_send(self.as_mut(), data.as_ptr() as _, data.len() as c_int) } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unreachable!(),
        }
    }

    /// ErrorKind::NotFound - conv is inconsistent
    ///
    /// ErrorKind::InvalidData - Invalid packet or unrecognized command
    pub fn input(&mut self, packet: &[u8]) -> io::Result<()> {
        match unsafe { ikcp_input(self.as_mut(), packet.as_ptr() as _, packet.len() as c_long) } {
            0 => Ok(()),
            -1 => Err(io::ErrorKind::NotFound.into()),
            -2 | -3 => Err(io::ErrorKind::InvalidData.into()),
            _ => unreachable!(),
        }
    }

    #[inline]
    pub fn flush(&mut self) {
        unsafe { ikcp_flush(self.as_mut()) }
    }

    #[inline]
    pub fn update(&mut self, current: u32) {
        unsafe { ikcp_update(self.as_mut(), current) }
    }

    #[inline]
    pub fn check(&self, current: u32) -> u32 {
        unsafe { ikcp_check(self.as_ref(), current) }
    }

    pub fn set_mtu(&mut self, mtu: u32) -> io::Result<()> {
        if self.as_ref().mtu == mtu {
            return Ok(());
        }
        match unsafe { ikcp_setmtu(self.as_mut(), mtu as c_int) } {
            0 => Ok(()),
            -1 => Err(io::ErrorKind::InvalidInput.into()),
            -2 => Err(io::ErrorKind::OutOfMemory.into()),
            _ => unreachable!(),
        }
    }

    pub fn set_nodelay(&mut self, nodelay: bool, interval: u32, resend: u32, nc: bool) {
        unsafe {
            ikcp_nodelay(
                self.as_mut(),
                nodelay.into(),
                interval as c_int,
                resend as c_int,
                nc.into(),
            );
        }
    }

    pub fn get_waitsnd(&self) -> u32 {
        unsafe { ikcp_waitsnd(self.as_ref()) as u32 }
    }

    pub fn set_wndsize(&mut self, sndwnd: u32, rcvwnd: u32) {
        unsafe {
            ikcp_wndsize(self.as_mut(), sndwnd as c_int, rcvwnd as c_int);
        }
    }
}

impl Kcp {
    export_fields! { conv, current, nsnd_que, nrcv_que, nrcv_buf }

    pub fn duration_since(&self, since: u32) -> u32 {
        (self.current().wrapping_sub(since) as i32).max(0) as u32
    }

    pub fn set_logmask(&mut self, logmask: u32) {
        self.as_mut().logmask = logmask as i32;
    }

    pub fn set_conv(&mut self, conv: u32) {
        self.as_mut().conv = conv;
    }

    pub fn set_stream(&mut self, stream: bool) {
        self.as_mut().stream = stream.into();
    }

    #[inline]
    pub fn is_dead_link(&self) -> bool {
        self.as_ref().state == u32::MAX
    }

    #[inline]
    pub fn is_recv_queue_full(&self) -> bool {
        self.as_ref().nrcv_que >= self.as_ref().rcv_wnd
    }

    #[inline]
    pub fn is_send_queue_full(&self) -> bool {
        //self.get_waitsnd() >= self.as_ref().snd_wnd
        self.as_ref().nsnd_que >= self.as_ref().snd_wnd
    }

    #[inline]
    pub fn has_ouput(&mut self) -> bool {
        !self.output_queue.is_empty()
    }

    #[inline]
    pub fn pop_output(&mut self) -> Option<BytesMut> {
        self.output_queue.pop_front()
    }

    pub fn write_ack_head(&self, buf: &mut BytesMut, cmd_flags: u8, payload_size: usize) {
        buf.reserve(IKCP_OVERHEAD as usize + payload_size);
        let kcp = self.as_ref();
        buf.put_u32_le(kcp.conv);
        buf.put_u8(IKCP_CMD_ACK as u8 | cmd_flags);
        buf.put_u8(0);
        buf.put_u16_le(kcp.rcv_wnd as u16);
        buf.put_u32_le(self.get_system_time());
        buf.put_u32_le(kcp.snd_nxt);
        buf.put_u32_le(kcp.rcv_nxt);
        buf.put_u32_le(0);
        buf.put_u32_le(payload_size as u32);
    }

    /// Read conv from a packet buffer.
    #[inline]
    pub fn read_conv(buf: &[u8]) -> Option<u32> {
        if buf.len() >= IKCP_OVERHEAD as usize {
            Some(unsafe {
                (*buf.get_unchecked(0) as u32)
                    | (*buf.get_unchecked(1) as u32).wrapping_shl(8)
                    | (*buf.get_unchecked(2) as u32).wrapping_shl(16)
                    | (*buf.get_unchecked(3) as u32).wrapping_shl(24)
            })
        } else {
            None
        }
    }

    /// Read cmd from a packet buffer.
    #[inline]
    pub fn read_cmd(buf: &[u8]) -> u8 {
        buf[4]
    }

    /// Write cmd to a packet buffer.
    #[inline]
    pub fn write_cmd(buf: &mut [u8], cmd: u8) {
        buf[4] = cmd;
    }

    /// Get the first segment payload from a packet buffer.
    #[inline]
    pub fn read_payload_data(buf: &[u8]) -> Option<&[u8]> {
        unsafe {
            let mut p = buf.as_ptr();
            let mut left = buf.len();
            while left >= IKCP_OVERHEAD as usize {
                let len = (*p.wrapping_add(IKCP_OVERHEAD as usize - 4) as usize)
                    | (*p.wrapping_add(IKCP_OVERHEAD as usize - 3) as usize).wrapping_shl(8)
                    | (*p.wrapping_add(IKCP_OVERHEAD as usize - 2) as usize).wrapping_shl(16)
                    | (*p.wrapping_add(IKCP_OVERHEAD as usize - 1) as usize).wrapping_shl(24);
                p = p.wrapping_add(IKCP_OVERHEAD as usize);
                left -= IKCP_OVERHEAD as usize;
                if (1..=left).contains(&len) {
                    return Some(slice::from_raw_parts(p, len));
                }
            }
        }
        None
    }
}

impl Kcp {
    /// The conv used for SYN handshake.
    pub const SYN_CONV: u32 = 0xFFFF_FFFE;

    /// Check if a conv is valid.
    #[inline]
    pub fn is_valid_conv(conv: u32) -> bool {
        conv != 0 && conv < Self::SYN_CONV
    }

    /// Generate a random conv.
    pub fn rand_conv() -> u32 {
        loop {
            let conv = rand::random();
            if Self::is_valid_conv(conv) {
                break conv;
            }
        }
    }

    /// Maximum size of a data frame.
    pub const fn max_frame_size(mtu: u32) -> u32 {
        (mtu - IKCP_OVERHEAD) * (IKCP_WND_RCV - 1)
    }
}
