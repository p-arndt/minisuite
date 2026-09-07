// Shared server state. Owns Server; re-exports the events Hub.

pub use crate::events::Hub;

pub struct Server {
    pub store: crate::store::Store,
    pub creds: crate::creds::Credentials,
    pub require_auth: bool, // gates BOTH SMTP AUTH-required and HTTP Basic (== !anonymous)
    pub hostname: String,
    pub version: &'static str, // env!("CARGO_PKG_VERSION")
    pub max_size: usize,       // ESMTP SIZE limit, bytes
    pub max_messages: Option<usize>,
    pub smtp_bind: String, // for /api/v1/info
    pub http_bind: String,
    pub hub: Hub,
    #[cfg(feature = "tls")]
    pub tls: Option<std::sync::Arc<rustls::ServerConfig>>,
}
