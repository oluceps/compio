use std::{
    future::Future,
    io,
    ops::DerefMut,
    pin::Pin,
    task::{Context, Poll},
};

use compio_buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf, IoVectoredBufMut};
use compio_io::{compat::SyncStream, AsyncRead, AsyncWrite};
use compio_net::TcpStream;
use compio_tls::TlsStream;
#[cfg(feature = "client")]
use hyper::client::connect::{Connected, Connection};
use hyper::Uri;
use send_wrapper::SendWrapper;

use crate::TlsBackend;

#[allow(clippy::large_enum_variant)]
enum HttpStreamInner {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl HttpStreamInner {
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().expect("there should be host");
        let port = uri.port_u16();
        match scheme {
            "http" => {
                let stream = TcpStream::connect((host, port.unwrap_or(80))).await?;
                Ok(Self::Tcp(stream))
            }
            "https" => {
                let stream = TcpStream::connect((host, port.unwrap_or(443))).await?;
                let connector = tls.create_connector()?;
                Ok(Self::Tls(connector.connect(host, stream).await?))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported scheme",
            )),
        }
    }

    pub fn from_tcp(s: TcpStream) -> Self {
        Self::Tcp(s)
    }

    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::Tls(s)
    }
}

impl AsyncRead for HttpStreamInner {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        match self {
            Self::Tcp(s) => s.read(buf).await,
            Self::Tls(s) => s.read(buf).await,
        }
    }

    async fn read_vectored<V: IoVectoredBufMut>(&mut self, buf: V) -> BufResult<usize, V> {
        match self {
            Self::Tcp(s) => s.read_vectored(buf).await,
            Self::Tls(s) => s.read_vectored(buf).await,
        }
    }
}

impl AsyncWrite for HttpStreamInner {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write(buf).await,
            Self::Tls(s) => s.write(buf).await,
        }
    }

    async fn write_vectored<T: IoVectoredBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            Self::Tcp(s) => s.write_vectored(buf).await,
            Self::Tls(s) => s.write_vectored(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush().await,
            Self::Tls(s) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown().await,
            Self::Tls(s) => s.shutdown().await,
        }
    }
}

type PinBoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A HTTP stream wrapper, based on compio, and exposes [`tokio::io`]
/// interfaces.
pub struct HttpStream {
    inner: SendWrapper<SyncStream<HttpStreamInner>>,
    read_future: Option<PinBoxFuture<io::Result<usize>>>,
    write_future: Option<PinBoxFuture<io::Result<usize>>>,
    shutdown_future: Option<PinBoxFuture<io::Result<()>>>,
}

impl HttpStream {
    /// Create [`HttpStream`] with target uri and TLS backend.
    pub async fn connect(uri: Uri, tls: TlsBackend) -> io::Result<Self> {
        Ok(Self::from_inner(HttpStreamInner::connect(uri, tls).await?))
    }

    /// Create [`HttpStream`] with connected TCP stream.
    pub fn from_tcp(s: TcpStream) -> Self {
        Self::from_inner(HttpStreamInner::from_tcp(s))
    }

    /// Create [`HttpStream`] with connected TLS stream.
    pub fn from_tls(s: TlsStream<TcpStream>) -> Self {
        Self::from_inner(HttpStreamInner::from_tls(s))
    }

    fn from_inner(s: HttpStreamInner) -> Self {
        Self {
            inner: SendWrapper::new(SyncStream::new(s)),
            read_future: None,
            write_future: None,
            shutdown_future: None,
        }
    }
}

macro_rules! poll_future {
    ($f:expr, $cx:expr, $e:expr) => {{
        let mut future = match $f.take() {
            Some(f) => f,
            None => Box::pin(SendWrapper::new($e)),
        };
        let f = future.as_mut();
        match f.poll($cx) {
            Poll::Pending => {
                $f = Some(future);
                return Poll::Pending;
            }
            Poll::Ready(res) => res,
        }
    }};
}

macro_rules! poll_future_would_block {
    ($f:expr, $cx:expr, $e:expr, $io:expr) => {{
        if let Some(mut f) = $f.take() {
            if f.as_mut().poll($cx).is_pending() {
                $f = Some(f);
                return Poll::Pending;
            }
        }

        match $io {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                $f = Some(Box::pin(SendWrapper::new($e)));
                $cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }};
}

#[cfg(not(feature = "read_buf"))]
#[inline]
fn read_buf(reader: &mut impl io::Read, buf: &mut tokio::io::ReadBuf<'_>) -> io::Result<()> {
    let slice = buf.initialize_unfilled();
    let len = reader.read(slice)?;
    buf.advance(len);
    Ok(())
}

#[cfg(feature = "read_buf")]
#[inline]
fn read_buf(reader: &mut impl io::Read, buf: &mut tokio::io::ReadBuf<'_>) -> io::Result<()> {
    let slice = unsafe { buf.unfilled_mut() };
    let len = {
        let mut borrowed_buf = io::BorrowedBuf::from(slice);
        let mut cursor = borrowed_buf.unfilled();
        reader.read_buf(cursor.reborrow())?;
        cursor.written()
    };
    unsafe { buf.assume_init(len) };
    buf.advance(len);
    Ok(())
}

impl tokio::io::AsyncRead for HttpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let inner: &'static mut SyncStream<HttpStreamInner> =
            unsafe { &mut *(self.inner.deref_mut() as *mut _) };

        poll_future_would_block!(
            self.read_future,
            cx,
            inner.fill_read_buf(),
            read_buf(inner, buf)
        )
    }
}

impl tokio::io::AsyncWrite for HttpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let inner: &'static mut SyncStream<HttpStreamInner> =
            unsafe { &mut *(self.inner.deref_mut() as *mut _) };

        poll_future_would_block!(
            self.write_future,
            cx,
            inner.flush_write_buf(),
            io::Write::write(inner, buf)
        )
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner: &'static mut SyncStream<HttpStreamInner> =
            unsafe { &mut *(self.inner.deref_mut() as *mut _) };
        let res = poll_future!(self.write_future, cx, inner.flush_write_buf());
        Poll::Ready(res.map(|_| ()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner: &'static mut SyncStream<HttpStreamInner> =
            unsafe { &mut *(self.inner.deref_mut() as *mut _) };
        let res = poll_future!(self.shutdown_future, cx, inner.get_mut().shutdown());
        Poll::Ready(res)
    }
}

#[cfg(feature = "client")]
impl Connection for HttpStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}
