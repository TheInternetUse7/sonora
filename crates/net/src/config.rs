use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context as _, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};

/// A global routing policy and optional provider-specific overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub global: Policy,
    pub providers: BTreeMap<String, Policy>,
}

/// How outbound requests select a proxy.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Automatic,
    Direct,
    Custom {
        url: String,
        #[serde(default)]
        bypass: String,
    },
}

impl Settings {
    pub fn effective(&self, provider: &str) -> &Policy {
        self.providers.get(provider).unwrap_or(&self.global)
    }

    pub fn validate(&self) -> Result<()> {
        self.global
            .validate()
            .context("invalid global proxy configuration")?;
        for policy in self.providers.values() {
            policy
                .validate()
                .context("invalid provider proxy configuration")?;
        }
        Ok(())
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        let Self::Custom { url, .. } = self else {
            return Ok(());
        };
        let parsed = Url::parse(url).context("invalid proxy URL")?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h"),
            "unsupported proxy scheme"
        );
        ensure!(parsed.host_str().is_some(), "proxy URL has no host");
        ensure!(
            parsed.port() != Some(0),
            "proxy port must be greater than zero"
        );
        ensure!(
            matches!(parsed.path(), "" | "/")
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "proxy URL must not contain a path, query, or fragment"
        );
        Ok(())
    }
}

impl fmt::Debug for Policy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automatic => formatter.write_str("Automatic"),
            Self::Direct => formatter.write_str("Direct"),
            Self::Custom { url, bypass } => {
                let redacted = Url::parse(url).ok().map(|mut url| {
                    url.set_username("").ok();
                    url.set_password(None).ok();
                    url.to_string()
                });
                formatter
                    .debug_struct("Custom")
                    .field("url", &redacted)
                    .field("bypass", bypass)
                    .finish()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(url: &str) -> Policy {
        Policy::Custom {
            url: url.to_owned(),
            bypass: String::new(),
        }
    }

    #[test]
    fn missing_settings_use_automatic_routing() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.effective("youtube"), &Policy::Automatic);
    }

    #[test]
    fn direct_subsonic_overrides_a_global_proxy() {
        let settings = Settings {
            global: custom("socks5h://proxy.example:1080"),
            providers: BTreeMap::from([("subsonic".to_owned(), Policy::Direct)]),
        };
        assert_eq!(settings.effective("subsonic"), &Policy::Direct);
        assert_eq!(settings.effective("youtube"), &settings.global);
    }

    #[test]
    fn youtube_can_use_a_proxy_while_other_services_are_direct() {
        let youtube = custom("socks5h://proxy.example:1080");
        let settings = Settings {
            global: Policy::Direct,
            providers: BTreeMap::from([("youtube".to_owned(), youtube.clone())]),
        };
        assert_eq!(settings.effective("youtube"), &youtube);
        assert_eq!(settings.effective("subsonic"), &Policy::Direct);
        assert_eq!(settings.effective(""), &Policy::Direct);
    }

    #[test]
    fn provider_policies_survive_a_settings_round_trip() {
        let settings = Settings {
            global: Policy::Direct,
            providers: BTreeMap::from([
                ("youtube".to_owned(), custom("socks5h://first.example:1080")),
                ("spotify".to_owned(), custom("http://second.example:8080")),
            ]),
        };
        let saved = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&saved).unwrap(), settings);
    }

    #[test]
    fn custom_proxy_rejects_invalid_endpoints() {
        for url in [
            "",
            "proxy.example:1080",
            "file:///proxy",
            "socks4://proxy.example:1080",
            "http://proxy.example:0",
            "http://proxy.example:70000",
            "http://proxy.example/path",
            "http://proxy.example?token=value",
            "http://proxy.example#fragment",
        ] {
            assert!(custom(url).validate().is_err(), "accepted {url}");
        }
    }

    #[test]
    fn custom_proxy_accepts_supported_schemes_and_authentication() {
        for url in [
            "http://proxy.example:8080",
            "https://proxy.example",
            "socks5://proxy.example:1080",
            "socks5h://user:password@proxy.example:1080",
            "http://[::1]:8080",
        ] {
            assert!(custom(url).validate().is_ok(), "rejected {url}");
        }
    }

    #[test]
    fn diagnostics_do_not_expose_proxy_credentials() {
        let policy = custom("socks5h://private-user:private-password@proxy.example:1080");
        let message = format!("{policy:?}");
        assert!(!message.contains("private-user"));
        assert!(!message.contains("private-password"));
    }
}
