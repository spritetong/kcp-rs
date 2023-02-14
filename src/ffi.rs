#![allow(dead_code)]
#![allow(improper_ctypes)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

pub const IKCP_LOG_OUTPUT: u32 = 1;
pub const IKCP_LOG_INPUT: u32 = 2;
pub const IKCP_LOG_SEND: u32 = 4;
pub const IKCP_LOG_RECV: u32 = 8;
pub const IKCP_LOG_IN_DATA: u32 = 16;
pub const IKCP_LOG_IN_ACK: u32 = 32;
pub const IKCP_LOG_IN_PROBE: u32 = 64;
pub const IKCP_LOG_IN_WINS: u32 = 128;
pub const IKCP_LOG_OUT_DATA: u32 = 256;
pub const IKCP_LOG_OUT_ACK: u32 = 512;
pub const IKCP_LOG_OUT_PROBE: u32 = 1024;
pub const IKCP_LOG_OUT_WINS: u32 = 2048;
pub type ISTDUINT32 = ::std::os::raw::c_uint;
pub type ISTDINT32 = ::std::os::raw::c_int;
pub type IINT32 = ISTDINT32;
pub type IUINT32 = ISTDUINT32;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct IQUEUEHEAD {
    pub next: *mut IQUEUEHEAD,
    pub prev: *mut IQUEUEHEAD,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct IKCPCB {
    pub conv: IUINT32,
    pub mtu: IUINT32,
    pub mss: IUINT32,
    pub state: IUINT32,
    pub snd_una: IUINT32,
    pub snd_nxt: IUINT32,
    pub rcv_nxt: IUINT32,
    pub ts_recent: IUINT32,
    pub ts_lastack: IUINT32,
    pub ssthresh: IUINT32,
    pub rx_rttval: IINT32,
    pub rx_srtt: IINT32,
    pub rx_rto: IINT32,
    pub rx_minrto: IINT32,
    pub snd_wnd: IUINT32,
    pub rcv_wnd: IUINT32,
    pub rmt_wnd: IUINT32,
    pub cwnd: IUINT32,
    pub probe: IUINT32,
    pub current: IUINT32,
    pub interval: IUINT32,
    pub ts_flush: IUINT32,
    pub xmit: IUINT32,
    pub nrcv_buf: IUINT32,
    pub nsnd_buf: IUINT32,
    pub nrcv_que: IUINT32,
    pub nsnd_que: IUINT32,
    pub nodelay: IUINT32,
    pub updated: IUINT32,
    pub ts_probe: IUINT32,
    pub probe_wait: IUINT32,
    pub dead_link: IUINT32,
    pub incr: IUINT32,
    pub snd_queue: IQUEUEHEAD,
    pub rcv_queue: IQUEUEHEAD,
    pub snd_buf: IQUEUEHEAD,
    pub rcv_buf: IQUEUEHEAD,
    pub acklist: *mut IUINT32,
    pub ackcount: IUINT32,
    pub ackblock: IUINT32,
    pub user: *mut ::std::os::raw::c_void,
    pub buffer: *mut ::std::os::raw::c_char,
    pub fastresend: ::std::os::raw::c_int,
    pub fastlimit: ::std::os::raw::c_int,
    pub nocwnd: ::std::os::raw::c_int,
    pub stream: ::std::os::raw::c_int,
    pub logmask: ::std::os::raw::c_int,
    pub output: ::std::option::Option<
        unsafe extern "C" fn(
            buf: *const ::std::os::raw::c_char,
            len: ::std::os::raw::c_int,
            kcp: *mut IKCPCB,
            user: *mut ::std::os::raw::c_void,
        ) -> ::std::os::raw::c_int,
    >,
    pub writelog: ::std::option::Option<
        unsafe extern "C" fn(
            log: *const ::std::os::raw::c_char,
            kcp: *mut IKCPCB,
            user: *mut ::std::os::raw::c_void,
        ),
    >,
}
pub type ikcpcb = IKCPCB;
extern "C" {
    pub fn ikcp_create(conv: IUINT32, user: *mut ::std::os::raw::c_void) -> *mut ikcpcb;
}
extern "C" {
    pub fn ikcp_release(kcp: *mut ikcpcb);
}
extern "C" {
    pub fn ikcp_setoutput(
        kcp: *mut ikcpcb,
        output: ::std::option::Option<
            unsafe extern "C" fn(
                buf: *const ::std::os::raw::c_char,
                len: ::std::os::raw::c_int,
                kcp: *mut ikcpcb,
                user: *mut ::std::os::raw::c_void,
            ) -> ::std::os::raw::c_int,
        >,
    );
}
extern "C" {
    pub fn ikcp_recv(
        kcp: *mut ikcpcb,
        buffer: *mut ::std::os::raw::c_char,
        len: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_send(
        kcp: *mut ikcpcb,
        buffer: *const ::std::os::raw::c_char,
        len: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_update(kcp: *mut ikcpcb, current: IUINT32);
}
extern "C" {
    pub fn ikcp_check(kcp: *const ikcpcb, current: IUINT32) -> IUINT32;
}
extern "C" {
    pub fn ikcp_input(
        kcp: *mut ikcpcb,
        data: *const ::std::os::raw::c_char,
        size: ::std::os::raw::c_long,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_flush(kcp: *mut ikcpcb);
}
extern "C" {
    pub fn ikcp_peeksize(kcp: *const ikcpcb) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_setmtu(kcp: *mut ikcpcb, mtu: ::std::os::raw::c_int) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_wndsize(
        kcp: *mut ikcpcb,
        sndwnd: ::std::os::raw::c_int,
        rcvwnd: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_waitsnd(kcp: *const ikcpcb) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_nodelay(
        kcp: *mut ikcpcb,
        nodelay: ::std::os::raw::c_int,
        interval: ::std::os::raw::c_int,
        resend: ::std::os::raw::c_int,
        nc: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
extern "C" {
    pub fn ikcp_log(
        kcp: *mut ikcpcb,
        mask: ::std::os::raw::c_int,
        fmt: *const ::std::os::raw::c_char,
        ...
    );
}
extern "C" {
    pub fn ikcp_allocator(
        new_malloc: ::std::option::Option<
            unsafe extern "C" fn(arg1: usize) -> *mut ::std::os::raw::c_void,
        >,
        new_free: ::std::option::Option<unsafe extern "C" fn(arg1: *mut ::std::os::raw::c_void)>,
    );
}
extern "C" {
    pub fn ikcp_getconv(ptr: *const ::std::os::raw::c_void) -> IUINT32;
}
pub const IKCP_RTO_NDL: IUINT32 = 30;
pub const IKCP_RTO_MIN: IUINT32 = 100;
pub const IKCP_RTO_DEF: IUINT32 = 200;
pub const IKCP_RTO_MAX: IUINT32 = 60000;
pub const IKCP_CMD_PUSH: IUINT32 = 81;
pub const IKCP_CMD_ACK: IUINT32 = 82;
pub const IKCP_CMD_WASK: IUINT32 = 83;
pub const IKCP_CMD_WINS: IUINT32 = 84;
pub const IKCP_ASK_SEND: IUINT32 = 1;
pub const IKCP_ASK_TELL: IUINT32 = 2;
pub const IKCP_WND_SND: IUINT32 = 32;
pub const IKCP_WND_RCV: IUINT32 = 256;
pub const IKCP_MTU_DEF: IUINT32 = 1400;
pub const IKCP_ACK_FAST: IUINT32 = 3;
pub const IKCP_INTERVAL: IUINT32 = 100;
pub const IKCP_OVERHEAD: IUINT32 = 24;
pub const IKCP_DEADLINK: IUINT32 = 20;
pub const IKCP_THRESH_INIT: IUINT32 = 2;
pub const IKCP_THRESH_MIN: IUINT32 = 2;
pub const IKCP_PROBE_INIT: IUINT32 = 7000;
pub const IKCP_PROBE_LIMIT: IUINT32 = 120000;
pub const IKCP_FASTACK_LIMIT: IUINT32 = 5;
