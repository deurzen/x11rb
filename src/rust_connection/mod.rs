//! A pure-rust implementation of a connection to an X11 server.

use std::convert::{TryFrom, TryInto};
use std::io::{BufReader, BufWriter, IoSlice, Read, Write};
use std::sync::{Condvar, Mutex, MutexGuard, TryLockError};

use crate::bigreq::{ConnectionExt as _, EnableReply};
use crate::connection::{
    compute_length_field, Connection, DiscardMode, RequestConnection, RequestKind, SequenceNumber,
};
use crate::cookie::{Cookie, CookieWithFds, VoidCookie};
pub use crate::errors::{ConnectError, ConnectionError, ParseError};
use crate::extension_manager::ExtensionManager;
use crate::utils::RawFdContainer;
use crate::x11_utils::ExtensionInformation;
use crate::xproto::Setup;

mod id_allocator;
mod inner;
mod parse_display;
mod stream;
mod xauth;

use inner::PollReply;

type Buffer = <RustConnection as RequestConnection>::Buf;
pub type ReplyOrIdError = crate::errors::ReplyOrIdError<Buffer>;
pub type ReplyError = crate::errors::ReplyError<Buffer>;
pub type GenericError = crate::x11_utils::GenericError<Buffer>;
pub type GenericEvent = crate::x11_utils::GenericEvent<Buffer>;
pub type EventAndSeqNumber = crate::connection::EventAndSeqNumber<Buffer>;
pub type BufWithFds = crate::connection::BufWithFds<Buffer>;
pub type Error = crate::Error<Buffer>;
pub type Event = crate::Event<Buffer>;

#[derive(Debug)]
enum MaxRequestBytes {
    Unknown,
    Requested(Option<SequenceNumber>),
    Known(usize),
}

type MutexGuardInner<'a, W> = MutexGuard<'a, inner::ConnectionInner<W>>;

/// A connection to an X11 server implemented in pure rust
#[derive(Debug)]
pub struct RustConnection<R: Read = BufReader<stream::Stream>, W: Write = BufWriter<stream::Stream>>
{
    inner: Mutex<inner::ConnectionInner<W>>,
    read: Mutex<R>,
    reader_condition: Condvar,
    id_allocator: Mutex<id_allocator::IDAllocator>,
    setup: Setup,
    extension_manager: Mutex<ExtensionManager>,
    maximum_request_bytes: Mutex<MaxRequestBytes>,
}

impl RustConnection<BufReader<stream::Stream>, BufWriter<stream::Stream>> {
    /// Establish a new connection.
    ///
    /// If no `dpy_name` is provided, the value from `$DISPLAY` is used.
    pub fn connect(dpy_name: Option<&str>) -> Result<(Self, usize), ConnectError> {
        // Parse display information
        let parsed_display =
            parse_display::parse_display(dpy_name).ok_or(ConnectError::DisplayParsingError)?;

        // Establish connection
        let protocol = parsed_display.protocol.as_ref().map(|s| &**s);
        let stream =
            stream::Stream::connect(&*parsed_display.host, protocol, parsed_display.display)?;
        let screen = parsed_display.screen.into();

        let (family, address) = stream.peer_addr()?;
        let (auth_name, auth_data) = xauth::get_auth(family, &address, parsed_display.display)
            // Ignore all errors while determining auth; instead we just try without auth info.
            .unwrap_or(None)
            .unwrap_or_else(|| (Vec::new(), Vec::new()));

        let write = BufWriter::new(stream.try_clone()?);
        let read = BufReader::new(stream);
        Ok((
            Self::connect_to_stream_with_auth_info(read, write, screen, auth_name, auth_data)?,
            screen,
        ))
    }
}

impl<R: Read, W: Write> RustConnection<R, W> {
    /// Establish a new connection to the given streams.
    ///
    /// `read` is used for reading data from the X11 server and `write` is used for writing.
    /// `screen` is the number of the screen that should be used. This function checks that a
    /// screen with that number exists.
    pub fn connect_to_stream(read: R, write: W, screen: usize) -> Result<Self, ConnectError> {
        Self::connect_to_stream_with_auth_info(read, write, screen, Vec::new(), Vec::new())
    }

