use futures_lite::{AsyncRead, AsyncWrite, Stream};
use log::*;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::tcp::{TcpStream, TcpTransport};
#[cfg(feature = "transport_utp")]
use super::utp::{UtpStream, UtpTransport};
use super::{Connection, Transport};

#[derive(Debug)]
pub struct CombinedTransport {
    tcp: TcpTransport,
    #[cfg(feature = "transport_utp")]
    utp: UtpTransport,
    local_addr: SocketAddr,
    connected: HashSet<SocketAddr>,
}

impl CombinedTransport {
    pub async fn bind<A>(local_addr: A) -> io::Result<Self>
    where
        A: ToSocketAddrs + Send,
    {
        let tcp = TcpTransport::bind(local_addr).await?;
        let local_addr = tcp.local_addr();
        #[cfg(feature = "transport_utp")]
        let utp = UtpTransport::bind(local_addr).await?;
        Ok(Self {
            tcp,
            #[cfg(feature = "transport_utp")]
            utp,
            local_addr,
            connected: HashSet::new(), // pending_connects: HashSet::new(),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn on_poll_connection<T, F>(
        &mut self,
        poll: Poll<Option<io::Result<Connection<T>>>>,
        map: F,
    ) -> Option<io::Result<Connection<CombinedStream>>>
    where
        T: std::fmt::Debug + AsyncRead + AsyncWrite + Unpin,
        F: Fn(T) -> CombinedStream,
    {
        match poll {
            Poll::Pending => None,
            Poll::Ready(None) => None,
            Poll::Ready(Some(Err(err))) => Some(Err(err)),
            Poll::Ready(Some(Ok(conn))) => self.on_connection(conn, map),
        }
    }

    fn on_connection<T, F>(
        &mut self,
        conn: Connection<T>,
        map: F,
    ) -> Option<io::Result<Connection<CombinedStream>>>
    where
        T: std::fmt::Debug + AsyncRead + AsyncWrite + Unpin,
        F: Fn(T) -> CombinedStream,
    {
        // let (stream, peer_addr, is_initiator, protocol) = conn.into_parts();
        // let stream = map(stream);
        // let conn = Connection::new(stream, peer_addr, is_initiator, protocol);
        // Some(Ok(conn))

        // TODO:
        // The code above leads to establishing BOTH a utp and a tcp connection.
        // This we do not want.
        // The code below would cancel either connection if connected already over the other
        // protocol. However this does not work reliably either. The connectoin disambituation
        // needs some more thought.

        // let addr_without_port = peer_addr.set_port(0);
        let (stream, peer_addr, is_initiator, protocol) = conn.into_parts();
        let take_connection = if !is_initiator {
            true
        } else {
            if !self.connected.contains(&peer_addr) {
                self.connected.insert(peer_addr.clone());
                true
            } else {
                false
            }
        };
        if take_connection {
            debug!(
                "new connection to {} via {} (init {})",
                peer_addr, protocol, is_initiator
            );
            let stream = map(stream);
            let conn = Connection::new(stream, peer_addr, is_initiator, protocol);
            Some(Ok(conn))
        } else {
            debug!(
                "skip double connection to {} via {} (init {})",
                peer_addr, protocol, is_initiator
            );
            None
        }
    }
}

impl Transport for CombinedTransport {
    type Connection = CombinedStream;
    fn connect(&mut self, peer_addr: SocketAddr) {
        self.tcp.connect(peer_addr);
        #[cfg(feature = "transport_utp")]
        self.utp.connect(peer_addr);
    }
}

impl Stream for CombinedTransport {
    type Item = io::Result<Connection<<Self as Transport>::Connection>>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let tcp_next = Pin::new(&mut self.tcp).poll_next(cx);
        if let Some(res) = self.on_poll_connection(tcp_next, CombinedStream::Tcp) {
            return Poll::Ready(Some(res));
        }

        #[cfg(feature = "transport_utp")]
        {
            let utp_next = Pin::new(&mut self.utp).poll_next(cx);
            if let Some(res) = self.on_poll_connection(utp_next, CombinedStream::Utp) {
                return Poll::Ready(Some(res));
            }
        }

        Poll::Pending
    }
}

pub enum CombinedStream {
    Tcp(TcpStream),
    #[cfg(feature = "transport_utp")]
    Utp(UtpStream),
}

impl Debug for CombinedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Tcp(_) => "Tcp",
            #[cfg(feature = "transport_utp")]
            Self::Utp(_) => "Utp",
        };
        write!(f, "CombinedStream::{}", name)
    }
}

impl CombinedStream {
    pub fn peer_addr(&self) -> SocketAddr {
        match self {
            Self::Tcp(stream) => stream.peer_addr().unwrap(),
            #[cfg(feature = "transport_utp")]
            Self::Utp(stream) => stream.peer_addr(),
        }
    }

    pub fn protocol(&self) -> String {
        match self {
            CombinedStream::Tcp(_) => "tcp".into(),
            #[cfg(feature = "transport_utp")]
            CombinedStream::Utp(_) => "utp".into(),
        }
    }
}

impl AsyncRead for CombinedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            CombinedStream::Tcp(ref mut stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(feature = "transport_utp")]
            CombinedStream::Utp(ref mut stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for CombinedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            CombinedStream::Tcp(ref mut stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(feature = "transport_utp")]
            CombinedStream::Utp(ref mut stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            CombinedStream::Tcp(ref mut stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(feature = "transport_utp")]
            CombinedStream::Utp(ref mut stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            CombinedStream::Tcp(ref mut stream) => Pin::new(stream).poll_close(cx),
            #[cfg(feature = "transport_utp")]
            CombinedStream::Utp(ref mut stream) => Pin::new(stream).poll_close(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    // use std::net::{IpAddr, Ipv4Addr};
    // use super::*;
    // use async_std::stream::StreamExt;
    // use async_std::task;

    // #[async_std::test]
    // async fn test_combined() -> io::Result<()> {
    //     env_logger::init();
    //     let mut ta = CombinedTransport::bind("localhost:0").await?;
    //     let mut tb = CombinedTransport::bind("localhost:0").await?;
    //     let addr_a = ta.local_addr();
    //     let addr_b = tb.local_addr();
    //     eprintln!("ta {:?}", ta);
    //     eprintln!("tb {:?}", tb);

    //     ta.connect(addr_b);
    //     tb.connect(addr_a);

    //     let task1 = task::spawn(async move {
    //         while let Some(stream) = ta.next().await {
    //             eprintln!("ta in: {:?}", stream);
    //         }
    //     });

    //     let task2 = task::spawn(async move {
    //         while let Some(stream) = tb.next().await {
    //             eprintln!("tb in: {:?}", stream);
    //         }
    //     });

    //     task1.await;
    //     task2.await;
    //     Ok(())
    // }
}
