use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use bytes::Bytes;
use futures::TryStreamExt as _;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Empty, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use reqwest::Url;
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tokio::task::{JoinHandle, JoinSet};

use crate::route::Resolver;
use crate::socket;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;

/// Reserves an adapter address without constructing clients or starting runtime tasks.
pub(crate) struct Listener {
    pub url: Url,
    tcp: std::net::TcpListener,
}

impl Listener {
    pub fn bind() -> Result<Self> {
        let tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        tcp.set_nonblocking(true)?;
        let url = Url::parse(&format!("http://{}", tcp.local_addr()?))?;
        Ok(Self { url, tcp })
    }
}

/// An HTTP proxy adapter for transports that cannot use the selected upstream protocol directly.
pub(crate) struct Gateway {
    pub url: Url,
    task: JoinHandle<()>,
}

impl Gateway {
    pub fn start(provider: String, resolver: Arc<Resolver>, handle: &Handle) -> Result<Self> {
        Self::start_from(provider, resolver, handle, Listener::bind()?)
    }

    /// Starts a previously reserved adapter after its startup policy and runtime are ready.
    pub fn start_from(
        provider: String,
        resolver: Arc<Resolver>,
        handle: &Handle,
        listener: Listener,
    ) -> Result<Self> {
        let Listener { url, tcp } = listener;
        let _entered = handle.enter();
        let listener = TcpListener::from_std(tcp)?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .proxy(resolver.http_proxy(&provider))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()?;
        let task = handle.spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((connection, _)) = accepted else { break };
                        let http = http.clone();
                        let resolver = resolver.clone();
                        let provider = provider.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request| {
                                forward(request, http.clone(), resolver.clone(), provider.clone())
                            });
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(connection), service)
                                .with_upgrades().await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { url, task })
    }
}