    /// Establish a new connection to the given streams.
    ///
    /// `read` is used for reading data from the X11 server and `write` is used for writing.
    /// `screen` is the number of the screen that should be used. This function checks that a
    /// screen with that number exists.
    ///
    /// The parameters `auth_name` and `auth_data` are used for the members
    /// `authorization_protocol_name` and `authorization_protocol_data` of the `SetupRequest` that
    /// is sent to the X11 server.
    pub fn connect_to_stream_with_auth_info(
        mut read: R,
        write: W,
        screen: usize,
        auth_name: Vec<u8>,
        auth_data: Vec<u8>,
    ) -> Result<Self, ConnectError> {
        let (inner, setup) =
            inner::ConnectionInner::connect(&mut read, write, auth_name, auth_data)?;

        // Check that we got a valid screen number
        if screen >= setup.roots.len() {
            return Err(ConnectError::InvalidScreen);
        }

        // Success! Set up our state
        Self::for_inner(read, inner, setup)
    }

    /// Establish a new connection for an already connected stream.
    ///
    /// `read` is used for reading data from the X11 server and `write` is used for writing.
    /// It is assumed that `setup` was just received from the server. Thus, the first reply to a
    /// request that is sent will have sequence number one.
    pub fn for_connected_stream(read: R, write: W, setup: Setup) -> Result<Self, ConnectError> {
        Self::for_inner(read, inner::ConnectionInner::new(write), setup)
    }

    fn for_inner(
        read: R,
        inner: inner::ConnectionInner<W>,
        setup: Setup,
    ) -> Result<Self, ConnectError> {
        let allocator =
            id_allocator::IDAllocator::new(setup.resource_id_base, setup.resource_id_mask)?;
        Ok(RustConnection {
            inner: Mutex::new(inner),
            read: Mutex::new(read),
            reader_condition: Condvar::new(),
            id_allocator: Mutex::new(allocator),
            setup,
            extension_manager: Default::default(),
            maximum_request_bytes: Mutex::new(MaxRequestBytes::Unknown),
        })
    }

    /// Internal function for actually sending a request.
    ///
    /// This function "does the actual work" for `send_request_with_reply()` and
    /// `send_request_without_reply()`.
    fn send_request(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
        kind: RequestKind,
    ) -> Result<SequenceNumber, ConnectionError> {
        if !fds.is_empty() {
            return Err(ConnectionError::FDPassingFailed);
        }
        self.inner
            .lock()
            .unwrap()
            .send_request(bufs, kind)
            .or(Err(ConnectionError::UnknownError))
    }

    /// Read a packet from the connection.
    ///
    /// This function waits for an X11 packet to be received. It drops the mutex protecting the
    /// inner data while waiting for a packet so that other threads can make progress. For this
    /// reason, you need to pass in a `MutexGuard` to be dropped. This function locks the mutex
    /// again and returns a new `MutexGuard`.
    fn read_packet_and_enqueue<'a>(
        &'a self,
        mut inner: MutexGuardInner<'a, W>,
    ) -> Result<MutexGuardInner<'a, W>, std::io::Error> {
        // 0.1. Try to lock the `read` mutex.
        match self.read.try_lock() {
            Err(TryLockError::WouldBlock) => {
                // 1.1. Someone else is reading (other thread is at 2.2);
                // wait for it. `Condvar::wait` will unlock `inner`, so
                // the other thread can relock `inner` at 2.3 (and to allow
                // other threads to arrive 0.1).
                //
                // When `wait` finishes, other thread has enqueued a packet,
                // so the purpose of this function has been fulfilled. `wait`
                // will relock `inner` when it returns.
                Ok(self.reader_condition.wait(inner).unwrap())
            }
            Err(TryLockError::Poisoned(e)) => panic!("{}", e),
            Ok(mut lock) => {
                // 2.1. Drop inner so other threads can use it while
                // `read_packet` is blocking.
                drop(inner);

                // 2.2. Block the thread until a packet is received.
                let packet = read_packet(&mut *lock)?;

                // 2.3. Relock `inner` to enqueue the packet.
                inner = self.inner.lock().unwrap();

                // 2.4. Once `inner` has been relocked, drop the
                // lock on `read`. While inner is locked, other
                // threads cannot arrive 0.1 anyways.
                //
                // `read` cannot unlocked before `inner` is relocked
                // because it could let another thread wait on 2.2
                // for a reply that has been read but not enqueued yet.
                drop(lock);

                // 2.5. Actually enqueue the read packet.
                inner.enqueue_packet(packet);

                // 2.6. Notify threads that a packet has been enqueued,
                // so other threads waiting on 1.1 can return.
                self.reader_condition.notify_all();

                // 2.7. Return the locked `inner` to the caller.
                Ok(inner)
            }
        }
    }

    fn prefetch_maximum_request_bytes_impl(&self, max_bytes: &mut MutexGuard<'_, MaxRequestBytes>) {
        if let MaxRequestBytes::Unknown = **max_bytes {
            let request = self
                .bigreq_enable()
                .map(|cookie| cookie.into_sequence_number())
                .ok();
            **max_bytes = MaxRequestBytes::Requested(request);
        }
    }
}

