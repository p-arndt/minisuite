// Socket transport seam (SPEC §5/§10): plain TCP, or (feature=tls) a rustls stream.
// Pure std by default — no rustls symbol is referenced unless `tls` is enabled.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub enum Stream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Stream {
    #[allow(dead_code)] // frozen SPEC §5 Stream API; peer logged from accept() addr instead
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Stream::Plain(s) => s.peer_addr(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.sock.peer_addr(),
        }
    }

    pub fn set_nodelay(&self, on: bool) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.set_nodelay(on),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.sock.set_nodelay(on),
        }
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.set_read_timeout(dur),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.sock.set_read_timeout(dur),
        }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.set_write_timeout(dur),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.sock.set_write_timeout(dur),
        }
    }

    /// Plain: clone into a second Stream::Plain. Tls: Err(Unsupported).
    pub fn try_clone(&self) -> io::Result<Stream> {
        match self {
            Stream::Plain(s) => Ok(Stream::Plain(s.try_clone()?)),
            #[cfg(feature = "tls")]
            Stream::Tls(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot try_clone a TLS stream",
            )),
        }
    }

    pub fn is_tls(&self) -> bool {
        match self {
            Stream::Plain(_) => false,
            #[cfg(feature = "tls")]
            Stream::Tls(_) => true,
        }
    }

    /// Recover the concrete TcpStream (for the STARTTLS handshake). Only valid on Plain.
    #[allow(dead_code)] // frozen SPEC §5 Stream API; only the tls STARTTLS path calls this
    pub fn into_tcp(self) -> io::Result<TcpStream> {
        match self {
            Stream::Plain(s) => Ok(s),
            #[cfg(feature = "tls")]
            Stream::Tls(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot recover a plain TcpStream from a TLS stream",
            )),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    // Establish a connected loopback pair; return (server_side, client_side).
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || listener.accept().unwrap().0);
        let client = TcpStream::connect(addr).unwrap();
        let server = handle.join().unwrap();
        (server, client)
    }

    #[test]
    fn plain_is_not_tls() {
        let (server, _client) = pair();
        let s = Stream::Plain(server);
        assert!(!s.is_tls());
    }

    #[test]
    fn peer_addr_reports_remote() {
        let (server, client) = pair();
        let want = client.local_addr().unwrap();
        let s = Stream::Plain(server);
        assert_eq!(s.peer_addr().unwrap(), want);
    }

    #[test]
    fn socket_options_delegate() {
        let (server, _client) = pair();
        let s = Stream::Plain(server);
        s.set_nodelay(true).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
        s.set_read_timeout(None).unwrap();
    }

    #[test]
    fn try_clone_yields_plain() {
        let (server, _client) = pair();
        let s = Stream::Plain(server);
        let c = s.try_clone().unwrap();
        assert!(!c.is_tls());
    }

    #[test]
    fn into_tcp_recovers_socket() {
        let (server, client) = pair();
        let want = client.local_addr().unwrap();
        let s = Stream::Plain(server);
        let tcp = s.into_tcp().unwrap();
        assert_eq!(tcp.peer_addr().unwrap(), want);
    }

    #[test]
    fn read_write_round_trip() {
        let (server, mut client) = pair();
        let mut s = Stream::Plain(server);
        s.write_all(b"ping").unwrap();
        s.flush().unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");

        client.write_all(b"pong").unwrap();
        client.flush().unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"pong");
    }
}
