use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use moka::sync::Cache;
use reqwest::Url;

static OWNERS: LazyLock<Ownership> = LazyLock::new(Ownership::new);

struct Ownership {
    urls: Cache<String, &'static str>,
    origins: RwLock<BTreeMap<String, &'static str>>,
}

impl Ownership {
    fn new() -> Self {
        Self {
            urls: Cache::new(32_768),
            origins: RwLock::default(),
        }
    }

    fn remember(&self, provider: &'static str, url: &str) {
        self.urls.insert(url.to_owned(), provider);
    }

    fn origin(&self, provider: &'static str, url: &str) {
        if let Ok(mut parsed) = Url::parse(url) {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let prefix = format!("{}/", parsed.as_str().trim_end_matches('/'));
            self.origins
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .insert(prefix, provider);
        }
    }

    fn provider(&self, url: &str) -> &'static str {
        if let Some(provider) = self.urls.get(url) {
            return provider;
        }
        if let Some((_, provider)) = self
            .origins
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(prefix, _)| url.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
        {
            return provider;
        }
        let Ok(parsed) = Url::parse(url) else {
            return "";
        };
        let Some(host) = parsed.host_str() else {
            return "";
        };
        for (domain, provider) in [
            ("scdn.co", "spotify"),
            ("spotifycdn.com", "spotify"),
            ("spotifycdn.net", "spotify"),
            ("googleusercontent.com", "youtube"),
            ("ggpht.com", "youtube"),
            ("ytimg.com", "youtube"),
            ("mzstatic.com", "apple"),
            ("dzcdn.net", "deezer"),
            ("tidal.com", "tidal"),
        ] {
            if host == domain
                || host
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
            {
                return provider;
            }
        }
        ""
    }
}

/// Records artwork ownership without changing URLs used by caches or external integrations.
pub fn own_artwork(provider: &'static str, url: String) -> String {
    OWNERS.remember(provider, &url);
    url
}

/// Retains routing for persisted artwork served below a self-hosted provider's base URL.
pub fn register_artwork_origin(provider: &'static str, url: &str) {
    OWNERS.origin(provider, url);
}

pub fn artwork_provider(url: &str) -> &'static str {
    OWNERS.provider(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_artwork_keeps_its_provider_scope() {
        let owners = Ownership::new();
        let subsonic = "https://images.example/subsonic.jpg";
        let youtube = "https://images.example/youtube.jpg";
        owners.remember("subsonic", subsonic);
        owners.remember("youtube", youtube);
        assert_eq!(owners.provider(subsonic), "subsonic");
        assert_eq!(owners.provider(youtube), "youtube");
    }

    #[test]
    fn persisted_artwork_uses_registered_server_paths_and_known_cdns() {
        let owners = Ownership::new();
        owners.origin("subsonic", "https://library.example/music");
        assert_eq!(
            owners.provider("https://library.example/music/rest/getCoverArt?id=1"),
            "subsonic"
        );
        assert_eq!(
            owners.provider("https://library.example/music-other/image"),
            ""
        );
        assert_eq!(
            owners.provider("https://lh3.googleusercontent.com/image"),
            "youtube"
        );
        assert_eq!(
            owners.provider("https://googleusercontent.com.example/image"),
            ""
        );
    }
}
