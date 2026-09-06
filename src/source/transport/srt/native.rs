//! Narrow safe ownership boundary around libSRT.
//!
//! The C shim includes the authoritative headers, so enum values and the large
//! statistics structure never have to be duplicated in Rust. No native socket
//! or pointer escapes this module.

use std::{
    ffi::{CStr, c_char, c_int},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicI32, Ordering},
    },
};

use parking_lot::Mutex;
use socket2::SockAddr;
use thiserror::Error;

const INVALID_SOCKET: i32 = -1;
const INVALID_POLL: i32 = -1;
const RECEIVE_ERROR: i32 = -1;
const RECEIVE_RETRY: i32 = -2;
const SOCKET_BROKEN: i32 = 6;
const SOCKET_CLOSING: i32 = 7;
const SOCKET_CLOSED: i32 = 8;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct NativeError {
    message: Box<str>,
}

impl NativeError {
    fn last(operation: &'static str) -> Self {
        // SAFETY: libSRT returns a process-owned, nul-terminated error string.
        let detail = unsafe {
            let pointer = rushls_srt_last_error();
            if pointer.is_null() {
                "unknown libSRT error".into()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }
        };
        Self {
            message: format!("{operation}: {detail}").into_boxed_str(),
        }
    }

    fn message(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[repr(C)]
struct Options {
    latency_ms: i32,
    peer_idle_timeout_ms: i32,
    receive_buffer_bytes: i32,
    payload_size: i32,
    minimum_peer_version: i32,
    passphrase: *const c_char,
    passphrase_length: i32,
    key_length: i32,
}

pub struct NativeOptions<'a> {
    pub latency_ms: i32,
    pub peer_idle_timeout_ms: i32,
    pub receive_buffer_bytes: i32,
    pub payload_size: i32,
    pub minimum_peer_version: i32,
    pub passphrase: Option<&'a [u8]>,
    pub key_length: i32,
}

impl NativeOptions<'_> {
    fn ffi(&self) -> Options {
        let (passphrase, passphrase_length) =
            self.passphrase.map_or((std::ptr::null(), 0), |passphrase| {
                (
                    passphrase.as_ptr().cast(),
                    i32::try_from(passphrase.len()).unwrap_or(i32::MAX),
                )
            });
        Options {
            latency_ms: self.latency_ms,
            peer_idle_timeout_ms: self.peer_idle_timeout_ms,
            receive_buffer_bytes: self.receive_buffer_bytes,
            payload_size: self.payload_size,
            minimum_peer_version: self.minimum_peer_version,
            passphrase,
            passphrase_length,
            key_length: self.key_length,
        }
    }
}

#[repr(C)]
struct Peer {
    address: [u8; 16],
    port: u16,
    is_ipv6: u8,
    scope_id: u32,
    protocol_version: u32,
    stream_id: [c_char; 513],
    stream_id_length: i32,
}

impl Peer {
    fn empty() -> Self {
        Self {
            address: [0; 16],
            port: 0,
            is_ipv6: 0,
            scope_id: 0,
            protocol_version: 0,
            stream_id: [0; 513],
            stream_id_length: 0,
        }
    }

    fn socket_address(&self) -> SocketAddr {
        if self.is_ipv6 == 0 {
            SocketAddrV4::new(
                Ipv4Addr::new(
                    self.address[0],
                    self.address[1],
                    self.address[2],
                    self.address[3],
                ),
                self.port,
            )
            .into()
        } else {
            SocketAddrV6::new(Ipv6Addr::from(self.address), self.port, 0, self.scope_id).into()
        }
    }

    fn stream_id(&self) -> Result<Box<str>, NativeError> {
        let length = usize::try_from(self.stream_id_length)
            .ok()
            .filter(|length| *length <= 512)
            .ok_or_else(|| NativeError::message("libSRT returned an invalid Stream ID length"))?;
        // SAFETY: `c_char` and `u8` have identical size and the C shim wrote
        // exactly `length` initialized bytes into this inline array.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.stream_id.as_ptr().cast::<u8>(), length) };
        std::str::from_utf8(bytes)
            .map(Box::<str>::from)
            .map_err(|_| NativeError::message("the SRT Stream ID is not valid UTF-8"))
    }
}

#[derive(Debug)]
struct Runtime;

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: the last listener/connection releases the matching startup.
        let _ = unsafe { rushls_srt_cleanup() };
    }
}

