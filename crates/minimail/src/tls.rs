// TLS support (feature = "tls"): rustls + rustls-pemfile only. No cert generation.
// SPEC §10. This is the ONLY file that names rustls / rustls-pemfile. The whole
// file is compiled solely under `--features tls`.

use std::fs::File;
use std::io::{self, BufReader};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::PrivateKeyDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::stream::Stream;

/// Load a PEM cert chain + private key into a rustls `ServerConfig`.
/// Errors as `io::Error` (InvalidData) on a missing/empty/malformed PEM so main
/// can `exit(2)` uniformly.
pub fn load_server_config(cert_path: &Path, key_path: &Path) -> io::Result<Arc<ServerConfig>> {
    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no certificates found in cert file",
        ));
    }

    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut key_reader)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no private key found in key file",
            )
        })?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;

    Ok(Arc::new(config))
}

/// Complete a server handshake over an already-accepted `TcpStream` and return a
/// `Stream::Tls`. Used for STARTTLS upgrade and implicit SMTPS.
pub fn accept(tcp: TcpStream, cfg: &Arc<ServerConfig>) -> io::Result<Stream> {
    let mut conn = ServerConnection::new(Arc::clone(cfg)).map_err(io::Error::other)?;
    let mut tcp = tcp;
    // Drive the handshake to completion before wrapping; a failure here surfaces
    // as an io::Error so serve() can reply 454 and close.
    conn.complete_io(&mut tcp)?;
    Ok(Stream::Tls(Box::new(StreamOwned::new(conn, tcp))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_cert_errors() {
        let err = load_server_config(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
