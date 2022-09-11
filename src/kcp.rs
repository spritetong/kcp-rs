use bytes::{Buf, Bytes};
use std::{
    collections::VecDeque,
    ffi::CStr,
    io,
    ops::{Deref, DerefMut},
    os::raw::{c_char, c_int, c_long, c_void},
    ptr::null_mut,
    slice, str,
    time::Instant,
};

pub use crate::ffi::*;

pub struct Kcp {
    handle: usize,
    time_base: Instant,
    output_queue: VecDeque<Bytes>,
}

impl Deref for Kcp {
    type Target = IKCPCB;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.handle as *const IKCPCB) }
    }
}

impl DerefMut for Kcp {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *(self.handle as *mut IKCPCB) }
    }
}

impl Drop for Kcp {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { ikcp_release(self.deref_mut()) }
            self.handle = 0;
            self.output_queue.clear();
        }
    }
}

impl Kcp {
    pub fn new(conv: u32) -> Self {
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
            this.output_queue
                .push_back(Bytes::copy_from_slice(slice::from_raw_parts(
                    buf as _,
                    len as usize,
                )));
            len
        }

        let handle = unsafe {
            let kcp = &mut *ikcp_create(conv, null_mut());
            // Output is called only in flush.
            kcp.output = Some(_output);
            kcp.writelog = Some(_writelog);
            kcp as *const _ as usize
        };

        Self {
            handle,
            time_base: Instant::now(),
            output_queue: VecDeque::with_capacity(32),
        }
    }

    pub fn current(&self) -> u32 {
        let elapsed = self.time_base.elapsed();
        (elapsed.as_secs() as u32)
            .wrapping_mul(1000)
            .wrapping_add(elapsed.subsec_millis())
    }

    pub fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match unsafe {
            ikcp_recv(
                self.deref_mut(),
                buf.as_mut_ptr() as _,
                -(buf.len() as c_int),
            )
        } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Ok(0),
            -3 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unimplemented!(),
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match unsafe { ikcp_recv(self.deref_mut(), buf.as_mut_ptr() as _, buf.len() as c_int) } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Ok(0),
            -3 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unimplemented!(),
        }
    }

    pub fn peeksize(&self) -> usize {
        unsafe { ikcp_peeksize(self.deref()).max(0) as usize }
    }

    pub fn send(&mut self, data: &[u8]) -> io::Result<usize> {
        match unsafe { ikcp_send(self.deref_mut(), data.as_ptr() as _, data.len() as c_int) } {
            size if size >= 0 => Ok(size as usize),
            -1 | -2 => Err(io::ErrorKind::InvalidInput.into()),
            _ => unimplemented!(),
        }
    }

    /// ErrorKind::NotFound - conv is inconsistent
    ///
    /// ErrorKind::InvalidData - Invalid packet or unrecognized CMD
    pub fn input(&mut self, packet: &[u8]) -> io::Result<()> {
        match unsafe {
            ikcp_input(
                self.deref_mut(),
                packet.as_ptr() as _,
                packet.len() as c_long,
            )
        } {
            0 => Ok(()),
            -1 => Err(io::ErrorKind::NotFound.into()),
            -2 | -3 => Err(io::ErrorKind::InvalidData.into()),
            _ => unimplemented!(),
        }
    }

    pub fn flush(&mut self) {
        unsafe {
            // Set self as "user".
            self.user = self as *const _ as _;
            ikcp_flush(self.deref_mut());
        }
    }

    #[inline]
    pub fn pop_output(&mut self) -> Option<Bytes> {
        self.output_queue.pop_front()
    }

    pub fn update(&mut self, current: u32) {
        unsafe { ikcp_update(self.deref_mut(), current) }
    }

    pub fn check(&self, current: u32) -> u32 {
        unsafe { ikcp_check(self.deref(), current) }
    }

    pub fn set_mtu(&mut self, mtu: u32) -> io::Result<()> {
        match unsafe { ikcp_setmtu(self.deref_mut(), mtu as c_int) } {
            0 => Ok(()),
            -1 => Err(io::ErrorKind::InvalidInput.into()),
            -2 => Err(io::ErrorKind::OutOfMemory.into()),
            _ => unimplemented!(),
        }
    }

    pub fn set_nodelay(&mut self, nodelay: bool, interval: u32, resend: u32, nc: bool) {
        unsafe {
            ikcp_nodelay(
                self.deref_mut(),
                if nodelay { 1 } else { 0 },
                interval as c_int,
                resend as c_int,
                if nc { 1 } else { 0 },
            );
        }
    }

    pub fn get_waitsnd(&self) -> u32 {
        unsafe { ikcp_waitsnd(self.deref()) as u32 }
    }

    pub fn set_wndsize(&mut self, sndwnd: u32, rcvwnd: u32) {
        unsafe {
            ikcp_wndsize(self.deref_mut(), sndwnd as c_int, rcvwnd as c_int);
        }
    }

    /// Read conv from a packet buffer.
    pub fn read_conv(mut buf: &[u8]) -> Option<u32> {
        if buf.len() >= 24 {
            Some(buf.get_u32_le())
        } else {
            None
        }
    }
}
