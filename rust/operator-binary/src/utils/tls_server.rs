use std::{error::Error as StdError, sync::Arc, task::Poll};

use futures::stream::{FuturesUnordered, Stream};
use hyper::server::accept::Accept;
use pin_project::pin_project;
use snafu::Snafu;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{rustls::ServerConfig, server::TlsStream};
use tracing::warn;

#[pin_project(project = TlsAcceptProj)]
pub struct TlsAccept<A: Accept> {
    #[pin]
    inner: A,
    inner_terminated: bool,
    rustls_acceptor: tokio_rustls::TlsAcceptor,
    #[pin]
    handshaking_conns: FuturesUnordered<tokio_rustls::Accept<A::Conn>>,
}
impl<A: Accept> TlsAccept<A> {
    pub fn new(inner: A, server_config: Arc<ServerConfig>) -> Self {
        Self {
            inner,
            inner_terminated: false,
            rustls_acceptor: server_config.into(),
            handshaking_conns: FuturesUnordered::new(),
        }
    }
}
impl<A: Accept> Accept for TlsAccept<A>
where
    A::Conn: AsyncRead + AsyncWrite + Unpin,
{
    type Conn = TlsStream<A::Conn>;
    type Error = A::Error;

    fn poll_accept(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        let mut this = self.project();
        if !*this.inner_terminated {
            while let Poll::Ready(plaintext_conn) = this.inner.as_mut().poll_accept(cx) {
                match plaintext_conn {
                    Some(Ok(plaintext_conn)) => {
                        this.handshaking_conns
                            .push(this.rustls_acceptor.accept(plaintext_conn));
                    }
                    Some(Err(err)) => return Poll::Ready(Some(Err(err))),
                    None => {
                        *this.inner_terminated = true;
                        break;
                    }
                }
            }
        }

        loop {
            break if this.handshaking_conns.is_empty() && !*this.inner_terminated {
                Poll::Pending
            } else {
                match this.handshaking_conns.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(tls_conn))) => Poll::Ready(Some(Ok(tls_conn))),
                    Poll::Ready(Some(Err(handshake_err))) => {
                        warn!(
                            error = &handshake_err as &dyn StdError,
                            "tls handshake failed"
                        );
                        continue;
                    }
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                }
            };
        }
    }
}
#[derive(Debug, Snafu)]
pub enum TlsAcceptError<E: StdError + 'static> {
    PlaintextAccept { source: E },
}
