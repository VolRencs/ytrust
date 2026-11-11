use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::Result;
use napi_derive::napi;
use serde_json::Value;
use ytrust::api::{self, ResolveOptions as CoreResolveOptions};
use ytrust::constants::{DEFAULT_PLAYLIST_ITEMS, DEFAULT_PLAYLIST_LIMIT, DEFAULT_TIMEOUT_SECS};
use ytrust::cookies::{self, LoadedCookies};
use ytrust::error::YtrustError;
use ytrust::http::RequestConfig;
use ytrust::youtube::fetch_player_source;

const COOKIE_CACHE_TTL: Duration = Duration::from_secs(60);
type CookieCache = Mutex<HashMap<(Option<String>, Option<String>), (Instant, LoadedCookies)>>;
static COOKIE_CACHE: OnceLock<CookieCache> = OnceLock::new();

fn load_cookies(options: &ResolveOptions) -> ytrust::error::Result<LoadedCookies> {
    let key = (
        options.cookies_path.clone(),
        options.cookies_from_browser.clone(),
    );
    if key.0.is_none() && key.1.is_none() {
        return cookies::load(None, None);
    }
    let cache = COOKIE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some((loaded_at, loaded)) = cache.get(&key)
        && loaded_at.elapsed() < COOKIE_CACHE_TTL
    {
        return Ok(loaded.clone());
    }
    let loaded = cookies::load(
        options.cookies_path.as_deref().map(std::path::Path::new),
        options.cookies_from_browser.as_deref(),
    )?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, (Instant::now(), loaded.clone()));
    }
    Ok(loaded)
}

#[napi(object)]
pub struct ResolveOptions {
    pub cookies_path: Option<String>,
    pub cookies_from_browser: Option<String>,
    pub proxy: Option<String>,
    pub timeout: Option<u32>,
    pub playlist_limit: Option<u32>,
    pub playlist_items: Option<String>,
    pub no_playlist: Option<bool>,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            cookies_path: None,
            cookies_from_browser: None,
            proxy: None,
            timeout: Some(DEFAULT_TIMEOUT_SECS as u32),
            playlist_limit: Some(DEFAULT_PLAYLIST_LIMIT as u32),
            playlist_items: Some(DEFAULT_PLAYLIST_ITEMS.to_string()),
            no_playlist: Some(false),
        }
    }
}

fn napi_error(error: YtrustError) -> napi::Error {
    napi::Error::from_reason(format!("[{}] {error}", error.code().as_str()))
}

fn request_config(
    options: &ResolveOptions,
) -> ytrust::error::Result<(RequestConfig, CoreResolveOptions)> {
    let cookies = load_cookies(options)?;
    let timeout = u64::from(options.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS as u32));
    let config = RequestConfig {
        proxy: options.proxy.clone(),
        cookies: cookies.header,
        sapisid: cookies.sapisid,
        sapisid1p: cookies.sapisid1p,
        sapisid3p: cookies.sapisid3p,
        timeout,
    };
    let core = CoreResolveOptions {
        no_playlist: options.no_playlist.unwrap_or(false),
        playlist_limit: u64::from(
            options
                .playlist_limit
                .unwrap_or(DEFAULT_PLAYLIST_LIMIT as u32),
        ),
        playlist_items: options
            .playlist_items
            .clone()
            .unwrap_or_else(|| DEFAULT_PLAYLIST_ITEMS.to_string()),
    };
    Ok((config, core))
}

#[napi]
pub fn version() -> String {
    ytrust::version().to_string()
}

#[napi]
pub async fn resolve(input: String, options: Option<ResolveOptions>) -> Result<Value> {
    let options = options.unwrap_or_default();
    let (config, core_options) = request_config(&options).map_err(napi_error)?;
    let resolved = api::resolve(&input, &config, &core_options)
        .await
        .map_err(napi_error)?;
    Ok(resolved.to_json())
}

/// Fetch player JavaScript through the same proxy/timeout/cookies settings as resolve.
#[napi]
pub async fn fetch_player_js(url: String, options: Option<ResolveOptions>) -> Result<String> {
    let options = options.unwrap_or_default();
    let (config, _) = request_config(&options).map_err(napi_error)?;
    fetch_player_source(&url, &config).await.map_err(napi_error)
}

/// Resolve the current player JS URL for a video.
#[napi]
pub async fn player_js_url(video_id: String, options: Option<ResolveOptions>) -> Result<String> {
    let options = options.unwrap_or_default();
    let (config, _) = request_config(&options).map_err(napi_error)?;
    ytrust::youtube::player_js_url(&video_id, &config)
        .await
        .map_err(napi_error)
}
