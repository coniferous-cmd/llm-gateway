//! Upstream forwarding over hyper.
//!
//! The transport chooses its path by destination authority: a configured peer
//! is dialed through its Noise dialer, HTTPS is wrapped by rustls with system
//! roots and ALPN, and everything else uses plain TCP. Requests are sent once;
//! there is no retry layer.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, HeaderName, Method, Request, Response, Uri, Version};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tower_service::Service;

use super::prepare::ReqBody;
use super::rewrite::rewrite_path;
use crate::config::Target;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A transport that has already been encrypted by a peer dialer.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncReadWrite for T {}

/// A future that opens an encrypted peer connection.
pub type PeerDialFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn AsyncReadWrite>>> + Send>>;

/// Opens an encrypted connection to a peer gateway.
pub trait PeerDialer: Send + Sync {
    fn dial(&self, address: String) -> PeerDialFuture;
}

/// A connection that is either plain TCP or a peer's encrypted stream.
pub enum InnerStream {
    Plain(TcpStream),
    Noise(Box<dyn AsyncReadWrite>),
}

impl AsyncRead for InnerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Noise(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for InnerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Noise(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Noise(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Noise(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl Connection for InnerStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

#[derive(Clone)]
pub struct GatewayConnector {
    peers: Arc<HashMap<String, Arc<dyn PeerDialer>>>,
}

impl GatewayConnector {
    pub fn new(peers: HashMap<String, Arc<dyn PeerDialer>>) -> Self {
        Self {
            peers: Arc::new(peers),
        }
    }
}

impl Service<Uri> for GatewayConnector {
    type Response = TokioIo<InnerStream>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let peers = self.peers.clone();
        Box::pin(async move {
            let scheme = uri.scheme_str().unwrap_or("http");
            let host = uri.host().ok_or("upstream URI has no host")?;
            let port = uri
                .port_u16()
                .unwrap_or(if scheme == "https" { 443 } else { 80 });
            let address = format!("{host}:{port}");
            if let Some(dialer) = peers.get(&address) {
                let stream = dialer.dial(address).await?;
                return Ok(TokioIo::new(InnerStream::Noise(stream)));
            }
            let stream = connect_tcp(host, port).await?;
            Ok(TokioIo::new(InnerStream::Plain(stream)))
        })
    }
}

async fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let connect = async {
        let mut last_error = None;
        let addresses = tokio::net::lookup_host((host, port)).await?;
        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no addresses resolved")))
    };
    match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "upstream connect timed out",
        )),
    }
}

/// Forwarding handler shared by every listener.
pub struct Forwarder {
    client: Client<HttpsConnector<GatewayConnector>, ReqBody>,
}

impl Forwarder {
    pub fn new(peers: HashMap<String, Arc<dyn PeerDialer>>) -> io::Result<Self> {
        // Prefer the system trust store, matching Go. Minimal containers and
        // some CI sandboxes have none, so fall back to the bundled roots.
        let builder = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap_or_else(|_| hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots());
        let connector = builder
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(GatewayConnector::new(peers));
        let client: Client<HttpsConnector<GatewayConnector>, ReqBody> =
            Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(16)
                .pool_idle_timeout(Duration::from_secs(90))
                .retry_canceled_requests(false)
                .build(connector);
        Ok(Self { client })
    }

    /// Send one request upstream. The caller has already selected the target
    /// and prepared the body. Errors are terminal: no retry or failover.
    pub async fn send(
        &self,
        method: Method,
        uri: &Uri,
        headers: HeaderMap,
        body: ReqBody,
        target: &Target,
        replace_content_length: bool,
    ) -> Result<Response<Incoming>, BoxError> {
        let original = uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);
        let path_and_query =
            rewrite_path(original, &target.rewrite).unwrap_or_else(|| original.to_string());
        let (scheme, authority) = split_origin(&target.base_url)?;
        let upstream_uri = Uri::builder()
            .scheme(scheme)
            .authority(authority)
            .path_and_query(path_and_query)
            .build()?;

        let mut headers = headers;
        let keep_te_trailers = header_contains_token(&headers, TE, "trailers");
        remove_hop_by_hop(&mut headers);
        if keep_te_trailers {
            headers.insert(TE, http::HeaderValue::from_static("trailers"));
        }
        headers.remove(HOST);
        if target.api_key.is_empty() {
            headers.remove(AUTHORIZATION);
        } else {
            let mut value = http::HeaderValue::from_str(&format!("Bearer {}", target.api_key))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        if replace_content_length {
            headers.remove(CONTENT_LENGTH);
        }

        let mut request = Request::new(body);
        *request.method_mut() = method;
        *request.uri_mut() = upstream_uri;
        *request.version_mut() = Version::HTTP_11;
        *request.headers_mut() = headers;

        match tokio::time::timeout(Duration::from_secs(30), self.client.request(request)).await {
            Ok(Ok(response)) => {
                let (mut parts, body) = response.into_parts();
                remove_hop_by_hop(&mut parts.headers);
                Ok(Response::from_parts(parts, body))
            }
            Ok(Err(error)) => Err(Box::new(error) as BoxError),
            Err(_) => Err(Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "upstream response headers timed out",
            )) as BoxError),
        }
    }
}

fn split_origin(base_url: &str) -> Result<(&str, &str), BoxError> {
    let (scheme, authority) = base_url
        .split_once("://")
        .ok_or("target base_url is not an origin")?;
    if authority.is_empty() {
        return Err("target base_url has no authority".into());
    }
    Ok((scheme, authority))
}

fn header_contains_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
}

fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in [
        CONNECTION,
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("keep-alive"),
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(name);
    }
    for name in named {
        headers.remove(name);
    }
}

/// Box an upstream response body for the server pipeline.
pub fn box_response(body: Incoming) -> super::prepare::ReqBody {
    body.map_err(|error| Box::new(error) as BoxError)
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    #[test]
    fn hop_by_hop_headers_are_removed_including_connection_named() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Hop, keep-alive"));
        headers.insert(
            HeaderName::from_static("x-hop"),
            HeaderValue::from_static("secret"),
        );
        headers.insert(
            HeaderName::from_static("keep-alive"),
            HeaderValue::from_static("timeout=5"),
        );
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("keep"),
        );
        remove_hop_by_hop(&mut headers);
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("x-hop"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key(TRANSFER_ENCODING));
        assert_eq!(headers.get("x-custom").unwrap(), "keep");
    }

    #[test]
    fn te_trailers_is_preserved_like_go() {
        let mut headers = HeaderMap::new();
        headers.insert(TE, HeaderValue::from_static("trailers, deflate"));
        assert!(header_contains_token(&headers, TE, "trailers"));
        assert!(!header_contains_token(&headers, TE, "gzip"));
    }

    #[test]
    fn split_origin_keeps_configured_authority() {
        assert_eq!(
            split_origin("https://api.example.com").unwrap(),
            ("https", "api.example.com")
        );
        assert_eq!(
            split_origin("http://127.0.0.1:11434").unwrap(),
            ("http", "127.0.0.1:11434")
        );
        assert!(split_origin("not-an-origin").is_err());
    }
}