impl<R: Read, W: Write> RequestConnection for RustConnection<R, W> {
    type Buf = Vec<u8>;

    fn send_request_with_reply<Reply>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<Cookie<'_, Self, Reply>, ConnectionError>
    where
        Reply: for<'a> TryFrom<&'a [u8], Error = ParseError>,
    {
        let mut storage = Default::default();
        let bufs = compute_length_field(self, bufs, &mut storage)?;

        Ok(Cookie::new(
            self,
            self.send_request(bufs, fds, RequestKind::HasResponse)?,
        ))
    }

    fn send_request_with_reply_with_fds<Reply>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<CookieWithFds<'_, Self, Reply>, ConnectionError>
    where
        Reply: for<'a> TryFrom<(&'a [u8], Vec<RawFdContainer>), Error = ParseError>,
    {
        let mut storage = Default::default();
        let bufs = compute_length_field(self, bufs, &mut storage)?;

        let _ = (bufs, fds);
        Err(ConnectionError::FDPassingFailed)
    }

    fn send_request_without_reply(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<VoidCookie<'_, Self>, ConnectionError> {
        let mut storage = Default::default();
        let bufs = compute_length_field(self, bufs, &mut storage)?;

        Ok(VoidCookie::new(
            self,
            self.send_request(bufs, fds, RequestKind::IsVoid)?,
        ))
    }

    fn discard_reply(&self, sequence: SequenceNumber, _kind: RequestKind, mode: DiscardMode) {
        self.inner.lock().unwrap().discard_reply(sequence, mode);
    }

    fn prefetch_extension_information(
        &self,
        extension_name: &'static str,
    ) -> Result<(), ConnectionError> {
        self.extension_manager
            .lock()
            .unwrap()
            .prefetch_extension_information(self, extension_name)
    }

    fn extension_information(
        &self,
        extension_name: &'static str,
    ) -> Result<Option<ExtensionInformation>, ConnectionError> {
        self.extension_manager
            .lock()
            .unwrap()
            .extension_information(self, extension_name)
    }

    fn wait_for_reply_or_error(&self, sequence: SequenceNumber) -> Result<Vec<u8>, ReplyError> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush()?; // Ensure the request is sent
        loop {
            if let Some(reply) = inner.poll_for_reply_or_error(sequence) {
                if reply[0] == 0 {
                    let error = GenericError::new(reply)?;
                    return Err(error.into());
                } else {
                    return Ok(reply);
                }
            }
            inner = self.read_packet_and_enqueue(inner)?;
        }
    }

    fn wait_for_reply(&self, sequence: SequenceNumber) -> Result<Option<Vec<u8>>, ConnectionError> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush()?; // Ensure the request is sent
        loop {
            match inner.poll_for_reply(sequence) {
                PollReply::TryAgain => {}
                PollReply::NoReply => return Ok(None),
                PollReply::Reply(buffer) => return Ok(Some(buffer)),
            }
            inner = self.read_packet_and_enqueue(inner)?;
        }
    }

    fn check_for_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<GenericError>, ConnectionError> {
        let mut inner = self.inner.lock().unwrap();
        inner.prepare_check_for_reply_or_error(sequence)?;
        inner.flush()?; // Ensure the request is sent
        loop {
            match inner.poll_check_for_reply_or_error(sequence) {
                PollReply::TryAgain => {}
                PollReply::NoReply => return Ok(None),
                PollReply::Reply(buffer) => return Ok(GenericError::new(buffer).ok()),
            }
            inner = self.read_packet_and_enqueue(inner)?;
        }
    }

    fn wait_for_reply_with_fds(&self, _sequence: SequenceNumber) -> Result<BufWithFds, ReplyError> {
        unreachable!(
            "To wait for a reply containing FDs, a successful call to \
        send_request_with_reply_with_fds() is necessary. However, this function never succeeds."
        );
    }

    fn maximum_request_bytes(&self) -> usize {
        let mut max_bytes = self.maximum_request_bytes.lock().unwrap();
        self.prefetch_maximum_request_bytes_impl(&mut max_bytes);
        use MaxRequestBytes::*;
        match *max_bytes {
            Unknown => unreachable!("We just prefetched this"),
            Requested(seqno) => {
                let length = seqno
                    // If prefetching the request succeeded, get a cookie
                    .and_then(|seqno| {
                        Cookie::<_, EnableReply>::new(self, seqno)
                            // and then get the reply to the request
                            .reply()
                            .map(|reply| reply.maximum_request_length)
                            .ok()
                    })
                    // If anything failed (sending the request, getting the reply), use Setup
                    .unwrap_or_else(|| self.setup.maximum_request_length.into())
                    // Turn the u32 into usize, using the max value in case of overflow
                    .try_into()
                    .unwrap_or(usize::max_value());
                let length = length * 4;
                *max_bytes = Known(length);
                length
            }
            Known(length) => length,
        }
    }

    fn prefetch_maximum_request_bytes(&self) {
        let mut max_bytes = self.maximum_request_bytes.lock().unwrap();
        self.prefetch_maximum_request_bytes_impl(&mut max_bytes);
    }

    fn parse_error(&self, error: GenericError) -> Result<Error, ParseError> {
        let ext_mgr = self.extension_manager.lock().unwrap();
        Error::parse(error, &*ext_mgr)
    }

    fn parse_event(&self, event: GenericEvent) -> Result<Event, ParseError> {
        let ext_mgr = self.extension_manager.lock().unwrap();
        Event::parse(event, &*ext_mgr)
    }
}

