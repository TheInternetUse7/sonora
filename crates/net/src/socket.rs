use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use percent_encoding::percent_decode_str;
use reqwest::Url;
use rustls_platform_verifier::BuilderVerifierExt as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpStream, lookup_host};
use tokio_rustls::TlsConnector;
use tokio_socks::TargetAddr;
use tokio_socks::tcp::Socks5Stream;

use crate::route::Resolver;

const MAX_HEADER: usize = 16 * 1024;

/// A connected transport, including a TLS connection to an HTTPS proxy when required.
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

pub type Stream = Box<dyn Transport>;

pub(crate) async fn connect(
    resolver: &Resolver,
    provider: &str,
    destination: &Url,
) -> Result<Stream> {
    let host = host(destination)?;
    let port = destination
        .port_or_known_default()
        .context("destination has no port")?;
    let Some(proxy) = resolver.proxy(provider, destination) else {
        return Ok(Box::new(TcpStream::connect((host, port)).await?));
    };
    let proxy_host = self::host(&proxy)?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(1080);
    let socket = TcpStream::connect((proxy_host, proxy_port))
        .await
        .context("cannot connect to proxy")?;
    match proxy.scheme() {
        "socks5" | "socks5h" => {
            let target = match host.parse() {
                Ok(ip) => TargetAddr::Ip(std::net::SocketAddr::new(ip, port)),
                Err(_) if proxy.scheme() == "socks5h" => {
                    TargetAddr::Domain(Cow::Borrowed(host), port)
                }
                Err(_) => TargetAddr::Ip(
                    lookup_host((host, port))
                        .await
                        .context("cannot resolve destination")?
                        .next()
                        .context("destination resolved to no addresses")?,
                ),
            };
            let socket = match credentials(&proxy) {
                Some((username, password)) => {
                    Socks5Stream::connect_with_password_and_socket(
                        socket, target, &username, &password,
                    )
                    .await
                }
                None => Socks5Stream::connect_with_socket(socket, target).await,
            }
            .context("SOCKS proxy connection failed")?;
            Ok(Box::new(socket.into_inner()))
        }
        "http" | "https" => {
            let mut socket: Stream = match proxy.scheme() {
                "https" => {
                    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .with_safe_default_protocol_versions()?
                    .with_platform_verifier()?
                    .with_no_client_auth();
                    let server = rustls::pki_types::ServerName::try_from(proxy_host.to_owned())?;
                    Box::new(
                        TlsConnector::from(Arc::new(config))
                            .connect(server, socket)
                            .await
                            .context("cannot establish TLS with proxy")?,
                    )
                }
                _ => Box::new(socket),
            };
            let authority = match host.contains(':') {
                true => format!("[{host}]:{port}"),
                false => format!("{host}:{port}"),
            };
            let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
            if let Some((username, password)) = credentials(&proxy) {
                let encoded = STANDARD.encode(format!("{username}:{password}"));
                request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
            }
            request.push_str("\r\n");
            socket.write_all(request.as_bytes()).await?;
            let mut response = Vec::new();
            while !response.ends_with(b"\r\n\r\n") {
                ensure!(
                    response.len() < MAX_HEADER,
                    "proxy response headers are too large"
                );
                response.push(
                    socket
                        .read_u8()
                        .await
                        .context("proxy closed the connection during CONNECT")?,
                );
            }
            let status = std::str::from_utf8(&response)?
                .lines()
                .next()
                .context("proxy returned no status")?;
            let mut fields = status.split_whitespace();
            ensure!(
                matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1")),
                "proxy returned an invalid HTTP response"
            );
            let code: u16 = fields
                .next()
                .context("proxy returned no status code")?
                .parse()?;
            ensure!(
                (200..300).contains(&code),
                "proxy refused CONNECT with status {code}"
            );
            Ok(socket)
        }
        _ => bail!("unsupported proxy scheme"),
    }
}

fn host(url: &Url) -> Result<&str> {
    let host = url.host_str().context("URL has no host")?;
    Ok(host.trim_start_matches('[').trim_end_matches(']'))
}

