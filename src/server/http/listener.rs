//! What the HTTP server accepts from: plain TCP or rotating TLS.
//!
//! Not `axum::serve::Listener`, whose `accept` cannot fail. A listener whose
//! socket is broken would then retry forever while the node kept reporting
//! itself healthy, so here a fatal error ends the server and, with it, the
//! node: the orchestrator restarts a process that can listen again.

use std::{future::Future, io, net::SocketAddr};

use rushls_common::accept::{AcceptErrors, Next};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};

use crate::observe::{Events, NodeEvent, Protocol};

pub trait HttpListener: Send + 'static {
    type Io: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// The next connection. Temporary failures are retried inside, so an
    /// error means this listener can never accept again.
    fn accept(&mut self) -> impl Future<Output = io::Result<(Self::Io, SocketAddr)>> + Send;
}

/// A cleartext listener that reports accept failures as node events.
pub struct TcpHttpListener {
    tcp: TcpListener,
    errors: AcceptErrors,
    events: Events,
    protocol: Protocol,
}

impl TcpHttpListener {
    pub fn new(tcp: TcpListener, events: Events, protocol: Protocol) -> Self {
        Self {
            tcp,
            errors: AcceptErrors::default(),
            events,
            protocol,
        }
    }
}

/// Unreported, for tests and embedders that do not observe events.
impl From<TcpListener> for TcpHttpListener {
    fn from(tcp: TcpListener) -> Self {
        Self::new(tcp, Events::default(), Protocol::Http)
    }
}

impl HttpListener for TcpHttpListener {
    type Io = TcpStream;

    async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        loop {
            match self.tcp.accept().await {
                Ok(accepted) => {
                    self.errors.succeeded();
                    return Ok(accepted);
                }
                Err(error) => match self.errors.failed(&error) {
                    Next::Stop => return Err(error),
                    Next::Retry { pause, report } => {
                        if let Some(failures) = report {
                            self.events.emit(NodeEvent::ListenerAcceptFailed {
                                protocol: self.protocol,
                                reason: error.to_string(),
                                failures,
                            });
                        }
                        // Retrying at once would spin a core while
                        // descriptors stay exhausted.
                        tokio::time::sleep(pause).await;
                    }
                },
            }
        }
    }
}

impl<O: rushls_common::tls::TlsObserver> HttpListener for rushls_common::tls::TlsListener<O> {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;

    fn accept(&mut self) -> impl Future<Output = io::Result<(Self::Io, SocketAddr)>> + Send {
        self.accept_tls()
    }
}