impl<R: Read, W: Write> Connection for RustConnection<R, W> {
    fn wait_for_event_with_sequence(&self) -> Result<EventAndSeqNumber, ConnectionError> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if let Some(event) = inner.poll_for_event_with_sequence() {
                return Ok(event);
            }
            inner = self.read_packet_and_enqueue(inner)?;
        }
    }

    fn poll_for_event_with_sequence(&self) -> Result<Option<EventAndSeqNumber>, ConnectionError> {
        Ok(self.inner.lock().unwrap().poll_for_event_with_sequence())
    }

    fn flush(&self) -> Result<(), ConnectionError> {
        self.inner.lock().unwrap().flush()?;
        Ok(())
    }

    fn setup(&self) -> &Setup {
        &self.setup
    }

    fn generate_id(&self) -> Result<u32, ReplyOrIdError> {
        self.id_allocator.lock().unwrap().generate_id(self)
    }
}

// Read a single X11 packet from the connection.
//
// This function only supports errors, events, and replies. Namely, this cannot be used to receive
// the initial setup reply from the X11 server.
fn read_packet(read: &mut impl Read) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = vec![0; 32];
    read.read_exact(&mut buffer)?;

    use crate::xproto::GE_GENERIC_EVENT;
    const REPLY: u8 = 1;
    const SENT_GE_GENERIC_EVENT: u8 = GE_GENERIC_EVENT | 0x80;
    let extra_length = match buffer[0] {
        REPLY | GE_GENERIC_EVENT | SENT_GE_GENERIC_EVENT => {
            4 * u32::from_ne_bytes([buffer[4], buffer[5], buffer[6], buffer[7]])
        }
        _ => 0,
    } as usize;
    // Use `Vec::reserve_exact` because this will be the final
    // length of the vector.
    buffer.reserve_exact(extra_length);
    buffer.resize(32 + extra_length, 0);
    read.read_exact(&mut buffer[32..])?;

    Ok(buffer)
}