async fn forward(
    mut request: Request<Incoming>,
    http: reqwest::Client,
    resolver: Arc<Resolver>,
    provider: String,
) -> Result<Response<Body>, Infallible> {
    let response = async {
        if request.method() == Method::CONNECT {
            let authority = request
                .uri()
                .authority()
                .context("CONNECT has no authority")?;
            ensure!(authority.port_u16().is_some(), "CONNECT has no port");
            let destination = Url::parse(&format!("https://{authority}/"))?;
            let mut remote = tokio::time::timeout(
                CONNECT_TIMEOUT,
                socket::connect(&resolver, &provider, &destination),
            )
            .await??;
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                if let Ok(upgraded) = upgrade.await {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(upgraded), &mut remote)
                        .await;
                }
            });
            return Ok(Response::new(empty()));
        }
        let (mut parts, body) = request.into_parts();
        let destination = Url::parse(&parts.uri.to_string())?;
        ensure!(
            matches!(destination.scheme(), "http" | "https"),
            "unsupported destination scheme"
        );
        strip_hop_headers(&mut parts.headers);
        let response = http
            .request(parts.method, destination)
            .headers(parts.headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;
        let status = response.status();
        let mut headers = response.headers().clone();
        strip_hop_headers(&mut headers);
        let body = response
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(|error| Box::new(error) as Error);
        let mut outgoing = Response::new(StreamBody::new(body).boxed_unsync());
        *outgoing.status_mut() = status;
        *outgoing.headers_mut() = headers;
        Ok::<_, anyhow::Error>(outgoing)
    }
    .await;
    Ok(response.unwrap_or_else(|_| {
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        response
    }))
}

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| http::header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hyper_util::client::proxy::matcher::Matcher;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;

    use super::*;
    use crate::{Policy, Settings};

    async fn header(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        while !received.ends_with(b"\r\n\r\n") {
            received.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(received).unwrap()
    }

    fn routes(proxy: String) -> Arc<Resolver> {
        Arc::new(Resolver::new(
            &Settings {
                global: Policy::Custom {
                    url: proxy,
                    bypass: String::new(),
                },
                ..Settings::default()
            },
            Matcher::builder().build(),
        ))
    }

    #[tokio::test]
    async fn direct_and_proxied_clients_remain_independent() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tokio::time::timeout(Duration::from_secs(3), async {
            let direct = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = format!("http://{}/", direct.local_addr().unwrap());
            let settings = Settings {
                global: Policy::Custom {
                    url: format!("http://{}", upstream.local_addr().unwrap()),
                    bypass: String::new(),
                },
                providers: std::collections::BTreeMap::from([(
                    "subsonic".to_owned(),
                    Policy::Direct,
                )]),
            };
            let resolver = Resolver::new(&settings, Matcher::builder().build());
            let direct_server = tokio::spawn(async move {
                let (mut connection, _) = direct.accept().await.unwrap();
                assert!(
                    header(&mut connection)
                        .await
                        .starts_with("GET / HTTP/1.1\r\n")
                );
                connection
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ndirect",
                    )
                    .await
                    .unwrap();
            });
            let proxy_server = tokio::spawn(async move {
                let (mut connection, _) = upstream.accept().await.unwrap();
                assert!(header(&mut connection).await.starts_with("GET http://"));
                connection
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                    )
                    .await
                    .unwrap();
            });
            for (provider, expected) in [("youtube", "proxied"), ("subsonic", "direct")] {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .proxy(resolver.http_proxy(provider))
                    .build()
                    .unwrap();
                assert_eq!(
                    client
                        .get(&target)
                        .send()
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap(),
                    expected
                );
            }
            direct_server.await.unwrap();
            proxy_server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn refused_tunnels_never_report_success_or_connect_directly() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tokio::time::timeout(Duration::from_secs(3), async {
            let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let gateway = Gateway::start(
                "youtube".to_owned(),
                routes(format!("http://{}", upstream.local_addr().unwrap())),
                &Handle::current(),
            )
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut connection, _) = upstream.accept().await.unwrap();
                header(&mut connection).await;
                connection
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            });
            let mut connection =
                TcpStream::connect((gateway.url.host_str().unwrap(), gateway.url.port().unwrap()))
                    .await
                    .unwrap();
            let authority = destination.local_addr().unwrap();
            connection
                .write_all(
                    format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            assert!(header(&mut connection).await.starts_with("HTTP/1.1 502"));
            server.await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), destination.accept())
                    .await
                    .is_err()
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn forwards_http_through_the_scoped_upstream() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tokio::time::timeout(Duration::from_secs(3), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let gateway = Gateway::start(
                "youtube".to_owned(),
                routes(format!("http://{}", upstream.local_addr().unwrap())),
                &Handle::current(),
            )
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut connection, _) = upstream.accept().await.unwrap();
                let request = header(&mut connection).await;
                assert!(request.starts_with("GET http://destination.invalid/ HTTP/1.1\r\n"));
                assert!(!request.to_lowercase().contains("proxy-authorization"));
                connection
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                    )
                    .await
                    .unwrap();
            });
            let response = reqwest::Client::builder()
                .no_proxy()
                .proxy(reqwest::Proxy::all(gateway.url.as_str()).unwrap())
                .build()
                .unwrap()
                .get("http://destination.invalid/")
                .header("Proxy-Authorization", "Basic local-only")
                .send()
                .await
                .unwrap();
            assert_eq!(response.text().await.unwrap(), "proxied");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connect_waits_for_the_remote_tunnel_and_preserves_its_bytes() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let gateway = Gateway::start("spotify".to_owned(), routes(format!("http://{}", upstream.local_addr().unwrap())), &Handle::current()).unwrap();
            let server = tokio::spawn(async move {
                let (mut connection, _) = upstream.accept().await.unwrap();
                let request = header(&mut connection).await;
                assert!(request.starts_with("CONNECT destination.invalid:443 HTTP/1.1\r\n"));
                connection.write_all(b"HTTP/1.1 200 OK\r\n\r\nhello").await.unwrap();
            });
            let mut stream = TcpStream::connect((gateway.url.host_str().unwrap(), gateway.url.port().unwrap())).await.unwrap();
            stream.write_all(b"CONNECT destination.invalid:443 HTTP/1.1\r\nHost: destination.invalid:443\r\n\r\n").await.unwrap();
            assert!(header(&mut stream).await.starts_with("HTTP/1.1 200"));
            let mut received = [0; 5];
            stream.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"hello");
            server.await.unwrap();
        }).await.unwrap();
    }
}
