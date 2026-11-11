use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hyper_util::client::proxy::matcher::Matcher;
use reqwest::Url;

use crate::{Policy, Settings};

/// Resolves provider policies against a startup snapshot of the environment.
#[derive(Clone)]
pub(crate) struct Resolver {
    global: Arc<Matcher>,
    providers: BTreeMap<String, Arc<Matcher>>,
}

impl Resolver {
    pub fn http_proxy(&self, provider: &str) -> reqwest::Proxy {
        let resolver = self.clone();
        let provider = provider.to_owned();
        reqwest::Proxy::custom(move |url| resolver.proxy(&provider, url))
    }

    pub fn new(settings: &Settings, environment: impl Into<Arc<Matcher>>) -> Self {
        let environment = environment.into();
        Self {
            global: matcher(&settings.global, &environment),
            providers: settings
                .providers
                .iter()
                .map(|(provider, policy)| (provider.clone(), matcher(policy, &environment)))
                .collect(),
        }
    }

    pub fn proxy(&self, provider: &str, destination: &Url) -> Option<Url> {
        let mut destination = destination.clone();
        match destination.scheme() {
            "ws" => destination.set_scheme("http").ok()?,
            "wss" => destination.set_scheme("https").ok()?,
            _ => {}
        }
        let matcher = self.providers.get(provider).unwrap_or(&self.global);
        let proxy = matcher.intercept(&destination.as_str().parse().ok()?)?;
        let mut url = Url::parse(&proxy.uri().to_string()).ok()?;
        if let Some((username, password)) = proxy.raw_auth() {
            url.set_username(username).ok()?;
            url.set_password(Some(password)).ok()?;
        } else if let Some(encoded) = proxy
            .basic_auth()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
        {
            let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
            let (username, password) = decoded.split_once(':')?;
            url.set_username(username).ok()?;
            url.set_password(Some(password)).ok()?;
        }
        Some(url)
    }
}

fn matcher(policy: &Policy, environment: &Arc<Matcher>) -> Arc<Matcher> {
    match policy {
        Policy::Automatic => environment.clone(),
        Policy::Direct => Arc::new(Matcher::builder().build()),
        Policy::Custom { url, bypass } => Arc::new(
            Matcher::builder()
                .all(url.as_str())
                .no(bypass.as_str())
                .build(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::Policy;

    fn configured(bypass: &str) -> Settings {
        Settings {
            global: Policy::Custom {
                url: "socks5h://proxy.example:1080".to_owned(),
                bypass: bypass.to_owned(),
            },
            providers: BTreeMap::from([("subsonic".to_owned(), Policy::Direct)]),
        }
    }

    #[test]
    fn provider_routing_is_independent_of_the_global_proxy() {
        let resolver = Resolver::new(&configured(""), Matcher::builder().build());
        let target = Url::parse("https://music.example/").unwrap();
        assert!(resolver.proxy("subsonic", &target).is_none());
        let proxy = resolver.proxy("youtube", &target).unwrap();
        assert_eq!(proxy.scheme(), "socks5h");
        assert_eq!(proxy.host_str(), Some("proxy.example"));
        assert_eq!(proxy.port(), Some(1080));
    }

    #[test]
    fn bypasses_match_domains_subdomains_and_ip_networks() {
        let resolver = Resolver::new(
            &configured("example.org, 192.0.2.0/24, ::1"),
            Matcher::builder().build(),
        );
        for target in [
            "https://example.org/",
            "https://music.example.org/",
            "http://192.0.2.10/",
            "http://[::1]/",
        ] {
            assert!(
                resolver
                    .proxy("youtube", &Url::parse(target).unwrap())
                    .is_none()
            );
        }
        assert!(
            resolver
                .proxy("youtube", &Url::parse("https://notexample.org/").unwrap())
                .is_some()
        );
    }

    #[test]
    fn a_wildcard_bypass_connects_directly() {
        let resolver = Resolver::new(&configured("*"), Matcher::builder().build());
        assert!(
            resolver
                .proxy("youtube", &Url::parse("https://music.example/").unwrap())
                .is_none()
        );
    }

    #[test]
    fn automatic_uses_protocol_specific_environment_routes() {
        let resolver = Resolver::new(
            &Settings::default(),
            Matcher::builder()
                .http("http://http-proxy.example:8080")
                .https("http://https-proxy.example:8080")
                .no("direct.example")
                .build(),
        );
        for (destination, expected) in [
            ("http://music.example/", "http-proxy.example"),
            ("https://music.example/", "https-proxy.example"),
            ("wss://music.example/", "https-proxy.example"),
        ] {
            assert_eq!(
                resolver
                    .proxy("youtube", &Url::parse(destination).unwrap())
                    .unwrap()
                    .host_str(),
                Some(expected)
            );
        }
        assert!(
            resolver
                .proxy("youtube", &Url::parse("https://direct.example/").unwrap())
                .is_none()
        );
    }

    #[test]
    fn explicit_policies_override_environment_routes() {
        let resolver = Resolver::new(
            &configured(""),
            Matcher::builder()
                .all("http://environment.example:8080")
                .build(),
        );
        let target = Url::parse("https://music.example/").unwrap();
        assert!(resolver.proxy("subsonic", &target).is_none());
        assert_eq!(
            resolver.proxy("youtube", &target).unwrap().host_str(),
            Some("proxy.example")
        );
    }

    #[test]
    fn authentication_survives_proxy_resolution() {
        for proxy in [
            "http://user:p%40ss@proxy.example:8080",
            "socks5h://user:p%40ss@proxy.example:1080",
        ] {
            let settings = Settings {
                global: Policy::Custom {
                    url: proxy.to_owned(),
                    bypass: String::new(),
                },
                ..Settings::default()
            };
            let resolver = Resolver::new(&settings, Matcher::builder().build());
            let resolved = resolver
                .proxy("youtube", &Url::parse("https://music.example/").unwrap())
                .unwrap();
            assert_eq!(resolved.username(), "user");
            assert_eq!(resolved.password(), Some("p%40ss"));
        }
    }
}