fn runtime() -> Result<Arc<Runtime>, NativeError> {
    static RUNTIME: OnceLock<Mutex<Weak<Runtime>>> = OnceLock::new();
    let mut runtime = RUNTIME.get_or_init(|| Mutex::new(Weak::new())).lock();
    if let Some(existing) = runtime.upgrade() {
        return Ok(existing);
    }

    // SAFETY: serialized by `RUNTIME`; a successful call is balanced by Drop.
    if unsafe { rushls_srt_startup() } != 0 {
        return Err(NativeError::last("initializing libSRT"));
    }
    let started = Arc::new(Runtime);
    *runtime = Arc::downgrade(&started);
    Ok(started)
}

#[derive(Debug)]
pub struct Socket {
    handle: AtomicI32,
    _runtime: Arc<Runtime>,
}

impl Socket {
    fn new(handle: i32, runtime: Arc<Runtime>) -> Self {
        Self {
            handle: AtomicI32::new(handle),
            _runtime: runtime,
        }
    }

    fn handle(&self) -> Result<i32, NativeError> {
        let handle = self.handle.load(Ordering::Acquire);
        if handle == INVALID_SOCKET {
            Err(NativeError::message("the libSRT socket is closed"))
        } else {
            Ok(handle)
        }
    }

    pub fn close(&self) {
        let handle = self.handle.swap(INVALID_SOCKET, Ordering::AcqRel);
        if handle != INVALID_SOCKET {
            // SAFETY: the atomic swap grants this call unique close ownership.
            let _ = unsafe { rushls_srt_close(handle) };
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct Listener {
    pub socket: Arc<Socket>,
    pub poll: Arc<Poll>,
    pub local_address: SocketAddr,
}

pub struct Connection {
    pub socket: Arc<Socket>,
    pub remote_address: SocketAddr,
    pub stream_id: Box<str>,
    pub protocol_version: u32,
}

#[derive(Debug)]
pub struct Poll {
    handle: i32,
    _runtime: Arc<Runtime>,
}

impl Drop for Poll {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the poll descriptor.
        let _ = unsafe { rushls_srt_poll_close(self.handle) };
    }
}

pub fn version() -> u32 {
    // SAFETY: this reads immutable library version state.
    unsafe { rushls_srt_version() }
}

pub fn open_listener(
    address: SocketAddr,
    options: &NativeOptions<'_>,
    backlog: usize,
) -> Result<Listener, NativeError> {
    let runtime = runtime()?;
    let address = SockAddr::from(address);
    let backlog = i32::try_from(backlog)
        .ok()
        .filter(|backlog| *backlog > 0)
        .ok_or_else(|| NativeError::message("the SRT listener backlog is invalid"))?;
    let mut handle = INVALID_SOCKET;
    let mut poll = INVALID_POLL;
    let mut local = Peer::empty();
    let options = options.ffi();
    // SAFETY: every pointer targets a live value for the duration of the call;
    // the shim copies the socket address and options synchronously.
    let result = unsafe {
        rushls_srt_listener_open(
            address.as_ptr().cast(),
            i32::try_from(address.len()).unwrap_or(i32::MAX),
            &raw const options,
            backlog,
            &raw mut handle,
            &raw mut poll,
            &raw mut local,
        )
    };
    if result != 0 {
        return Err(NativeError::last("binding the SRT listener"));
    }
    Ok(Listener {
        socket: Arc::new(Socket::new(handle, Arc::clone(&runtime))),
        poll: Arc::new(Poll {
            handle: poll,
            _runtime: runtime,
        }),
        local_address: local.socket_address(),
    })
}

pub enum Wait {
    Ready,
    Timeout,
}

pub fn wait(poll: &Poll, timeout: std::time::Duration) -> Result<Wait, NativeError> {
    let timeout = i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX);
    // SAFETY: `poll` stays alive through the shared reference.
    match unsafe { rushls_srt_listener_wait(poll.handle, timeout) } {
        1 => Ok(Wait::Ready),
        0 => Ok(Wait::Timeout),
        _ => Err(NativeError::last("waiting for an SRT connection")),
    }
}

pub fn accept(listener: &Socket) -> Result<Connection, NativeError> {
    let listener = listener.handle()?;
    let mut accepted = INVALID_SOCKET;
    let mut peer = Peer::empty();
    // SAFETY: output pointers are valid and libSRT initializes them on success.
    if unsafe { rushls_srt_accept(listener, &raw mut accepted, &raw mut peer) } != 0 {
        return Err(NativeError::last("accepting an SRT connection"));
    }
    let runtime = runtime()?;
    let stream_id = peer.stream_id()?;
    Ok(Connection {
        socket: Arc::new(Socket::new(accepted, runtime)),
        remote_address: peer.socket_address(),
        stream_id,
        protocol_version: peer.protocol_version,
    })
}

pub enum Receive {
    Data(usize),
    Retry,
    End,
}

pub fn receive(socket: &Socket, buffer: &mut [u8]) -> Result<Receive, NativeError> {
    let handle = socket.handle()?;
    let length = i32::try_from(buffer.len())
        .ok()
        .filter(|length| *length > 0)
        .ok_or_else(|| NativeError::message("the SRT receive buffer is invalid"))?;
    // SAFETY: `buffer` is writable for `length` bytes and the socket remains
    // alive through the shared reference.
    let result = unsafe { rushls_srt_recv(handle, buffer.as_mut_ptr(), length) };
    match result {
        RECEIVE_RETRY => Ok(Receive::Retry),
        RECEIVE_ERROR => Err(NativeError::last("receiving SRT media")),
        0 => Ok(Receive::End),
        received if received > 0 => Ok(Receive::Data(
            usize::try_from(received).expect("positive i32 fits usize"),
        )),
        _ => Err(NativeError::message(
            "libSRT returned an unknown receive result",
        )),
    }
}

pub fn ended_cleanly(socket: &Socket) -> bool {
    let Ok(handle) = socket.handle() else {
        return true;
    };
    // SAFETY: the socket remains alive through the shared reference.
    matches!(
        unsafe { rushls_srt_socket_state(handle) },
        SOCKET_CLOSING | SOCKET_CLOSED
    )
}

pub fn is_broken(socket: &Socket) -> bool {
    let Ok(handle) = socket.handle() else {
        return false;
    };
    // SAFETY: the socket remains alive through the shared reference.
    (unsafe { rushls_srt_socket_state(handle) }) == SOCKET_BROKEN
}

pub fn receive_loss_total(socket: &Socket) -> Option<u64> {
    let handle = socket.handle().ok()?;
    // SAFETY: the socket remains alive through the shared reference.
    u64::try_from(unsafe { rushls_srt_receive_loss_total(handle) }).ok()
}

pub fn test_connect(
    address: SocketAddr,
    options: &NativeOptions<'_>,
    stream_id: &str,
) -> Result<Arc<Socket>, NativeError> {
    let runtime = runtime()?;
    let address = SockAddr::from(address);
    let stream_id_length = i32::try_from(stream_id.len())
        .map_err(|_| NativeError::message("the test Stream ID is too large"))?;
    let options = options.ffi();
    let mut connected = INVALID_SOCKET;
    // SAFETY: every pointer remains valid and is copied during this call.
    if unsafe {
        rushls_srt_test_connect(
            address.as_ptr().cast(),
            i32::try_from(address.len()).unwrap_or(i32::MAX),
            &raw const options,
            stream_id.as_ptr().cast(),
            stream_id_length,
            &raw mut connected,
        )
    } != 0
    {
        return Err(NativeError::last("connecting the SRT test caller"));
    }
    Ok(Arc::new(Socket::new(connected, runtime)))
}

pub fn test_send(socket: &Socket, bytes: &[u8]) -> Result<(), NativeError> {
    let handle = socket.handle()?;
    let length = i32::try_from(bytes.len())
        .map_err(|_| NativeError::message("the SRT test message is too large"))?;
    // SAFETY: `bytes` is readable for `length` and retained for this call.
    let sent = unsafe { rushls_srt_test_send(handle, bytes.as_ptr(), length) };
    if sent == length {
        Ok(())
    } else {
        Err(NativeError::last("sending SRT test media"))
    }
}

unsafe extern "C" {
    fn rushls_srt_startup() -> c_int;
    fn rushls_srt_cleanup() -> c_int;
    fn rushls_srt_version() -> u32;
    fn rushls_srt_last_error() -> *const c_char;
    fn rushls_srt_listener_open(
        address: *const libc::sockaddr,
        address_length: c_int,
        options: *const Options,
        backlog: c_int,
        listener: *mut i32,
        poll: *mut i32,
        local: *mut Peer,
    ) -> c_int;
    fn rushls_srt_listener_wait(poll: i32, timeout_ms: i64) -> c_int;
    fn rushls_srt_poll_close(poll: i32) -> c_int;
    fn rushls_srt_accept(listener: i32, accepted: *mut i32, peer: *mut Peer) -> c_int;
    fn rushls_srt_recv(socket: i32, buffer: *mut u8, length: c_int) -> c_int;
    fn rushls_srt_socket_state(socket: i32) -> c_int;
    fn rushls_srt_receive_loss_total(socket: i32) -> i64;
    fn rushls_srt_close(socket: i32) -> c_int;

    fn rushls_srt_test_connect(
        address: *const libc::sockaddr,
        address_length: c_int,
        options: *const Options,
        stream_id: *const c_char,
        stream_id_length: c_int,
        connected: *mut i32,
    ) -> c_int;
    fn rushls_srt_test_send(socket: i32, buffer: *const u8, length: c_int) -> c_int;
}
