use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use anyhow::{Result, ensure};
use hyper_util::client::proxy::matcher::Matcher;
use reqwest::Url;
use tokio::runtime::Handle;

use crate::gateway::{Gateway, Listener};
use crate::route::Resolver;
use crate::{Settings, socket};

static ACTIVE: OnceLock<Network> = OnceLock::new();
static DEFAULTS: LazyLock<Settings> = LazyLock::new(Settings::default);
static ENVIRONMENT: LazyLock<Arc<Matcher>> = LazyLock::new(|| Arc::new(Matcher::from_env()));
static FALLBACK: LazyLock<Arc<Resolver>> =
    LazyLock::new(|| Arc::new(Resolver::new(&DEFAULTS, ENVIRONMENT.clone())));
static INHERITED: OnceLock<Mutex<Option<Inherited>>> = OnceLock::new();

/// The adapter reserved for dependency clients that only support process-wide proxy settings.
struct Inherited {
    provider: String,
    listener: Listener,
}

struct Network {
    settings: Settings,
    resolver: Arc<Resolver>,
    handle: Handle,
    gateways: Mutex<BTreeMap<String, Gateway>>,
}

/// Makes unconfigured dependency clients inherit one provider's adapter until process exit.
/// Call once at startup, then initialize routing before constructing any network clients.
/// All other clients must use scoped builders; Automatic keeps the original environment.
///
/// # Safety
/// On non-Windows platforms this must run before any threads start or native code can read
/// the environment concurrently. It never restores or changes the environment after startup.
pub unsafe fn inherit(provider: &str) -> Result<()> {
    ensure!(
        ACTIVE.get().is_none(),
        "network configuration is already initialized"
    );
    LazyLock::force(&ENVIRONMENT);
    let listener = Listener::bind()?;
    let url = listener.url.clone();
    ensure!(
        INHERITED
            .set(Mutex::new(Some(Inherited {
                provider: provider.to_owned(),
                listener,
            })))
            .is_ok(),
        "default proxy adapter is already prepared"
    );
    // SAFETY: the caller guarantees a single-threaded startup on non-Windows platforms.
    unsafe {
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(key, url.as_str());
        }
        // Destination bypasses belong to the adapter, never to the inherited client.
        for key in ["NO_PROXY", "no_proxy"] {
            std::env::set_var(key, "");
        }
        // Sonora is not a CGI host. This flag disables reqwest's environment proxies, even
        // though the adapter address above is ours rather than an untrusted request header.
        std::env::remove_var("REQUEST_METHOD");
    }
    Ok(())
}

/// Installs the startup policy before any network clients are constructed.
pub fn init(settings: Settings, handle: Handle) -> Result<()> {
    settings.validate()?;
    ensure!(
        ACTIVE.get().is_none(),
        "network configuration is already initialized"
    );
    let resolver = Arc::new(Resolver::new(&settings, ENVIRONMENT.clone()));
    let mut gateways = BTreeMap::new();
    if let Some(inherited) = INHERITED.get().and_then(|pending| {
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }) {
        let gateway = Gateway::start_from(
            inherited.provider.clone(),
            resolver.clone(),
            &handle,
            inherited.listener,
        )?;
        gateways.insert(inherited.provider, gateway);
    }
    ensure!(
        ACTIVE
            .set(Network {
                settings,
                resolver,
                handle,
                gateways: Mutex::new(gateways)
            })
            .is_ok(),
        "network configuration is already initialized"
    );
    Ok(())
}

pub fn active() -> &'static Settings {
    ACTIVE
        .get()
        .map(|network| &network.settings)
        .unwrap_or(&DEFAULTS)
}

fn resolver() -> Arc<Resolver> {
    ACTIVE
        .get()
        .map(|network| network.resolver.clone())
        .unwrap_or_else(|| FALLBACK.clone())
}

/// Builds a provider-scoped client. An empty scope selects the global policy.
pub fn http(provider: &str) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .proxy(resolver().http_proxy(provider))
}

pub fn client(provider: &str) -> reqwest::Client {
    http(provider)
        .build()
        .expect("cannot build the configured HTTP client")
}

pub fn blocking(provider: &str) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .proxy(resolver().http_proxy(provider))
}

/// Provides a scoped HTTP adapter once application networking is initialized.
pub fn gateway(provider: &str) -> Result<Option<Url>> {
    let Some(network) = ACTIVE.get() else {
        return Ok(None);
    };
    let mut gateways = network
        .gateways
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(gateway) = gateways.get(provider) {
        return Ok(Some(gateway.url.clone()));
    }
    let gateway = Gateway::start(
        provider.to_owned(),
        network.resolver.clone(),
        &network.handle,
    )?;
    let url = gateway.url.clone();
    gateways.insert(provider.to_owned(), gateway);
    Ok(Some(url))
}

/// Connects to the destination through its scope's policy, without applying destination TLS.
pub async fn connect(provider: &str, destination: &Url) -> Result<socket::Stream> {
    socket::connect(&resolver(), provider, destination).await
}
