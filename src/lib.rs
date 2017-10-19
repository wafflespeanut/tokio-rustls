//! Asynchronous TLS/SSL streams for Tokio using [Rustls](https://github.com/ctz/rustls).


#[cfg_attr(feature = "tokio-proto", macro_use)] extern crate futures;
#[macro_use] extern crate tokio_io;
extern crate rustls;
extern crate webpki;

pub mod proto;

use std::io;
use std::sync::Arc;
use futures::{ Future, Poll, Async };
use tokio_io::{ AsyncRead, AsyncWrite };
use rustls::{
    Session, ClientSession, ServerSession,
    ClientConfig, ServerConfig
};
use webpki::DNSNameRef;


/// Extension trait for the `Arc<ClientConfig>` type in the `rustls` crate.
pub trait ClientConfigExt {
    fn connect_async<S>(&self, domain: DNSNameRef, stream: S)
        -> ConnectAsync<S>
        where S: AsyncRead + AsyncWrite;
}

/// Extension trait for the `Arc<ServerConfig>` type in the `rustls` crate.
pub trait ServerConfigExt {
    fn accept_async<S>(&self, stream: S)
        -> AcceptAsync<S>
        where S: AsyncRead + AsyncWrite;
}


/// Future returned from `ClientConfigExt::connect_async` which will resolve
/// once the connection handshake has finished.
pub struct ConnectAsync<S>(MidHandshake<S, ClientSession>);

/// Future returned from `ServerConfigExt::accept_async` which will resolve
/// once the accept handshake has finished.
pub struct AcceptAsync<S>(MidHandshake<S, ServerSession>);


impl ClientConfigExt for Arc<ClientConfig> {
    fn connect_async<S>(&self, domain: DNSNameRef, stream: S)
        -> ConnectAsync<S>
        where S: AsyncRead + AsyncWrite
    {
        connect_async_with_session(stream, ClientSession::new(self, domain))
    }
}

#[inline]
pub fn connect_async_with_session<S>(stream: S, session: ClientSession)
    -> ConnectAsync<S>
    where S: AsyncRead + AsyncWrite
{
    ConnectAsync(MidHandshake {
        inner: Some(TlsStream::new(stream, session))
    })
}

impl ServerConfigExt for Arc<ServerConfig> {
    fn accept_async<S>(&self, stream: S)
        -> AcceptAsync<S>
        where S: AsyncRead + AsyncWrite
    {
        accept_async_with_session(stream, ServerSession::new(self))
    }
}

#[inline]
pub fn accept_async_with_session<S>(stream: S, session: ServerSession)
    -> AcceptAsync<S>
    where S: AsyncRead + AsyncWrite
{
    AcceptAsync(MidHandshake {
        inner: Some(TlsStream::new(stream, session))
    })
}

impl<S: AsyncRead + AsyncWrite> Future for ConnectAsync<S> {
    type Item = TlsStream<S, ClientSession>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

impl<S: AsyncRead + AsyncWrite> Future for AcceptAsync<S> {
    type Item = TlsStream<S, ServerSession>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}


struct MidHandshake<S, C> {
    inner: Option<TlsStream<S, C>>
}

impl<S, C> Future for MidHandshake<S, C>
    where S: AsyncRead + AsyncWrite, C: Session
{
    type Item = TlsStream<S, C>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let stream = self.inner.as_mut().unwrap();
            if !stream.session.is_handshaking() { break };

            match stream.do_io() {
                Ok(()) => match (stream.eof, stream.session.is_handshaking()) {
                    (true, true) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                    (false, true) => continue,
                    (..) => break
                },
                Err(e) => match (e.kind(), stream.session.is_handshaking()) {
                    (io::ErrorKind::WouldBlock, true) => return Ok(Async::NotReady),
                    (io::ErrorKind::WouldBlock, false) => break,
                    (..) => return Err(e)
                }
            }
        }

        Ok(Async::Ready(self.inner.take().unwrap()))
    }
}


/// A wrapper around an underlying raw stream which implements the TLS or SSL
/// protocol.
#[derive(Debug)]
pub struct TlsStream<S, C> {
    is_shutdown: bool,
    eof: bool,
    io: S,
    session: C
}

impl<S, C> TlsStream<S, C> {
    pub fn get_ref(&self) -> (&S, &C) {
        (&self.io, &self.session)
    }

    pub fn get_mut(&mut self) -> (&mut S, &mut C) {
        (&mut self.io, &mut self.session)
    }
}

impl<S, C> TlsStream<S, C>
    where S: AsyncRead + AsyncWrite, C: Session
{
    #[inline]
    pub fn new(io: S, session: C) -> TlsStream<S, C> {
        TlsStream {
            is_shutdown: false,
            eof: false,
            io: io,
            session: session
        }
    }

    pub fn do_io(&mut self) -> io::Result<()> {
        loop {
            let read_would_block = if !self.eof && self.session.wants_read() {
                match self.session.read_tls(&mut self.io) {
                    Ok(0) => {
                        self.eof = true;
                        continue
                    },
                    Ok(_) => {
                        if let Err(err) = self.session.process_new_packets() {
                            // flush queued messages before returning an Err in
                            // order to send alerts instead of abruptly closing
                            // the socket
                            if self.session.wants_write() {
                                // ignore result to avoid masking original error
                                let _ = self.session.write_tls(&mut self.io);
                            }
                            return Err(io::Error::new(io::ErrorKind::Other, err));
                        }
                        continue
                    },
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => true,
                    Err(e) => return Err(e)
                }
            } else {
                false
            };

            let write_would_block = if self.session.wants_write() {
                match self.session.write_tls(&mut self.io) {
                    Ok(_) => continue,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => true,
                    Err(e) => return Err(e)
                }
            } else {
                false
            };

            if read_would_block || write_would_block {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            } else {
                return Ok(());
            }
        }
    }
}

impl<S, C> io::Read for TlsStream<S, C>
    where S: AsyncRead + AsyncWrite, C: Session
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.session.read(buf) {
                Ok(0) if !self.eof => self.do_io()?,
                Ok(n) => return Ok(n),
                Err(e) => if e.kind() == io::ErrorKind::ConnectionAborted {
                    self.do_io()?;
                    return if self.eof { Ok(0) } else { Err(e) }
                } else {
                    return Err(e)
                }
            }
        }
    }
}

impl<S, C> io::Write for TlsStream<S, C>
    where S: AsyncRead + AsyncWrite, C: Session
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        loop {
            let output = self.session.write(buf)?;

            while self.session.wants_write() {
                match self.session.write_tls(&mut self.io) {
                    Ok(_) => (),
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => if output == 0 {
                        // Both rustls buffer and IO buffer are blocking.
                        return Err(io::Error::from(io::ErrorKind::WouldBlock));
                    } else {
                        break;
                    },
                    Err(e) => return Err(e)
                }
            }

            if output > 0 {
                // Already wrote something out.
                return Ok(output);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.session.flush()?;
        while self.session.wants_write() {
            self.session.write_tls(&mut self.io)?;
        }
        self.io.flush()
    }
}

impl<S, C> AsyncRead for TlsStream<S, C>
    where
        S: AsyncRead + AsyncWrite,
        C: Session
{}

impl<S, C> AsyncWrite for TlsStream<S, C>
    where
        S: AsyncRead + AsyncWrite,
        C: Session
{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        if !self.is_shutdown {
            self.session.send_close_notify();
            self.is_shutdown = true;
        }
        while self.session.wants_write() {
            try_nb!(self.session.write_tls(&mut self.io));
        }
        try_nb!(self.io.flush());
        self.io.shutdown()
    }
}
