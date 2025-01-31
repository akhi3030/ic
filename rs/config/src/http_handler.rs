use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

const DEFAULT_IP_ADDR: &str = "0.0.0.0";

const DEFAULT_PORT: u16 = 8080u16;

/// The internal configuration -- any historical warts from the external
/// configuration are removed. Anything using this struct can trust that it
/// has been validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// IP address and port to listen on
    pub listen_addr: SocketAddr,

    /// The path to write the listening port to
    pub port_file_path: Option<PathBuf>,

    /// The endpoint can serve from at most 'max_tcp_connections'
    /// simultaneous TCP connections. If the limit is reached and a new
    /// TCP connection arrives, it is accepted and dropped immediately.
    pub max_tcp_connections: usize,

    /// If no bytes are read from a connection for the duration of
    /// 'connection_read_timeout_seconds', then the connection is dropped.
    /// There is no point is setting a timeout on the write bytes since
    /// they are conditioned on the received requests.
    pub connection_read_timeout_seconds: u64,

    /// Per request timeout in seconds before the server replies with `504 Gateway Timeout`.
    pub request_timeout_seconds: u64,

    /// The `SETTINGS_MAX_CONCURRENT_STREAMS` option for HTTP2 connections.
    pub http_max_concurrent_streams: u32,

    /// The maximum time we should wait for a peeking the first bytes on a TCP
    /// connection. Effectively, if we can't read the first bytes within the
    /// timeout the connection is broken.
    /// If you modify this constant please also adjust:
    /// - `ic_canister_client::agent::MAX_POLL_INTERVAL`,
    /// - `canister_test::canister::MAX_BACKOFF_INTERVAL`.
    /// See VER-1060 for details.
    pub max_tcp_peek_timeout_seconds: u64,

    /// Request with body size bigger than `max_request_size_bytes` will be rejected
    /// and [`413 Content Too Large`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/413) will be returned to the user.
    pub max_request_size_bytes: u64,

    /// Delegation certificate requests with body size bigger than `max_delegation_certificate_size_bytes`
    /// will be rejected. For valid IC delegation certificates this is never the case since the size is always constant.
    pub max_delegation_certificate_size_bytes: u64,

    /// If the request body is not received/parsed within
    /// `max_request_receive_seconds`, then the request will be rejected and
    /// [`408 Request Timeout`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/408) will be returned to the user.
    pub max_request_receive_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(
                DEFAULT_IP_ADDR.parse().expect("can't fail"),
                DEFAULT_PORT,
            ),
            port_file_path: None,
            max_tcp_connections: 20_000,
            connection_read_timeout_seconds: 1_200, // 20 min
            request_timeout_seconds: 300,           // 5 min
            http_max_concurrent_streams: 256,
            max_tcp_peek_timeout_seconds: 11,
            max_request_size_bytes: 5 * 1024 * 1024, // 5MB
            max_delegation_certificate_size_bytes: 1024 * 1024, // 1MB
            max_request_receive_seconds: 300,        // 5 min
        }
    }
}