fn credentials(url: &Url) -> Option<(String, String)> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    Some((
        percent_decode_str(url.username())
            .decode_utf8_lossy()
            .into_owned(),
        percent_decode_str(url.password().unwrap_or_default())
            .decode_utf8_lossy()
            .into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hyper_util::client::proxy::matcher::Matcher;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{Policy, Settings};

    const LIMIT: Duration = Duration::from_secs(3);

    fn resolver(proxy: String) -> Resolver {
        Resolver::new(
            &Settings {
                global: Policy::Custom {
                    url: proxy,
                    bypass: String::new(),
                },
                ..Settings::default()
            },
            Matcher::builder().build(),
        )
    }

    async fn header(stream: &mut TcpStream) -> Vec<u8> {
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
            assert!(header.len() < 16 * 1024);
        }
        header
    }

    #[tokio::test]
    async fn http_connect_preserves_destination_bytes_and_proxy_authentication() {
        tokio::time::timeout(LIMIT, async {
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let settings = resolver(format!(
                "http://user:password@{}",
                proxy.local_addr().unwrap()
            ));
            let server = tokio::spawn(async move {
                let (mut connection, _) = proxy.accept().await.unwrap();
                let request = String::from_utf8(header(&mut connection).await).unwrap();
                assert!(request.starts_with("CONNECT destination.invalid:443 HTTP/1.1\r\n"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("proxy-authorization: basic dxnlcjpwyxnzd29yza==")
                );
                connection
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nhello")
                    .await
                    .unwrap();
            });
            let mut connected = connect(
                &settings,
                "youtube",
                &Url::parse("https://destination.invalid/").unwrap(),
            )
            .await
            .unwrap();
            let mut received = [0; 5];
            connected.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"hello");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn socks5h_sends_the_destination_hostname_to_the_proxy() {
        tokio::time::timeout(LIMIT, async {
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let settings = resolver(format!("socks5h://{}", proxy.local_addr().unwrap()));
            let server = tokio::spawn(async move {
                let (mut connection, _) = proxy.accept().await.unwrap();
                assert_eq!(connection.read_u8().await.unwrap(), 5);
                let methods = connection.read_u8().await.unwrap();
                let mut methods = vec![0; usize::from(methods)];
                connection.read_exact(&mut methods).await.unwrap();
                assert!(methods.contains(&0));
                connection.write_all(&[5, 0]).await.unwrap();
                let mut request = [0; 4];
                connection.read_exact(&mut request).await.unwrap();
                assert_eq!(request, [5, 1, 0, 3]);
                let length = connection.read_u8().await.unwrap();
                let mut host = vec![0; usize::from(length)];
                connection.read_exact(&mut host).await.unwrap();
                assert_eq!(host, b"destination.invalid");
                assert_eq!(connection.read_u16().await.unwrap(), 443);
                connection
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();
                connection.write_all(b"hello").await.unwrap();
            });
            let mut connected = connect(
                &settings,
                "youtube",
                &Url::parse("https://destination.invalid/").unwrap(),
            )
            .await
            .unwrap();
            let mut received = [0; 5];
            connected.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"hello");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_refused_proxy_does_not_fall_back_to_the_destination() {
        tokio::time::timeout(LIMIT, async {
            let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let settings = resolver(format!("http://{}", proxy.local_addr().unwrap()));
            let server = tokio::spawn(async move {
                let (mut connection, _) = proxy.accept().await.unwrap();
                header(&mut connection).await;
                connection
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            });
            let url =
                Url::parse(&format!("https://{}/", destination.local_addr().unwrap())).unwrap();
            assert!(connect(&settings, "youtube", &url).await.is_err());
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
    async fn socks5_authenticates_and_resolves_the_destination_locally() {
        tokio::time::timeout(LIMIT, async {
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let settings = resolver(format!(
                "socks5://user:p%40ss@{}",
                proxy.local_addr().unwrap()
            ));
            let server = tokio::spawn(async move {
                let (mut connection, _) = proxy.accept().await.unwrap();
                assert_eq!(connection.read_u8().await.unwrap(), 5);
                let count = connection.read_u8().await.unwrap();
                let mut methods = vec![0; usize::from(count)];
                connection.read_exact(&mut methods).await.unwrap();
                assert!(methods.contains(&2));
                connection.write_all(&[5, 2]).await.unwrap();
                assert_eq!(connection.read_u8().await.unwrap(), 1);
                let length = connection.read_u8().await.unwrap();
                let mut username = vec![0; usize::from(length)];
                connection.read_exact(&mut username).await.unwrap();
                let length = connection.read_u8().await.unwrap();
                let mut password = vec![0; usize::from(length)];
                connection.read_exact(&mut password).await.unwrap();
                assert_eq!(username, b"user");
                assert_eq!(password, b"p@ss");
                connection.write_all(&[1, 0]).await.unwrap();
                let mut request = [0; 4];
                connection.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..3], &[5, 1, 0]);
                let mut address = vec![
                    0;
                    match request[3] {
                        1 => 4,
                        4 => 16,
                        other => panic!("expected a resolved address, got {other}"),
                    }
                ];
                connection.read_exact(&mut address).await.unwrap();
                assert_eq!(connection.read_u16().await.unwrap(), 443);
                connection
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();
            });
            connect(
                &settings,
                "youtube",
                &Url::parse("https://localhost/").unwrap(),
            )
            .await
            .unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
