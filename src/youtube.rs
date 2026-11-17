use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell as AsyncOnceCell;
use tokio::task::JoinHandle;
use url::Url;

use crate::constants::{
    TV_SAMSUNG_CLIENT_ID, TV_SAMSUNG_CLIENT_VERSION, TV_SAMSUNG_UA, VISIONOS_CLIENT_ID,
    VISIONOS_CLIENT_VERSION, VISIONOS_UA, WEB_CLIENT_ID, WEB_CLIENT_VERSION,
    WEB_EMBEDDED_CLIENT_ID, WEB_EMBEDDED_CLIENT_VERSION, WEB_EMBEDDED_UA, WEB_UA,
};
use crate::error::{ErrorCode, Result, YtrustError, best_error};
use crate::http::{
    InnertubeRequest, MAX_PLAYER_JS_BYTES, client as build_client, fetch_text, fetch_text_limited,
    post_innertube,
};
use crate::sync::MutexExt;

pub use crate::http::RequestConfig;

#[derive(Debug, Clone)]
pub enum Target {
    Video {
        id: String,
    },
    Playlist {
        id: String,
        video_id: Option<String>,
    },
    Search {
        query: String,
        limit: u64,
    },
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub duration: Option<u64>,
    pub thumbnails: Vec<Thumbnail>,
}

#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    pub entries: Vec<PlaylistEntry>,
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub channel: String,
    pub channel_id: String,
    pub webpage_url: String,
    pub description: String,
    pub duration_secs: Option<u64>,
    pub view_count: Option<u64>,
    pub upload_date: String,
    pub is_live: bool,
    pub thumbnails: Vec<Thumbnail>,
    pub subtitles: Vec<SubtitleTrack>,
    pub automatic_captions: Vec<SubtitleTrack>,
    pub formats: Vec<Format>,
    pub hls_manifest_url: Option<String>,
    pub player_js_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub url: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SubtitleTrack {
    pub language: String,
    pub name: String,
    pub url: String,
    pub is_auto: bool,
}

#[derive(Debug, Clone)]
pub struct Format {
    pub itag: u64,
    pub url: String,
    pub signature_cipher: Option<String>,
    pub cipher: Option<String>,
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub has_video: bool,
    pub has_audio: bool,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub fps: Option<u64>,
    pub bitrate: u64,
    pub audio_bitrate: Option<u64>,
    pub content_length: Option<u64>,
    pub quality_label: Option<String>,
    pub is_hls: bool,
    pub is_live: bool,
    pub audio_track_name: Option<String>,
    pub audio_track_id: Option<String>,
    pub audio_is_default: bool,
    pub language: Option<String>,
    pub needs_challenge: bool,
    pub user_agent: &'static str,
}

impl Format {
    /// Best available size estimate for progress reporting and JSON output.
    pub fn estimated_size(&self, duration_secs: Option<u64>) -> Option<u64> {
        self.content_length
            .filter(|size| *size > 0)
            .or_else(|| self.progress_size(duration_secs))
    }

    /// Size estimate for the requested duration, ignoring a content length
    /// that describes the complete stream (for example with section downloads).
    pub fn progress_size(&self, duration_secs: Option<u64>) -> Option<u64> {
        let bitrate = self
            .audio_bitrate
            .filter(|bitrate| *bitrate > 0)
            .unwrap_or(self.bitrate);
        duration_secs
            .filter(|duration| *duration > 0 && bitrate > 0)
            .map(|duration| bitrate.saturating_mul(duration) / 8)
    }

    /// True when the stream still needs signature or `n` challenge solving.
    pub fn needs_challenge(&self) -> bool {
        self.signature_cipher.is_some()
            || self.cipher.is_some()
            || (!self.url.is_empty() && url_has_n(&self.url))
    }
}

pub fn classify(url: &str, no_playlist: bool) -> Result<Target> {
    if let Some(rest) = url.strip_prefix("ytsearch") {
        let (count, query) = rest.split_once(':').ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                "search syntax is ytsearchN:query or ytsearch:query",
            )
        })?;
        let limit = if count.is_empty() {
            1
        } else {
            count.parse::<u64>().map_err(|_| {
                YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("invalid search count: {count}"),
                )
            })?
        };
        if query.trim().is_empty() {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "search query is empty",
            ));
        }
        return Ok(Target::Search {
            query: query.to_string(),
            limit: limit.max(1),
        });
    }

    let parsed = Url::parse(url)
        .map_err(|_| YtrustError::coded(ErrorCode::InvalidInput, format!("invalid URL: {url}")))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_start_matches("m.");

    if !matches!(
        host,
        "youtube.com" | "youtu.be" | "music.youtube.com" | "youtube-nocookie.com"
    ) {
        return Err(YtrustError::coded(
            ErrorCode::UnsupportedUrl,
            format!("not a YouTube URL: {url}\nOnly YouTube is supported."),
        ));
    }

    let path = parsed.path();
    let mut list = None;
    let mut query_video = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "list" if !value.is_empty() && list.is_none() => list = Some(value.into_owned()),
            "v" if !value.is_empty() && query_video.is_none() => {
                query_video = Some(value.into_owned());
            }
            _ => {}
        }
    }
    let path_video = video_id_from_url(&parsed);
    let video_id = query_video.as_deref().and_then(valid_id).or(path_video);

    if let Some(ref list) = list
        && !no_playlist
    {
        return Ok(Target::Playlist {
            id: list.clone(),
            video_id,
        });
    }

    if let Some(video_id) = video_id {
        return Ok(Target::Video { id: video_id });
    }

    if let Some(video) = query_video {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("invalid video ID: {video}"),
        ));
    }

    if path.starts_with("/playlist") {
        let list = list.ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                "playlist URL has no list parameter",
            )
        })?;
        return Ok(Target::Playlist {
            id: list,
            video_id: None,
        });
    }

    if path.starts_with("/@")
        || path.starts_with("/channel/")
        || path.starts_with("/user/")
        || path.starts_with("/c/")
    {
        return Err(YtrustError::coded(
            ErrorCode::UnsupportedUrl,
            "channel URLs are not supported yet; use a video or playlist URL",
        ));
    }

    Err(YtrustError::coded(
        ErrorCode::UnsupportedUrl,
        format!("unsupported YouTube URL: {url}"),
    ))
}

fn video_id_from_url(url: &Url) -> Option<String> {
    let host = url.host_str()?.trim_start_matches("www.");
    let path = url.path();

    if host == "youtu.be" {
        let id = path.trim_start_matches('/').split('/').next()?;
        return valid_id(id);
    }

    if let Some(id) = url
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value)
    {
        return valid_id(&id);
    }

    for prefix in ["/shorts/", "/embed/", "/live/", "/v/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest.split('/').next()?;
            return valid_id(id);
        }
    }

    None
}

fn valid_id(id: &str) -> Option<String> {
    let id = id.trim();
    if id.len() == 11
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        Some(id.to_string())
    } else {
        None
    }
}

pub async fn fetch_video(video_id: &str, config: &RequestConfig) -> Result<VideoInfo> {
    let client = build_client(config)?;
    let mut errors = Vec::new();
    let mut partial = None;

    for &client_kind in client_order(config) {
        tracing::debug!("trying YouTube player client {client_kind:?}");
        match fetch_player_response(&client, video_id, client_kind, config).await {
            Ok(fetched) => {
                let PlayerFetch {
                    response,
                    watch_metadata,
                    player_js_url: known_player_url,
                    source_prefetch,
                } = fetched;
                match parse_video_info(video_id, response, client_kind, None) {
                    Ok(mut info) if !info.formats.is_empty() => {
                        if let Some(watch) = watch_metadata {
                            merge_watch_metadata(&mut info, watch);
                        }
                        if !config.resolve_player_js || !needs_player_js(&info) {
                            return Ok(info);
                        }
                        let player_url = match known_player_url {
                            Some(url) => Some(url),
                            None => match player_js_url(video_id, config).await {
                                Ok(url) => Some(url),
                                Err(error) => {
                                    errors.push(error);
                                    None
                                }
                            },
                        };
                        let Some(player_url) = player_url else {
                            partial = Some(info);
                            continue;
                        };
                        info.player_js_url = Some(player_url);
                        if let Some(prefetch) = source_prefetch
                            && let Some(Err(error)) = prefetch.wait().await
                        {
                            tracing::debug!("player JS prefetch failed: {error}");
                        }
                        return Ok(info);
                    }
                    Ok(_) => {
                        errors.push(YtrustError::coded(
                            ErrorCode::Unavailable,
                            "YouTube returned no formats",
                        ));
                    }
                    Err(error) => errors.push(error),
                }
            }
            Err(error) => errors.push(error),
        }
    }

    match fetch_web_embedded_video(&client, video_id, config).await {
        Ok(info) if !info.formats.is_empty() => return Ok(info),
        Ok(_) => {
            errors.push(YtrustError::coded(
                ErrorCode::Unavailable,
                "YouTube returned no formats",
            ));
        }
        Err(error) => errors.push(error),
    }

    if let Some(info) = partial
        && info.formats.iter().any(|format| !format.url.is_empty())
    {
        return Ok(info);
    }

    Err(best_error(errors).unwrap_or_else(|| {
        YtrustError::coded(
            ErrorCode::Unavailable,
            "YouTube did not return a playable response",
        )
    }))
}

async fn fetch_watch_html(
    client: &reqwest::Client,
    video_id: &str,
    config: &RequestConfig,
) -> Option<String> {
    let watch_url = format!("https://www.youtube.com/watch?v={video_id}&hl=en&gl=US");
    let started = Instant::now();
    let html = fetch_text(client, &watch_url, config, WEB_UA, None)
        .await
        .ok();
    tracing::debug!(
        "watch page fetched: bytes={} elapsed_ms={}",
        html.as_ref().map_or(0, String::len),
        started.elapsed().as_millis()
    );
    html
}

fn merge_watch_metadata(info: &mut VideoInfo, response: PlayerResponse) {
    let Some(details) = response.video_details else {
        return;
    };
    if info.title.is_empty() {
        info.title = details.title;
    }
    if info.uploader.is_empty() {
        info.uploader = details.author.unwrap_or_default();
    }
    if info.channel.is_empty() {
        info.channel = details.owner_channel_name.unwrap_or_default();
    }
    if info.channel_id.is_empty() {
        info.channel_id = details.channel_id.unwrap_or_default();
    }
    if info.description.is_empty() {
        info.description = details.short_description.unwrap_or_default();
    }
    if info.duration_secs.is_none() {
        info.duration_secs = details.length_seconds.filter(|value| *value > 0);
    }
    if info.view_count.is_none() {
        info.view_count = details.view_count;
    }
    if info.thumbnails.is_empty()
        && let Some(list) = details.thumbnail
    {
        info.thumbnails = list
            .thumbnails
            .into_iter()
            .filter_map(|item| {
                Some(Thumbnail {
                    url: item.url?,
                    width: item.width,
                    height: item.height,
                })
            })
            .collect();
    }
    if let Some(renderer) = response
        .microformat
        .and_then(|microformat| microformat.player_microformat_renderer)
    {
        if info.upload_date.is_empty() {
            info.upload_date = renderer
                .get("uploadDate")
                .or_else(|| renderer.get("publishDate"))
                .and_then(Value::as_str)
                .map(normalize_upload_date)
                .unwrap_or_default();
        }
        if info.channel.is_empty() {
            info.channel = renderer
                .get("ownerChannelName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
    }
}

fn needs_player_js(info: &VideoInfo) -> bool {
    info.formats.iter().any(Format::needs_challenge)
}

fn url_has_n(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|url| {
        url.query_pairs()
            .any(|(key, value)| key == "n" && !value.is_empty())
    })
}

const PLAYER_JS_URL_TTL: Duration = Duration::from_mins(30);

static PLAYER_JS_URL_CACHE: LazyLock<Mutex<Option<(Instant, String)>>> =
    LazyLock::new(|| Mutex::new(None));

fn cached_player_js_url() -> Option<String> {
    let cache = &*PLAYER_JS_URL_CACHE;
    let guard = cache.lock_unpoisoned();
    let (cached_at, url) = guard.as_ref()?;
    (cached_at.elapsed() < PLAYER_JS_URL_TTL).then(|| url.clone())
}

fn remember_player_js_url(url: &str) {
    let cache = &*PLAYER_JS_URL_CACHE;
    let mut guard = cache.lock_unpoisoned();
    *guard = Some((Instant::now(), url.to_string()));
}

/// Resolve the current player JS URL from the watch or embed page.
pub async fn player_js_url(video_id: &str, config: &RequestConfig) -> Result<String> {
    if valid_id(video_id).is_none() {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("invalid video ID: {video_id}"),
        ));
    }
    if let Some(url) = cached_player_js_url() {
        return Ok(url);
    }

    let url = fetch_player_js_url(video_id, config).await?;
    remember_player_js_url(&url);
    Ok(url)
}

async fn fetch_player_js_url(video_id: &str, config: &RequestConfig) -> Result<String> {
    let client = build_client(config)?;
    let watch_url = format!("https://www.youtube.com/watch?v={video_id}&hl=en&gl=US");
    let watch_error = match fetch_text(&client, &watch_url, config, WEB_UA, None).await {
        Ok(html) => match extract_player_js_url(&html) {
            Some(url) => return Ok(url),
            None => YtrustError::coded(
                ErrorCode::InvalidResponse,
                "YouTube watch page has no player JS URL",
            ),
        },
        Err(error) => error,
    };

    let embed_url = format!("https://www.youtube.com/embed/{video_id}?html5=1");
    match fetch_text(
        &client,
        &embed_url,
        config,
        WEB_EMBEDDED_UA,
        Some("https://www.reddit.com/"),
    )
    .await
    {
        Ok(html) => extract_player_js_url(&html).ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidResponse,
                "YouTube embed page has no player JS URL",
            )
        }),
        Err(_) => Err(watch_error),
    }
}

/// Fetch the player JavaScript source through the configured proxy/timeout.
pub async fn fetch_player_source(url: &str, config: &RequestConfig) -> Result<String> {
    Ok(player_source(url, config).await?.to_string())
}

fn validate_player_source_url(url: &str) -> Result<()> {
    let parsed = Url::parse(url)
        .map_err(|_| YtrustError::coded(ErrorCode::InvalidInput, "invalid player JS URL"))?;
    if parsed.scheme() != "https"
        || parsed.username() != ""
        || parsed.password().is_some()
        || !parsed
            .host_str()
            .is_some_and(|host| host == "youtube.com" || host.ends_with(".youtube.com"))
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "player JS URL must use https and a youtube.com host",
        ));
    }
    Ok(())
}

async fn fetch_player_source_uncached(url: &str, config: &RequestConfig) -> Result<Arc<str>> {
    validate_player_source_url(url)?;
    let client = build_client(config)?;
    let started = Instant::now();
    tracing::debug!("player JS fetch started: url={url}");
    let source =
        fetch_text_limited(&client, url, config, WEB_UA, None, MAX_PLAYER_JS_BYTES).await?;
    tracing::debug!(
        "player JS fetched: url={} bytes={} elapsed_ms={}",
        url,
        source.len(),
        started.elapsed().as_millis()
    );
    Ok(Arc::from(source))
}

fn extract_player_js_url(html: &str) -> Option<String> {
    let ytcfg = extract_ytcfg(html)?;
    player_js_url_from_cfg(&ytcfg)
}

fn player_js_url_from_cfg(ytcfg: &Value) -> Option<String> {
    ytcfg
        .get("PLAYER_JS_URL")
        .and_then(Value::as_str)
        .or_else(|| {
            ytcfg
                .pointer("/WEB_PLAYER_CONTEXT_CONFIGS/WEB_PLAYER_CONTEXT_CONFIG_ID_EMBEDDED_PLAYER/jsUrl")
                .and_then(Value::as_str)
        })
        .or_else(|| ytcfg.get("JS_URL").and_then(Value::as_str))
        .map(|url| {
            let url = if url.starts_with("http") {
                url.to_string()
            } else if url.starts_with("//") {
                format!("https:{url}")
            } else {
                format!("https://www.youtube.com{url}")
            };
            normalize_player_js_url(url)
        })
}

/// The EJS solver does not understand the newer `*_tcl` player variants yet.
/// Use the equivalent `*_vflset` variant for both signature timestamp
/// extraction and challenge solving.
fn normalize_player_js_url(url: String) -> String {
    url.replace("player_es6_tcl.vflset", "player_es6.vflset")
        .replace("player_es6_tcc.vflset", "player_es6.vflset")
        .replace("player_ias_tcl.vflset", "player_ias.vflset")
        .replace("player_ias_tcc.vflset", "player_ias.vflset")
}

const SIGNATURE_CACHE_CAPACITY: usize = 8;
const SIGNATURE_CACHE_TTL: Duration = Duration::from_hours(6);
const PLAYER_SOURCE_CACHE_CAPACITY: usize = 4;
const PLAYER_SOURCE_TTL: Duration = Duration::from_hours(6);

struct SignatureEntry {
    inserted_at: Instant,
    last_used: Instant,
    value: u64,
}

struct PlayerSourceEntry {
    cell: Arc<AsyncOnceCell<Arc<str>>>,
    inserted_at: Instant,
    last_used: Instant,
}

static SIGNATURE_TIMESTAMPS: LazyLock<Mutex<HashMap<String, SignatureEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PLAYER_SOURCES: LazyLock<Mutex<HashMap<String, PlayerSourceEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sts_from_ytcfg(ytcfg: &Value) -> Option<u64> {
    let value = match ytcfg.get("STS")? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }?;
    (value > 0).then_some(value)
}

fn cached_sts(player_js_url: &str) -> Option<u64> {
    let cache = &*SIGNATURE_TIMESTAMPS;
    let mut cache = cache.lock_unpoisoned();
    let entry = cache.get_mut(player_js_url)?;
    if entry.inserted_at.elapsed() >= SIGNATURE_CACHE_TTL {
        cache.remove(player_js_url);
        return None;
    }
    entry.last_used = Instant::now();
    Some(entry.value)
}

fn remember_sts(player_js_url: &str, value: u64) {
    if value == 0 {
        return;
    }
    let now = Instant::now();
    let mut cache = SIGNATURE_TIMESTAMPS.lock_unpoisoned();
    if cache.len() >= SIGNATURE_CACHE_CAPACITY
        && !cache.contains_key(player_js_url)
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(
        player_js_url.to_string(),
        SignatureEntry {
            inserted_at: now,
            last_used: now,
            value,
        },
    );
}

fn source_entry_is_fresh(entry: &PlayerSourceEntry) -> bool {
    entry.inserted_at.elapsed() < PLAYER_SOURCE_TTL
}

fn evict_player_source(cache: &mut HashMap<String, PlayerSourceEntry>, incoming: &str) {
    if cache.len() < PLAYER_SOURCE_CACHE_CAPACITY || cache.contains_key(incoming) {
        return;
    }
    let oldest = cache
        .iter()
        .filter(|(_, entry)| entry.cell.get().is_some())
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(key, _)| key.clone());
    if let Some(oldest) = oldest {
        cache.remove(&oldest);
    }
}

fn store_player_source(player_js_url: &str, source: Arc<str>) {
    let now = Instant::now();
    let cell = AsyncOnceCell::new();
    let _ = cell.set(Arc::clone(&source));
    {
        let mut cache = PLAYER_SOURCES.lock_unpoisoned();
        evict_player_source(&mut cache, player_js_url);
        cache.insert(
            player_js_url.to_string(),
            PlayerSourceEntry {
                cell: Arc::new(cell),
                inserted_at: now,
                last_used: now,
            },
        );
    }
    if let Some(value) = find_signature_timestamp(&source) {
        remember_sts(player_js_url, value);
    }
}

pub(crate) fn cached_player_source(player_js_url: &str) -> Option<Arc<str>> {
    let cache = &*PLAYER_SOURCES;
    let mut cache = cache.lock_unpoisoned();
    let entry = cache.get_mut(player_js_url)?;
    if !source_entry_is_fresh(entry) {
        cache.remove(player_js_url);
        return None;
    }
    entry.last_used = Instant::now();
    entry.cell.get().cloned()
}

#[cfg(test)]
pub(crate) fn remember_player_source(player_js_url: &str, source: &str) {
    store_player_source(player_js_url, Arc::from(source));
}

/// Load the player JavaScript once per URL, sharing an in-flight fetch between
/// prefetch and challenge solving.
pub(crate) async fn player_source(player_js_url: &str, config: &RequestConfig) -> Result<Arc<str>> {
    player_source_with(player_js_url, || {
        fetch_player_source_uncached(player_js_url, config)
    })
    .await
}

async fn player_source_with<F, Fut>(player_js_url: &str, fetch: F) -> Result<Arc<str>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Arc<str>>>,
{
    if let Some(source) = cached_player_source(player_js_url) {
        tracing::debug!("player JS cache hit: url={player_js_url}");
        return Ok(source);
    }
    let cell = {
        let mut cache = PLAYER_SOURCES.lock_unpoisoned();
        let now = Instant::now();
        if let Some(entry) = cache.get_mut(player_js_url)
            && source_entry_is_fresh(entry)
        {
            entry.last_used = now;
            Arc::clone(&entry.cell)
        } else {
            let cell = Arc::new(AsyncOnceCell::new());
            evict_player_source(&mut cache, player_js_url);
            cache.insert(
                player_js_url.to_string(),
                PlayerSourceEntry {
                    cell: Arc::clone(&cell),
                    inserted_at: now,
                    last_used: now,
                },
            );
            cell
        }
    };
    let source = Arc::clone(cell.get_or_try_init(fetch).await?);
    // Replace the in-flight cell with an initialized entry so the signature
    // timestamp is extracted and later callers can read it synchronously.
    store_player_source(player_js_url, Arc::clone(&source));
    Ok(source)
}

/// Background player-JS fetch that is cancelled when it is not awaited.
pub(crate) struct SourcePrefetch(Option<JoinHandle<Result<Arc<str>>>>);

impl SourcePrefetch {
    fn spawn(url: String, config: RequestConfig) -> Self {
        tracing::debug!("player JS prefetch spawned: url={url}");
        Self(Some(tokio::spawn(async move {
            player_source(&url, &config).await
        })))
    }

    async fn wait(mut self) -> Option<Result<Arc<str>>> {
        let handle = self.0.take()?;
        match handle.await {
            Ok(result) => Some(result),
            Err(error) => Some(Err(YtrustError::coded(
                ErrorCode::Unknown,
                format!("player JS prefetch task failed: {error}"),
            ))),
        }
    }
}

impl Drop for SourcePrefetch {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

async fn fetch_web_embedded_video(
    client: &reqwest::Client,
    video_id: &str,
    config: &RequestConfig,
) -> Result<VideoInfo> {
    let embed_url = format!("https://www.youtube.com/embed/{video_id}?html5=1");
    let html = fetch_text(
        client,
        &embed_url,
        config,
        WEB_EMBEDDED_UA,
        Some("https://www.reddit.com/"),
    )
    .await?;
    let ytcfg = extract_ytcfg(&html).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "YouTube embed page has no ytcfg",
        )
    })?;
    let mut context = ytcfg.get("INNERTUBE_CONTEXT").cloned().ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "YouTube embed ytcfg has no context",
        )
    })?;
    let client_context = context
        .get_mut("client")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidResponse,
                "YouTube embed context has no client object",
            )
        })?;
    client_context.insert("hl".to_string(), json!("en"));
    client_context.insert("timeZone".to_string(), json!("UTC"));
    client_context.insert("utcOffsetMinutes".to_string(), json!(0));
    client_context.insert("originalUrl".to_string(), json!(embed_url));
    match context.get_mut("thirdParty") {
        Some(Value::Object(third_party)) => {
            third_party.insert("embedUrl".to_string(), json!("https://www.reddit.com/"));
        }
        _ => {
            context["thirdParty"] = json!({"embedUrl": "https://www.reddit.com/"});
        }
    }

    let visitor_data = ytcfg
        .pointer("/INNERTUBE_CONTEXT/client/visitorData")
        .and_then(Value::as_str)
        .or_else(|| ytcfg.get("VISITOR_DATA").and_then(Value::as_str));
    let player_js_url = player_js_url_from_cfg(&ytcfg).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "YouTube embed ytcfg has no player JS URL",
        )
    })?;
    remember_player_js_url(&player_js_url);
    let signature_timestamp = match sts_from_ytcfg(&ytcfg).or_else(|| cached_sts(&player_js_url)) {
        Some(value) => value,
        None => signature_timestamp(&player_js_url, config).await?,
    };
    let encrypted_flags = ytcfg
        .pointer("/WEB_PLAYER_CONTEXT_CONFIGS/WEB_PLAYER_CONTEXT_CONFIG_ID_EMBEDDED_PLAYER/encryptedHostFlags")
        .and_then(Value::as_str);
    let client_version = ytcfg
        .pointer("/INNERTUBE_CONTEXT/client/clientVersion")
        .and_then(Value::as_str)
        .unwrap_or(WEB_EMBEDDED_CLIENT_VERSION);
    let mut body = json!({
        "context": context,
        "videoId": video_id,
        "playbackContext": playback_context(Some(signature_timestamp), encrypted_flags),
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let user_session_id = ytcfg.get("USER_SESSION_ID").and_then(Value::as_str);
    let mut response: PlayerResponse = post_innertube(
        client,
        web_embedded_player_request(&body, client_version, visitor_data, user_session_id),
        config,
    )
    .await?;
    if needs_page_reload(&response)
        && let Ok(fresh) = signature_timestamp_fresh(&player_js_url, config).await
    {
        tracing::debug!("retrying embed player request with a fresh signature timestamp");
        body["playbackContext"] = playback_context(Some(fresh), encrypted_flags);
        response = post_innertube(
            client,
            web_embedded_player_request(&body, client_version, visitor_data, user_session_id),
            config,
        )
        .await?;
    }
    let mut info = parse_video_info(
        video_id,
        response,
        ClientKind::WebEmbedded,
        Some(player_js_url),
    )?;
    if info.upload_date.is_empty() {
        info.upload_date = upload_date_from_html(&html).unwrap_or_default();
    }
    Ok(info)
}

fn upload_date_from_html(html: &str) -> Option<String> {
    for marker in ["\"uploadDate\":\"", "\"publishDate\":\""] {
        if let Some(at) = html.find(marker) {
            let rest = &html[at + marker.len()..];
            let value: String = rest.chars().take(10).collect();
            let date = normalize_upload_date(&value);
            if date.len() == 8 {
                return Some(date);
            }
        }
    }
    None
}

fn playback_context(sts: Option<u64>, encrypted_flags: Option<&str>) -> Value {
    let mut playback = json!({"html5Preference": "HTML5_PREF_WANTS"});
    if let Some(sts) = sts {
        playback["signatureTimestamp"] = json!(sts);
    }
    if let Some(flags) = encrypted_flags {
        playback["encryptedHostFlags"] = json!(flags);
    }
    json!({"contentPlaybackContext": playback})
}

fn needs_page_reload(response: &PlayerResponse) -> bool {
    let Some(status) = &response.playability_status else {
        return false;
    };
    if status.status == "OK" {
        return false;
    }
    status.reason.as_deref().is_some_and(|reason| {
        reason
            .to_ascii_lowercase()
            .contains("page needs to be reloaded")
    })
}

async fn signature_timestamp(player_js_url: &str, config: &RequestConfig) -> Result<u64> {
    if let Some(value) = cached_sts(player_js_url) {
        return Ok(value);
    }
    signature_timestamp_fresh(player_js_url, config).await
}

async fn signature_timestamp_fresh(player_js_url: &str, config: &RequestConfig) -> Result<u64> {
    let source = player_source(player_js_url, config).await?;
    let value = find_signature_timestamp(&source).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "player JS has no valid signatureTimestamp",
        )
    })?;
    remember_sts(player_js_url, value);
    Ok(value)
}

fn find_signature_timestamp(source: &str) -> Option<u64> {
    for marker in ["signatureTimestamp", "sts"] {
        let mut from = 0;
        while let Some(offset) = source[from..].find(marker) {
            let at = from + offset;
            let before = source.as_bytes()[..at].last().copied();
            if marker == "sts"
                && before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                from = at + marker.len();
                continue;
            }
            if let Some(value) = parse_numeric_assignment(&source[at + marker.len()..]) {
                return Some(value);
            }
            from = at + marker.len();
        }
    }
    None
}

fn parse_numeric_assignment(rest: &str) -> Option<u64> {
    let rest = rest.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, ':' | '=' | '"' | '\'')
    });
    let digits: String = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .take(6)
        .collect();
    if digits.is_empty() {
        return None;
    }
    let value = digits.parse::<u64>().ok()?;
    (value > 0).then_some(value)
}

fn extract_ytcfg(html: &str) -> Option<Value> {
    let mut rest = html;
    while let Some(at) = rest.find("ytcfg.set") {
        if let Ok(value) = extract_json_assignment(&rest[at..], "ytcfg.set")
            && value.get("INNERTUBE_CONTEXT").is_some()
        {
            return Some(value);
        }
        rest = &rest[at + "ytcfg.set".len()..];
    }
    None
}

pub async fn fetch_playlist(
    playlist_id: &str,
    config: &RequestConfig,
    limit: u64,
) -> Result<PlaylistInfo> {
    let client = build_client(config)?;
    let browse_id = if playlist_id.starts_with("VL") {
        playlist_id.to_string()
    } else {
        format!("VL{playlist_id}")
    };

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut continuation = None;
    let mut title = String::from("Playlist");
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let max_pages = max_playlist_pages(limit);
    let context = web_context();

    for page in 0..max_pages {
        let body = match &continuation {
            Some(token) => json!({
                "context": &context,
                "continuation": token,
            }),
            None => json!({
                "context": &context,
                "browseId": browse_id,
            }),
        };

        let response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "browse",
                body: &body,
                client_name: WEB_CLIENT_ID,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
                send_cookies: true,
            },
            config,
        )
        .await?;

        if page == 0
            && let Some(found_title) = find_playlist_title(&response)
        {
            title = found_title;
        }
        let entries_root =
            find_key_value(&response, "playlistVideoListRenderer").unwrap_or(&response);
        collect_playlist_videos(entries_root, &mut entries, &mut seen, limit_usize);
        tracing::debug!("playlist browse response: entries={}", entries.len());

        if entries.len() >= limit_usize {
            break;
        }
        continuation = find_continuation_token(&response);
        if continuation.is_none() {
            break;
        }
    }

    Ok(PlaylistInfo {
        id: playlist_id.to_string(),
        title,
        entries,
    })
}

/// Inline playlists (radio/mix and `watch?...&list=...`) live in the watch
/// page. yt-dlp paginates them through `next` using the last item's
/// navigation endpoint; we do the same.
pub async fn fetch_inline_playlist(
    video_id: &str,
    playlist_id: &str,
    config: &RequestConfig,
    limit: u64,
) -> Result<PlaylistInfo> {
    let client = build_client(config)?;
    let mut watch_url = Url::parse("https://www.youtube.com/watch")
        .map_err(|error| YtrustError::coded(ErrorCode::InvalidInput, error.to_string()))?;
    {
        let mut query = watch_url.query_pairs_mut();
        query
            .append_pair("v", video_id)
            .append_pair("list", playlist_id);
        if playlist_id.starts_with("RD") {
            query.append_pair("start_radio", "1");
        }
        query.append_pair("hl", "en").append_pair("gl", "US");
    }
    let watch_url = watch_url.to_string();
    let html = fetch_text(&client, &watch_url, config, WEB_UA, None).await?;
    let data = extract_json_assignment(&html, "ytInitialData")?;
    let mut data = data;
    let mut playlist = data
        .pointer_mut("/contents/twoColumnWatchNextResults/playlist/playlist")
        .map(Value::take)
        .ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidResponse,
                "YouTube returned no inline playlist",
            )
        })?;
    let title = playlist
        .get("title")
        .and_then(value_text)
        .or_else(|| playlist.get("titleText").and_then(value_text))
        .unwrap_or_else(|| "Playlist".to_string());

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let max_pages = max_playlist_pages(limit);
    let context = web_context();
    for _ in 0..max_pages {
        let (page_entries, endpoint) = extract_inline_entries(&playlist);
        if page_entries.is_empty() {
            break;
        }
        for mut entry in page_entries {
            if !seen.insert(entry.id.clone()) {
                continue;
            }
            entry.index = entries.len() + 1;
            entries.push(entry);
            if entries.len() >= limit_usize {
                break;
            }
        }
        if entries.len() >= limit_usize {
            break;
        }
        let Some(endpoint) = endpoint else { break };
        let body = json!({
            "context": &context,
            "playlistId": playlist_id,
            "videoId": endpoint.video_id,
            "index": endpoint.index.unwrap_or(entries.len() as u64),
            "params": endpoint.params.unwrap_or_else(|| "OAE%3D".to_string()),
        });
        let mut response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "next",
                body: &body,
                client_name: WEB_CLIENT_ID,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
                send_cookies: true,
            },
            config,
        )
        .await?;
        let Some(next) = response
            .pointer_mut("/contents/twoColumnWatchNextResults/playlist/playlist")
            .map(Value::take)
        else {
            break;
        };
        playlist = next;
    }

    Ok(PlaylistInfo {
        id: playlist_id.to_string(),
        title,
        entries,
    })
}

fn extract_json_assignment(html: &str, marker: &str) -> Result<Value> {
    let marker_at = html.find(marker).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            format!("{marker} not found in YouTube page"),
        )
    })?;
    let start = html[marker_at..]
        .find('{')
        .map(|offset| marker_at + offset)
        .ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidResponse,
                format!("{marker} has no JSON object"),
            )
        })?;
    let bytes = html.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    YtrustError::coded(
                        ErrorCode::InvalidResponse,
                        format!("unbalanced JSON object after {marker}"),
                    )
                })?;
                if depth == 0 {
                    let end = start + offset + 1;
                    return Ok(serde_json::from_str(&html[start..end])?);
                }
            }
            _ => {}
        }
    }
    Err(YtrustError::coded(
        ErrorCode::InvalidResponse,
        format!("unterminated JSON object after {marker}"),
    ))
}

struct InlineEndpoint {
    video_id: String,
    index: Option<u64>,
    params: Option<String>,
}

fn extract_inline_entries(playlist: &Value) -> (Vec<PlaylistEntry>, Option<InlineEndpoint>) {
    let mut entries = Vec::new();
    let mut endpoint = None;
    let Some(contents) = playlist.get("contents").and_then(Value::as_array) else {
        return (entries, endpoint);
    };
    for content in contents {
        let Some(renderer) = content
            .get("playlistPanelVideoRenderer")
            .or_else(|| content.get("playlistVideoRenderer"))
        else {
            continue;
        };
        let Some(id) = renderer.get("videoId").and_then(Value::as_str) else {
            continue;
        };
        let title = renderer
            .get("title")
            .and_then(text_of)
            .unwrap_or_else(|| id.to_string());
        let duration = renderer
            .get("lengthText")
            .and_then(text_of)
            .and_then(|text| parse_time_text(&text))
            .or_else(|| {
                renderer
                    .get("lengthSeconds")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse().ok())
            });
        entries.push(PlaylistEntry {
            index: 0,
            id: id.to_string(),
            title,
            duration,
            thumbnails: thumbnails_from(renderer),
        });
        if let Some(watch) = renderer.pointer("/navigationEndpoint/watchEndpoint") {
            endpoint = Some(InlineEndpoint {
                video_id: watch
                    .get("videoId")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                index: watch.get("index").and_then(Value::as_u64),
                params: watch
                    .get("params")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    (entries, endpoint)
}

fn value_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| text_of(value))
}

fn text_of(value: &Value) -> Option<String> {
    if let Some(text) = value.get("simpleText").and_then(Value::as_str)
        && !text.is_empty()
    {
        return Some(text.to_string());
    }
    let text = value
        .get("runs")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn thumbnails_from(renderer: &Value) -> Vec<Thumbnail> {
    renderer
        .pointer("/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .map(|items| parse_thumbnail_array(items))
        .unwrap_or_default()
}

fn parse_time_text(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut parts = 0usize;
    for part in text.trim().split(':') {
        parts += 1;
        if parts > 3 {
            return None;
        }
        total = total
            .checked_mul(60)?
            .checked_add(part.trim().parse::<u64>().ok()?)?;
    }
    Some(total)
}

pub async fn fetch_search(query: &str, limit: u64, config: &RequestConfig) -> Result<PlaylistInfo> {
    let client = build_client(config)?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut continuation = None;
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let max_pages = max_playlist_pages(limit);
    let context = web_context();

    for _ in 0..max_pages {
        let body = match &continuation {
            Some(token) => json!({
                "context": &context,
                "continuation": token,
            }),
            None => json!({
                "context": &context,
                "query": query,
            }),
        };
        let response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "search",
                body: &body,
                client_name: WEB_CLIENT_ID,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
                send_cookies: true,
            },
            config,
        )
        .await?;
        let entries_root = find_key_value(&response, "searchResultsRenderer").unwrap_or(&response);
        collect_playlist_videos(entries_root, &mut entries, &mut seen, limit_usize);
        if entries.len() >= limit_usize {
            break;
        }
        continuation = find_continuation_token(&response);
        if continuation.is_none() {
            break;
        }
    }

    Ok(PlaylistInfo {
        id: format!("search:{query}"),
        title: format!("Search: {query}"),
        entries,
    })
}

fn max_playlist_pages(limit: u64) -> usize {
    limit.div_ceil(50).clamp(40, 1_000) as usize
}

pub fn max_playlist_item(expression: &str) -> Result<Option<usize>> {
    let mut max = None;
    for part in expression
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start_raw = start.trim();
            let end_raw = end.trim();
            let start = if start_raw.is_empty() {
                1
            } else {
                start_raw
                    .parse::<usize>()
                    .map_err(|_| invalid_playlist_part(part))?
            };
            if start == 0 {
                return Err(invalid_playlist_part(part));
            }
            if end_raw.is_empty() {
                return Ok(None);
            }
            let end = end_raw
                .parse::<usize>()
                .map_err(|_| invalid_playlist_part(part))?;
            if end == 0 || start > end {
                return Err(invalid_playlist_part(part));
            }
            max = Some(max.map_or(end, |current: usize| current.max(end)));
        } else {
            let index = part.parse::<usize>().map_err(|_| {
                YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("invalid playlist item: {part}"),
                )
            })?;
            if index == 0 {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    "playlist items start at 1",
                ));
            }
            max = Some(max.map_or(index, |current: usize| current.max(index)));
        }
    }
    Ok(max)
}

fn invalid_playlist_part(part: &str) -> YtrustError {
    YtrustError::coded(
        ErrorCode::InvalidInput,
        format!("invalid playlist range: {part}"),
    )
}

pub fn filter_playlist_entries(playlist: &mut PlaylistInfo, selection: &str) -> Result<()> {
    let mask = playlist_item_mask(selection, playlist.entries.len())?;
    if !mask.iter().any(|selected| *selected) {
        playlist.entries.clear();
        return Ok(());
    }
    playlist
        .entries
        .retain(|entry| mask.get(entry.index).copied().unwrap_or(false));
    Ok(())
}

fn playlist_item_mask(expression: &str, total: usize) -> Result<Vec<bool>> {
    let mut selected = vec![false; total.saturating_add(1)];

    for part in expression
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start_raw = start.trim();
            let end_raw = end.trim();
            let start = if start_raw.is_empty() {
                1
            } else {
                start_raw
                    .parse::<usize>()
                    .map_err(|_| invalid_playlist_part(part))?
            };
            let open_ended = end_raw.is_empty();
            let end = if open_ended {
                total
            } else {
                end_raw
                    .parse::<usize>()
                    .map_err(|_| invalid_playlist_part(part))?
            };
            if start == 0 || (!open_ended && (end == 0 || start > end)) {
                return Err(invalid_playlist_part(part));
            }
            let upper = end.min(total);
            if start <= upper {
                for flag in &mut selected[start..=upper] {
                    *flag = true;
                }
            }
        } else {
            let index = part.parse::<usize>().map_err(|_| {
                YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("invalid playlist item: {part}"),
                )
            })?;
            if index == 0 {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    "playlist items start at 1",
                ));
            }
            if let Some(flag) = selected.get_mut(index) {
                *flag = true;
            }
        }
    }

    Ok(selected)
}

/// Client order follows yt-dlp's current defaults: the cookie-capable,
/// PO-token-free TV client first, with the JSLess VisionOS client as a
/// fallback. VisionOS is skipped for authenticated sessions because YouTube
/// does not support cookies for it.
fn client_order(config: &RequestConfig) -> &'static [ClientKind] {
    const AUTHENTICATED: &[ClientKind] = &[ClientKind::TvSamsung];
    const ANONYMOUS: &[ClientKind] = &[ClientKind::TvSamsung, ClientKind::VisionOs];
    const JS_LESS: &[ClientKind] = &[ClientKind::VisionOs, ClientKind::TvSamsung];

    if config.prefer_jsless {
        JS_LESS
    } else if config.is_authenticated() {
        AUTHENTICATED
    } else {
        ANONYMOUS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientKind {
    WebEmbedded,
    VisionOs,
    TvSamsung,
}

impl ClientKind {
    fn context(self) -> (Value, &'static str, &'static str) {
        match self {
            Self::WebEmbedded => (
                json!({
                    "clientName": "WEB_EMBEDDED_PLAYER",
                    "clientVersion": WEB_EMBEDDED_CLIENT_VERSION,
                    "userAgent": WEB_EMBEDDED_UA,
                    "hl": "en",
                    "gl": "US",
                }),
                WEB_EMBEDDED_CLIENT_ID,
                WEB_EMBEDDED_CLIENT_VERSION,
            ),
            Self::VisionOs => (
                json!({
                    "clientName": "VISIONOS",
                    "clientVersion": VISIONOS_CLIENT_VERSION,
                    "deviceMake": "Apple",
                    "deviceModel": "RealityDevice17,1",
                    "userAgent": VISIONOS_UA,
                    "osName": "visionOS",
                    "osVersion": "26.5.23O471",
                    "hl": "en",
                    "gl": "US",
                }),
                VISIONOS_CLIENT_ID,
                VISIONOS_CLIENT_VERSION,
            ),
            Self::TvSamsung => (
                json!({
                    "clientName": "TVHTML5",
                    "clientVersion": TV_SAMSUNG_CLIENT_VERSION,
                    "deviceMake": "Samsung",
                    "deviceModel": "UKS9800",
                    "userAgent": TV_SAMSUNG_UA,
                    "osName": "Tizen",
                    "osVersion": "2.4.0",
                    "hl": "en",
                    "gl": "US",
                }),
                TV_SAMSUNG_CLIENT_ID,
                TV_SAMSUNG_CLIENT_VERSION,
            ),
        }
    }

    fn user_agent(self) -> &'static str {
        match self {
            Self::VisionOs => VISIONOS_UA,
            Self::WebEmbedded => WEB_EMBEDDED_UA,
            Self::TvSamsung => TV_SAMSUNG_UA,
        }
    }

    fn supports_cookies(self) -> bool {
        !matches!(self, Self::VisionOs)
    }
}

fn web_context() -> Value {
    json!({
        "client": {
            "clientName": "WEB",
            "clientVersion": WEB_CLIENT_VERSION,
            "userAgent": WEB_UA,
            "hl": "en",
            "gl": "US",
        }
    })
}

struct PlayerFetch {
    response: PlayerResponse,
    watch_metadata: Option<PlayerResponse>,
    player_js_url: Option<String>,
    source_prefetch: Option<SourcePrefetch>,
}

fn player_api_request<'a>(body: &'a Value, client_kind: ClientKind) -> InnertubeRequest<'a> {
    let (_, client_name, client_version) = client_kind.context();
    InnertubeRequest {
        endpoint: "player",
        body,
        client_name,
        client_version,
        user_agent: client_kind.user_agent(),
        visitor_data: None,
        user_session_id: None,
        browser: false,
        send_cookies: client_kind.supports_cookies(),
    }
}

fn web_embedded_player_request<'a>(
    body: &'a Value,
    client_version: &'a str,
    visitor_data: Option<&'a str>,
    user_session_id: Option<&'a str>,
) -> InnertubeRequest<'a> {
    InnertubeRequest {
        endpoint: "player",
        body,
        client_name: WEB_EMBEDDED_CLIENT_ID,
        client_version,
        user_agent: WEB_EMBEDDED_UA,
        visitor_data,
        user_session_id,
        browser: true,
        send_cookies: true,
    }
}

async fn fetch_player_response(
    client: &reqwest::Client,
    video_id: &str,
    client_kind: ClientKind,
    config: &RequestConfig,
) -> Result<PlayerFetch> {
    let (context, _, _) = client_kind.context();
    let mut body = json!({
        "context": {"client": context},
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let mut watch_metadata = None;
    let mut player_js_url = None;
    let mut source_prefetch = None;
    if client_kind == ClientKind::TvSamsung {
        // TVHTML5 clients reject requests without a playback context and a
        // current signature timestamp. yt-dlp sends the same fields. The watch
        // page also supplies the metadata TVHTML5 omits from its response.
        let watch_html = fetch_watch_html(client, video_id, config).await;
        let ytcfg = watch_html.as_deref().and_then(extract_ytcfg);
        if let Some(html) = &watch_html
            && let Ok(value) = extract_json_assignment(html, "ytInitialPlayerResponse")
        {
            watch_metadata = serde_json::from_value(value).ok();
        }
        player_js_url = ytcfg
            .as_ref()
            .and_then(player_js_url_from_cfg)
            .or_else(cached_player_js_url);
        if let Some(player_url) = player_js_url.as_deref() {
            remember_player_js_url(player_url);
            if let Some(sts) = ytcfg
                .as_ref()
                .and_then(sts_from_ytcfg)
                .or_else(|| cached_sts(player_url))
            {
                body["playbackContext"] = playback_context(Some(sts), None);
            } else {
                tracing::debug!("TvSamsung signature timestamp unavailable before player request");
            }
            if config.prefetch_player_source
                && config.resolve_player_js
                && cached_player_source(player_url).is_none()
            {
                source_prefetch = Some(SourcePrefetch::spawn(
                    player_url.to_string(),
                    config.clone(),
                ));
            }
        } else {
            tracing::debug!("TvSamsung player JS URL unavailable");
        }
    }

    let mut response =
        post_innertube(client, player_api_request(&body, client_kind), config).await?;
    if client_kind == ClientKind::TvSamsung
        && needs_page_reload(&response)
        && let Some(player_url) = player_js_url.as_deref()
    {
        match signature_timestamp_fresh(player_url, config).await {
            Ok(fresh) => {
                tracing::debug!(
                    "retrying TvSamsung player request with a fresh signature timestamp"
                );
                body["playbackContext"] = playback_context(Some(fresh), None);
                response =
                    post_innertube(client, player_api_request(&body, client_kind), config).await?;
            }
            Err(error) => tracing::debug!("TvSamsung reload recovery failed: {error}"),
        }
    }
    Ok(PlayerFetch {
        response,
        watch_metadata,
        player_js_url,
        source_prefetch,
    })
}

#[derive(Debug, Deserialize)]
struct PlayerResponse {
    #[serde(rename = "playabilityStatus")]
    playability_status: Option<PlayabilityStatus>,
    #[serde(rename = "streamingData")]
    streaming_data: Option<StreamingData>,
    #[serde(rename = "videoDetails")]
    video_details: Option<VideoDetails>,
    #[serde(rename = "microformat")]
    microformat: Option<Microformat>,
    captions: Option<Captions>,
}

#[derive(Debug, Deserialize)]
struct PlayabilityStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct StreamingData {
    #[serde(default, deserialize_with = "deserialize_formats")]
    formats: Vec<RawFormat>,
    #[serde(
        rename = "adaptiveFormats",
        default,
        deserialize_with = "deserialize_formats"
    )]
    adaptive_formats: Vec<RawFormat>,
    #[serde(rename = "hlsManifestUrl")]
    hls_manifest_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    #[serde(default)]
    itag: u64,
    url: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    bitrate: Option<u64>,
    #[serde(rename = "averageBitrate")]
    average_bitrate: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    fps: Option<u64>,
    #[serde(
        rename = "contentLength",
        default,
        deserialize_with = "deserialize_u64_opt"
    )]
    content_length: Option<u64>,
    #[serde(rename = "qualityLabel")]
    quality_label: Option<String>,
    #[serde(rename = "signatureCipher")]
    signature_cipher: Option<String>,
    cipher: Option<String>,
    #[serde(rename = "audioTrack")]
    audio_track: Option<RawAudioTrack>,
}

#[derive(Debug, Deserialize)]
struct RawAudioTrack {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    id: Option<String>,
    #[serde(rename = "audioIsDefault")]
    audio_is_default: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct VideoDetails {
    #[serde(rename = "videoId", default)]
    video_id: String,
    #[serde(default)]
    title: String,
    author: Option<String>,
    #[serde(rename = "ownerChannelName")]
    owner_channel_name: Option<String>,
    #[serde(rename = "shortDescription")]
    short_description: Option<String>,
    #[serde(
        rename = "lengthSeconds",
        default,
        deserialize_with = "deserialize_u64_opt"
    )]
    length_seconds: Option<u64>,
    #[serde(
        rename = "viewCount",
        default,
        deserialize_with = "deserialize_u64_opt"
    )]
    view_count: Option<u64>,
    #[serde(rename = "isLiveContent")]
    is_live_content: Option<bool>,
    #[serde(rename = "channelId")]
    channel_id: Option<String>,
    thumbnail: Option<ThumbnailList>,
}

#[derive(Debug, Deserialize)]
struct ThumbnailList {
    #[serde(default, deserialize_with = "deserialize_thumbnails")]
    thumbnails: Vec<RawThumbnail>,
}

#[derive(Debug, Deserialize)]
struct RawThumbnail {
    url: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Microformat {
    #[serde(rename = "playerMicroformatRenderer")]
    player_microformat_renderer: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct Captions {
    #[serde(rename = "playerCaptionsTracklistRenderer")]
    player_captions_tracklist_renderer: Option<CaptionTracklist>,
}

#[derive(Debug, Deserialize)]
struct CaptionTracklist {
    #[serde(
        rename = "captionTracks",
        default,
        deserialize_with = "deserialize_caption_tracks"
    )]
    caption_tracks: Vec<RawCaptionTrack>,
}

#[derive(Debug, Deserialize)]
struct RawCaptionTrack {
    #[serde(rename = "baseUrl", default)]
    base_url: String,
    #[serde(rename = "languageCode", default)]
    language_code: String,
    name: Option<Value>,
    kind: Option<String>,
}

fn deserialize_formats<'de, D>(deserializer: D) -> std::result::Result<Vec<RawFormat>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

fn deserialize_caption_tracks<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<RawCaptionTrack>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

fn deserialize_thumbnails<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<RawThumbnail>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

fn deserialize_u64_opt<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }))
}

fn playability_code(reason: &str) -> ErrorCode {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("not a bot") {
        ErrorCode::BotCheck
    } else if reason.contains("age-restricted")
        || reason.contains("age restricted")
        || reason.contains("confirm your age")
    {
        ErrorCode::AgeRestricted
    } else if reason.contains("private") {
        ErrorCode::Private
    } else if reason.contains("unavailable")
        || reason.contains("does not exist")
        || reason.contains("removed")
        || reason.contains("terminated")
    {
        ErrorCode::Unavailable
    } else {
        ErrorCode::Unknown
    }
}

fn parse_video_info(
    requested_id: &str,
    response: PlayerResponse,
    client_kind: ClientKind,
    player_js_url: Option<String>,
) -> Result<VideoInfo> {
    if let Some(status) = &response.playability_status {
        tracing::debug!(
            "player response status: {} ({:?})",
            status.status,
            status.reason
        );
        if !status.status.is_empty() && status.status != "OK" {
            let reason = status
                .reason
                .clone()
                .unwrap_or_else(|| status.status.clone());
            return Err(YtrustError::coded(
                playability_code(&reason),
                format!("YouTube: {reason}"),
            ));
        }
    }

    let captions = response.captions;
    let microformat = response.microformat;
    let default_audio_language = microformat
        .as_ref()
        .and_then(|microformat| microformat.player_microformat_renderer.as_ref())
        .and_then(|renderer| renderer.get("defaultAudioLanguage"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let details = response.video_details.ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "YouTube response has no video details",
        )
    })?;
    let streaming = response.streaming_data.ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "YouTube response has no streaming data",
        )
    })?;
    let is_live = details.is_live_content.unwrap_or(false);

    let hls_manifest_url = streaming.hls_manifest_url;
    let format_count = streaming.formats.len() + streaming.adaptive_formats.len();
    tracing::debug!("player response has {format_count} raw formats");

    let mut formats = Vec::with_capacity(format_count + usize::from(hls_manifest_url.is_some()));
    for raw in streaming
        .formats
        .into_iter()
        .chain(streaming.adaptive_formats)
    {
        if let Some(format) = parse_format(raw, client_kind, is_live) {
            formats.push(format);
        }
    }
    tracing::debug!("parsed {} formats", formats.len());

    if let Some(url) = &hls_manifest_url {
        formats.push(Format {
            itag: 0,
            url: url.clone(),
            signature_cipher: None,
            cipher: None,
            container: "mp4".to_string(),
            video_codec: None,
            audio_codec: None,
            has_video: true,
            has_audio: true,
            width: None,
            height: None,
            fps: None,
            bitrate: 0,
            audio_bitrate: None,
            content_length: None,
            quality_label: Some("HLS".to_string()),
            is_hls: true,
            is_live,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: true,
            language: None,
            needs_challenge: false,
            user_agent: client_kind.user_agent(),
        });
    }

    let upload_date = microformat
        .and_then(|m| m.player_microformat_renderer)
        .and_then(|value| {
            value
                .get("uploadDate")
                .or_else(|| value.get("publishDate"))
                .and_then(Value::as_str)
                .map(normalize_upload_date)
        })
        .unwrap_or_default();

    let duration_secs = details.length_seconds.filter(|value| *value > 0);

    let thumbnails = details
        .thumbnail
        .map(|list| list.thumbnails)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            Some(Thumbnail {
                url: item.url?,
                width: item.width,
                height: item.height,
            })
        })
        .collect();

    let (subtitles, automatic_captions) = parse_captions(captions);
    let default_language = default_audio_language
        .or_else(|| {
            subtitles
                .first()
                .map(|track| clean_caption_language(&track.language))
        })
        .or_else(|| {
            automatic_captions
                .first()
                .map(|track| clean_caption_language(&track.language))
        });
    for format in &mut formats {
        if format.has_audio && format.language.is_none() {
            format.language = default_language.clone();
        }
    }
    let id = if details.video_id.is_empty() {
        requested_id.to_string()
    } else {
        details.video_id
    };

    Ok(VideoInfo {
        webpage_url: format!("https://www.youtube.com/watch?v={id}"),
        id,
        title: details.title,
        uploader: details.author.unwrap_or_default(),
        channel: details.owner_channel_name.unwrap_or_default(),
        channel_id: details.channel_id.unwrap_or_default(),
        description: details.short_description.unwrap_or_default(),
        duration_secs,
        view_count: details.view_count,
        upload_date,
        is_live,
        thumbnails,
        subtitles,
        automatic_captions,
        formats,
        hls_manifest_url,
        player_js_url,
    })
}

fn parse_format(raw: RawFormat, client_kind: ClientKind, is_live: bool) -> Option<Format> {
    let url = raw.url.unwrap_or_default();
    if raw.itag == 0 || (url.is_empty() && raw.signature_cipher.is_none() && raw.cipher.is_none()) {
        return None;
    }

    let (container, video_codec, audio_codec, has_video, has_audio) =
        parse_mime_and_codecs(&raw.mime_type);
    let language = raw
        .audio_track
        .as_ref()
        .and_then(|track| track.id.as_deref())
        .map(|id| id.split('.').next().unwrap_or(id))
        .filter(|language| !language.is_empty())
        .map(str::to_string);
    let audio_bitrate = (has_audio && !has_video)
        .then(|| raw.average_bitrate.or(raw.bitrate))
        .flatten()
        .filter(|bitrate| *bitrate > 0);

    let mut format = Format {
        itag: raw.itag,
        url,
        signature_cipher: raw.signature_cipher,
        cipher: raw.cipher,
        container,
        video_codec,
        audio_codec,
        has_video,
        has_audio,
        width: raw.width,
        height: raw.height,
        fps: raw.fps,
        bitrate: raw.average_bitrate.or(raw.bitrate).unwrap_or(0),
        audio_bitrate,
        content_length: raw.content_length,
        quality_label: raw.quality_label,
        is_hls: false,
        is_live,
        audio_track_name: raw
            .audio_track
            .as_ref()
            .and_then(|track| track.display_name.clone()),
        audio_track_id: raw.audio_track.as_ref().and_then(|track| track.id.clone()),
        audio_is_default: raw
            .audio_track
            .as_ref()
            .and_then(|track| track.audio_is_default)
            .unwrap_or(false),
        language,
        needs_challenge: false,
        user_agent: client_kind.user_agent(),
    };
    format.needs_challenge = format.needs_challenge();
    Some(format)
}

fn clean_caption_language(language: &str) -> String {
    language
        .trim_start_matches("a-")
        .trim_end_matches("-orig")
        .to_string()
}

fn normalize_upload_date(value: &str) -> String {
    let value = value.trim();
    let date = value.get(..10).filter(|date| {
        date.as_bytes().get(4) == Some(&b'-') && date.as_bytes().get(7) == Some(&b'-')
    });
    let digits: String = date
        .unwrap_or(value)
        .chars()
        .filter(char::is_ascii_digit)
        .take(8)
        .collect();
    if digits.len() == 8 {
        digits
    } else {
        String::new()
    }
}

fn parse_captions(captions: Option<Captions>) -> (Vec<SubtitleTrack>, Vec<SubtitleTrack>) {
    let mut subtitles = Vec::new();
    let mut automatic = Vec::new();

    let tracks = captions
        .and_then(|captions| captions.player_captions_tracklist_renderer)
        .map(|tracklist| tracklist.caption_tracks)
        .unwrap_or_default();

    for track in tracks {
        if track.base_url.is_empty() || track.language_code.is_empty() {
            continue;
        }
        let is_auto = track.kind.as_deref() == Some("asr");
        let name = track
            .name
            .as_ref()
            .and_then(|value| {
                value
                    .get("simpleText")
                    .or_else(|| value.pointer("/runs/0/text"))
                    .and_then(Value::as_str)
            })
            .unwrap_or(&track.language_code)
            .to_string();
        let subtitle = SubtitleTrack {
            language: track.language_code,
            name,
            url: subtitle_url(track.base_url),
            is_auto,
        };
        if is_auto {
            automatic.push(subtitle);
        } else {
            subtitles.push(subtitle);
        }
    }

    (subtitles, automatic)
}

fn subtitle_url(base_url: String) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!("{base_url}{separator}fmt=vtt")
}

fn parse_mime_and_codecs(mime_type: &str) -> (String, Option<String>, Option<String>, bool, bool) {
    let mut parameters = mime_type.split(';');
    let mime = parameters
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let (kind, subtype) = mime.split_once('/').unwrap_or((mime.as_str(), ""));
    let container = match subtype {
        "mp4" => "mp4",
        "webm" => "webm",
        "3gpp" => "3gp",
        "x-matroska" => "mkv",
        other => other,
    }
    .to_string();

    let mut codecs = Vec::new();
    for parameter in parameters {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("codecs") {
            continue;
        }
        for codec in value.trim().trim_matches('"').split(',') {
            let codec = codec.trim();
            if !codec.is_empty() {
                codecs.push(codec.to_string());
            }
        }
    }

    let has_video = kind == "video";
    let video_codec = if has_video {
        codecs
            .iter()
            .find(|codec| is_video_codec(codec))
            .cloned()
            .or_else(|| codecs.first().cloned())
    } else {
        None
    };
    let audio_codec = if kind == "audio" {
        codecs
            .iter()
            .find(|codec| is_audio_codec(codec))
            .cloned()
            .or_else(|| codecs.first().cloned())
    } else {
        codecs.iter().find(|codec| is_audio_codec(codec)).cloned()
    };
    let has_audio = kind == "audio" || (has_video && audio_codec.is_some());

    (container, video_codec, audio_codec, has_video, has_audio)
}

fn is_video_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    codec.starts_with("avc")
        || codec.starts_with("hev")
        || codec.starts_with("hvc")
        || codec.starts_with("vp")
        || codec.starts_with("av01")
        || codec.starts_with("dvh")
        || codec.starts_with("theora")
}

fn is_audio_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    codec.starts_with("mp4a")
        || codec.starts_with("opus")
        || codec.starts_with("vorbis")
        || codec.starts_with("ac-3")
        || codec.starts_with("ec-3")
        || codec.starts_with("flac")
        || codec.starts_with("mp3")
}

fn find_playlist_title(value: &Value) -> Option<String> {
    find_key_value(value, "playlistMetadataRenderer")
        .and_then(|metadata| metadata.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            find_key_value(value, "playlistHeaderRenderer")
                .and_then(|header| header.get("title"))
                .and_then(text_of)
        })
        .or_else(|| {
            value
                .pointer("/sidebar/playlistSidebarRenderer/items/0/playlistSidebarPrimaryInfoRenderer/title")
                .and_then(text_of)
        })
        .filter(|title| !title.is_empty())
}

fn find_key_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|child| find_key_value(child, key))
        }
        Value::Array(items) => items.iter().find_map(|child| find_key_value(child, key)),
        _ => None,
    }
}

fn collect_playlist_videos(
    value: &Value,
    entries: &mut Vec<PlaylistEntry>,
    seen: &mut HashSet<String>,
    limit: usize,
) {
    if entries.len() >= limit {
        return;
    }

    match value {
        Value::Object(map) => {
            if let Some(renderer) = map
                .get("videoRenderer")
                .or_else(|| map.get("playlistVideoRenderer"))
                .or_else(|| map.get("reelItemRenderer"))
                .or_else(|| map.get("gridVideoRenderer"))
                .or_else(|| map.get("compactVideoRenderer"))
                && let Some((id, title, duration, thumbnails)) = standard_renderer_entry(renderer)
            {
                push_playlist_entry(entries, seen, limit, &id, title, duration, thumbnails);
            }
            if let Some(renderer) = map.get("lockupViewModel")
                && renderer.get("contentType").and_then(Value::as_str)
                    == Some("LOCKUP_CONTENT_TYPE_VIDEO")
                && let Some(id) = renderer.get("contentId").and_then(Value::as_str)
            {
                let title = renderer
                    .pointer("/metadata/lockupMetadataViewModel/title/content")
                    .or_else(|| {
                        renderer.pointer("/metadata/lockupMetadataViewModel/title/runs/0/text")
                    })
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .unwrap_or(id)
                    .to_string();
                let duration = find_key_value(renderer, "thumbnailBadgeViewModel")
                    .and_then(|badge| badge.get("text"))
                    .and_then(Value::as_str)
                    .and_then(parse_duration_text);
                let thumbnails = renderer
                    .pointer("/contentImage/thumbnailViewModel/image/sources")
                    .and_then(Value::as_array)
                    .map(|items| parse_thumbnail_array(items))
                    .unwrap_or_default();
                push_playlist_entry(entries, seen, limit, id, title, duration, thumbnails);
            }
            for child in map.values() {
                collect_playlist_videos(child, entries, seen, limit);
                if entries.len() >= limit {
                    break;
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_playlist_videos(child, entries, seen, limit);
                if entries.len() >= limit {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn standard_renderer_entry(
    renderer: &Value,
) -> Option<(String, String, Option<u64>, Vec<Thumbnail>)> {
    let id = renderer.get("videoId").and_then(Value::as_str)?;
    let title = text_of(renderer.get("title").unwrap_or(&Value::Null))
        .or_else(|| text_of(renderer.get("headline").unwrap_or(&Value::Null)))
        .unwrap_or_else(|| id.to_string());
    let duration = renderer
        .get("lengthSeconds")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            renderer
                .get("lengthText")
                .and_then(text_of)
                .and_then(|text| parse_duration_text(&text))
        });
    Some((id.to_string(), title, duration, thumbnails_from(renderer)))
}

fn push_playlist_entry(
    entries: &mut Vec<PlaylistEntry>,
    seen: &mut HashSet<String>,
    limit: usize,
    id: &str,
    title: String,
    duration: Option<u64>,
    thumbnails: Vec<Thumbnail>,
) {
    if entries.len() < limit && !id.is_empty() && seen.insert(id.to_string()) {
        entries.push(PlaylistEntry {
            index: entries.len() + 1,
            id: id.to_string(),
            title,
            duration,
            thumbnails,
        });
    }
}

fn parse_thumbnail_array(items: &[Value]) -> Vec<Thumbnail> {
    items
        .iter()
        .filter_map(|item| {
            Some(Thumbnail {
                url: item.get("url")?.as_str()?.to_string(),
                width: item.get("width").and_then(Value::as_u64),
                height: item.get("height").and_then(Value::as_u64),
            })
        })
        .collect()
}

fn parse_duration_text(value: &str) -> Option<u64> {
    parse_time_text(value).filter(|seconds| *seconds > 0)
}

fn find_continuation_token(value: &Value) -> Option<String> {
    find_continuation_token_in(value, false)
}

fn find_continuation_token_in(value: &Value, active: bool) -> Option<String> {
    match value {
        Value::Object(map) => {
            let active = active
                || map.keys().any(|key| {
                    matches!(
                        key.as_str(),
                        "playlistVideoListRenderer"
                            | "playlistPanelRenderer"
                            | "searchResultsRenderer"
                            | "gridRenderer"
                            | "reelShelfRenderer"
                    )
                });
            if active
                && let Some(token) = map
                    .get("continuationCommand")
                    .and_then(|command| command.get("token"))
                    .and_then(Value::as_str)
            {
                return Some(token.to_string());
            }
            for child in map.values() {
                if let Some(token) = find_continuation_token_in(child, active) {
                    return Some(token);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_continuation_token_in(child, active)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn parse_item_ranges(expression: &str, total: usize) -> Result<Vec<usize>> {
        let mask = playlist_item_mask(expression, total)?;
        Ok(mask
            .into_iter()
            .enumerate()
            .skip(1)
            .filter_map(|(index, selected)| selected.then_some(index))
            .collect())
    }

    #[test]
    fn classifies_watch_url() {
        let target = classify("https://www.youtube.com/watch?v=dQw4w9WgXcQ", false).unwrap();
        assert!(matches!(target, Target::Video { id, .. } if id == "dQw4w9WgXcQ"));
    }

    #[test]
    fn classifies_short_url() {
        let target = classify("https://youtu.be/dQw4w9WgXcQ", false).unwrap();
        assert!(matches!(target, Target::Video { id, .. } if id == "dQw4w9WgXcQ"));
    }

    #[test]
    fn classifies_playlist_url() {
        let target = classify("https://www.youtube.com/playlist?list=PL1234567890", false).unwrap();
        assert!(matches!(target, Target::Playlist { id, .. } if id == "PL1234567890"));
    }

    #[test]
    fn no_playlist_prefers_video() {
        let target = classify(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1234567890",
            true,
        )
        .unwrap();
        assert!(matches!(target, Target::Video { id, .. } if id == "dQw4w9WgXcQ"));
    }

    #[test]
    fn classifies_short_url_with_playlist_as_inline() {
        let target = classify("https://youtu.be/dQw4w9WgXcQ?list=RDdQw4w9WgXcQ", false).unwrap();
        assert!(matches!(
            target,
            Target::Playlist { id, video_id }
                if id == "RDdQw4w9WgXcQ" && video_id.as_deref() == Some("dQw4w9WgXcQ")
        ));
    }

    #[test]
    fn classifies_watch_with_playlist_as_inline() {
        let target = classify(
            "https://www.youtube.com/watch?v=nJqkWDFTK-Y&list=RDnJqkWDFTK-Y&start_radio=1",
            false,
        )
        .unwrap();
        assert!(matches!(
            target,
            Target::Playlist { id, video_id }
                if id == "RDnJqkWDFTK-Y" && video_id.as_deref() == Some("nJqkWDFTK-Y")
        ));
    }

    #[test]
    fn extracts_inline_playlist_entries() {
        let playlist = json!({
            "title": "Mix",
            "contents": [
                {"playlistPanelVideoRenderer": {
                    "videoId": "aaaaaaaaaaa",
                    "title": {"simpleText": "one"},
                    "lengthText": {"simpleText": "3:13"},
                    "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/a/hq.jpg", "width": 480, "height": 360}]},
                    "navigationEndpoint": {"watchEndpoint": {"videoId": "aaaaaaaaaaa", "index": 0, "params": "OAE%3D"}}
                }},
                {"playlistPanelVideoRenderer": {
                    "videoId": "bbbbbbbbbbb",
                    "title": {"runs": [{"text": "two"}]},
                    "lengthSeconds": "125",
                    "navigationEndpoint": {"watchEndpoint": {"videoId": "bbbbbbbbbbb", "index": 1, "params": "OAE%3D"}}
                }}
            ]
        });
        let (entries, endpoint) = extract_inline_entries(&playlist);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].duration, Some(193));
        assert_eq!(entries[0].thumbnails[0].width, Some(480));
        assert_eq!(entries[1].title, "two");
        assert_eq!(entries[1].duration, Some(125));
        let endpoint = endpoint.unwrap();
        assert_eq!(endpoint.video_id, "bbbbbbbbbbb");
        assert_eq!(endpoint.index, Some(1));
    }

    #[test]
    fn extracts_json_assignment_with_braces_in_strings() {
        let html = r#"<script>var ytInitialData = {"a":"}","b":{"c":1}};</script>"#;
        let value = extract_json_assignment(html, "ytInitialData").unwrap();
        assert_eq!(value["b"]["c"], 1);
    }

    #[test]
    fn rejects_invalid_video_ids() {
        assert!(classify("https://www.youtube.com/watch?v=short", false).is_err());
        assert!(classify("https://www.youtube.com/playlist", false).is_err());
        assert!(classify("https://example.com/watch?v=dQw4w9WgXcQ", false).is_err());
    }

    #[test]
    fn open_ranges_beyond_the_playlist_are_empty() {
        assert_eq!(parse_item_ranges("10-", 5).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_item_ranges("10-20", 5).unwrap(), Vec::<usize>::new());
        assert!(parse_item_ranges("10-5", 5).is_err());
    }

    #[test]
    fn classifies_playability_reasons() {
        assert_eq!(
            playability_code("Sign in to confirm you\'re not a bot"),
            ErrorCode::BotCheck
        );
        assert_eq!(
            playability_code("Sign in to confirm your age"),
            ErrorCode::AgeRestricted
        );
        assert_eq!(
            playability_code("This video is private"),
            ErrorCode::Private
        );
        assert_eq!(
            playability_code("This video is unavailable"),
            ErrorCode::Unavailable
        );
    }

    #[test]
    fn resolves_relative_player_js_urls() {
        let cfg = json!({"PLAYER_JS_URL": "/s/player/abc/player_ias.vflset/en_US/base.js"});
        assert_eq!(
            player_js_url_from_cfg(&cfg).as_deref(),
            Some("https://www.youtube.com/s/player/abc/player_ias.vflset/en_US/base.js")
        );
        let cfg = json!({"PLAYER_JS_URL": "//www.youtube.com/s/player/abc/base.js"});
        assert_eq!(
            player_js_url_from_cfg(&cfg).as_deref(),
            Some("https://www.youtube.com/s/player/abc/base.js")
        );
    }

    #[test]
    fn parses_muxed_mime() {
        let (container, vcodec, acodec, has_video, has_audio) =
            parse_mime_and_codecs("video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"");
        assert_eq!(container, "mp4");
        assert_eq!(vcodec.as_deref(), Some("avc1.42001E"));
        assert_eq!(acodec.as_deref(), Some("mp4a.40.2"));
        assert!(has_video && has_audio);
    }

    #[test]
    fn parses_unquoted_and_spaced_codecs() {
        let (_, vcodec, acodec, has_video, has_audio) =
            parse_mime_and_codecs("VIDEO/MP4; codecs = avc1.4d401f , mp4a.40.2");
        assert_eq!(vcodec.as_deref(), Some("avc1.4d401f"));
        assert_eq!(acodec.as_deref(), Some("mp4a.40.2"));
        assert!(has_video && has_audio);
    }

    #[test]
    fn normalizes_iso_upload_dates() {
        assert_eq!(normalize_upload_date("2024-01-02"), "20240102");
        assert_eq!(
            normalize_upload_date("2024-01-02T00:00:00-08:00"),
            "20240102"
        );
        assert_eq!(normalize_upload_date("not a date"), "");
    }

    #[test]
    fn normalizes_tcl_and_tcc_player_variants() {
        assert_eq!(
            normalize_player_js_url(
                "https://www.youtube.com/s/player/abc/player_es6_tcl.vflset/en_US/base.js"
                    .to_string()
            ),
            "https://www.youtube.com/s/player/abc/player_es6.vflset/en_US/base.js"
        );
        assert_eq!(
            normalize_player_js_url(
                "https://www.youtube.com/s/player/abc/player_es6_tcc.vflset/en_US/base.js"
                    .to_string()
            ),
            "https://www.youtube.com/s/player/abc/player_es6.vflset/en_US/base.js"
        );
        assert_eq!(
            normalize_player_js_url(
                "https://www.youtube.com/s/player/abc/player_ias_tcl.vflset/en_US/base.js"
                    .to_string()
            ),
            "https://www.youtube.com/s/player/abc/player_ias.vflset/en_US/base.js"
        );
        assert_eq!(
            normalize_player_js_url(
                "https://www.youtube.com/s/player/abc/player_ias_tcc.vflset/en_US/base.js"
                    .to_string()
            ),
            "https://www.youtube.com/s/player/abc/player_ias.vflset/en_US/base.js"
        );
    }

    #[test]
    fn finds_signature_timestamp_variants() {
        assert_eq!(
            find_signature_timestamp("var a={signatureTimestamp:19369};"),
            Some(19369)
        );
        assert_eq!(
            find_signature_timestamp("var a={\"signatureTimestamp\": 19369};"),
            Some(19369)
        );
        assert_eq!(find_signature_timestamp("var a={sts:19369};"), Some(19369));
        assert_eq!(
            find_signature_timestamp(
                "var s=\"signatureTimestamp\";var a={signatureTimestamp:19369};"
            ),
            Some(19369)
        );
        assert_eq!(find_signature_timestamp("var stsx=1;"), None);
    }

    #[test]
    fn reads_sts_from_ytcfg() {
        assert_eq!(sts_from_ytcfg(&json!({"STS": 20768})), Some(20768));
        assert_eq!(sts_from_ytcfg(&json!({"STS": "20768"})), Some(20768));
        assert_eq!(sts_from_ytcfg(&json!({"STS": 0})), None);
        assert_eq!(sts_from_ytcfg(&json!({"STS": "0"})), None);
        assert_eq!(sts_from_ytcfg(&json!({})), None);
    }

    #[test]
    fn detects_page_reload_player_error() {
        let reload: PlayerResponse = serde_json::from_value(json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "reason": "The page needs to be reloaded."
            }
        }))
        .unwrap();
        assert!(needs_page_reload(&reload));

        let ok: PlayerResponse = serde_json::from_value(json!({
            "playabilityStatus": {"status": "OK"}
        }))
        .unwrap();
        assert!(!needs_page_reload(&ok));

        let other: PlayerResponse = serde_json::from_value(json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "reason": "This video is unavailable"
            }
        }))
        .unwrap();
        assert!(!needs_page_reload(&other));
    }

    #[test]
    fn player_source_cache_seeds_signature_timestamp() {
        let url = "https://www.youtube.com/s/player/test-source-sts-19369/base.js";
        remember_player_source(url, "var a={signatureTimestamp:19369};");
        assert_eq!(cached_sts(url), Some(19369));
        assert_eq!(
            cached_player_source(url).as_deref(),
            Some("var a={signatureTimestamp:19369};")
        );
    }

    #[test]
    fn evicts_only_the_oldest_player_source() {
        let mut cache = HashMap::new();
        let base = Instant::now();
        for index in 0..PLAYER_SOURCE_CACHE_CAPACITY {
            let cell = AsyncOnceCell::new();
            let _ = cell.set(Arc::from(format!("source-{index}")));
            cache.insert(
                format!("https://www.youtube.com/s/player/lru-{index}/base.js"),
                PlayerSourceEntry {
                    cell: Arc::new(cell),
                    inserted_at: base,
                    last_used: base + Duration::from_millis(index as u64),
                },
            );
        }
        evict_player_source(
            &mut cache,
            "https://www.youtube.com/s/player/lru-new/base.js",
        );
        assert_eq!(cache.len(), PLAYER_SOURCE_CACHE_CAPACITY - 1);
        assert!(!cache.contains_key("https://www.youtube.com/s/player/lru-0/base.js"));
        assert!(cache.contains_key(&format!(
            "https://www.youtube.com/s/player/lru-{}/base.js",
            PLAYER_SOURCE_CACHE_CAPACITY - 1
        )));
    }

    #[tokio::test]
    async fn shares_in_flight_player_source_fetch() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let url = "https://www.youtube.com/s/player/test-dedup-inflight/base.js";
        let first = player_source_with(url, || async {
            CALLS.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(Arc::from("var a={signatureTimestamp:19369};"))
        });
        let second = player_source_with(url, || async {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::from("var a={signatureTimestamp:19369};"))
        });
        let (first, second) = tokio::join!(first, second);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(first.unwrap(), second.unwrap());
    }

    #[tokio::test]
    async fn retries_player_source_after_fetch_error() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let url = "https://www.youtube.com/s/player/test-dedup-error/base.js";
        let failed = player_source_with(url, || async {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Err(YtrustError::coded(ErrorCode::Network, "boom"))
        })
        .await;
        assert!(failed.is_err());
        let retried = player_source_with(url, || async {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::from("var a={signatureTimestamp:19370};"))
        })
        .await
        .unwrap();
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(cached_sts(url), Some(19370));
        assert_eq!(retried.as_ref(), "var a={signatureTimestamp:19370};");
    }

    #[test]
    fn parses_audio_bitrate_and_language() {
        let raw: RawFormat = serde_json::from_value(json!({
            "itag": 140,
            "url": "https://rr1.googlevideo.com/videoplayback?itag=140",
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "averageBitrate": 129000,
            "bitRate": 128000,
            "contentLength": "12345",
            "audioTrack": {"id": "en-US.4", "audioIsDefault": true}
        }))
        .unwrap();
        let format = parse_format(raw, ClientKind::VisionOs, false).unwrap();
        assert_eq!(format.audio_bitrate, Some(129000));
        assert_eq!(format.language.as_deref(), Some("en-US"));
        assert_eq!(format.content_length, Some(12345));
        assert!(format.audio_is_default);
    }

    #[test]
    fn empty_n_query_is_not_a_challenge() {
        let raw: RawFormat = serde_json::from_value(json!({
            "itag": 140,
            "url": "https://rr1.googlevideo.com/videoplayback?n=",
            "mimeType": "audio/mp4"
        }))
        .unwrap();
        let format = parse_format(raw, ClientKind::VisionOs, false).unwrap();
        assert!(!format.needs_challenge());
    }

    #[test]
    fn continuation_only_comes_from_known_containers() {
        let response = json!({
            "unrelated": {"continuationCommand": {"token": "wrong"}},
            "contents": {
                "playlistVideoListRenderer": {
                    "contents": [{"continuationItemRenderer": {
                        "continuationEndpoint": {"continuationCommand": {"token": "right"}}
                    }}]
                }
            }
        });
        assert_eq!(find_continuation_token(&response).as_deref(), Some("right"));
    }

    #[test]
    fn classifies_music_and_nocookie_hosts() {
        assert!(matches!(
            classify("https://music.youtube.com/watch?v=dQw4w9WgXcQ", false).unwrap(),
            Target::Video { id } if id == "dQw4w9WgXcQ"
        ));
        assert!(matches!(
            classify("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ", false).unwrap(),
            Target::Video { id } if id == "dQw4w9WgXcQ"
        ));
    }

    #[test]
    fn reads_plain_and_structured_text() {
        assert_eq!(
            value_text(&json!("Mix - title")),
            Some("Mix - title".to_string())
        );
        assert_eq!(
            value_text(&json!({"runs": [{"text": "a"}, {"text": "b"}]})),
            Some("ab".to_string())
        );
        assert_eq!(
            value_text(&json!({"simpleText": "plain"})),
            Some("plain".to_string())
        );
    }

    #[test]
    fn merges_metadata_omitted_by_tv_samsung() {
        let mut info = VideoInfo {
            id: "dQw4w9WgXcQ".to_string(),
            title: String::new(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            webpage_url: String::new(),
            description: String::new(),
            duration_secs: Some(213),
            view_count: None,
            upload_date: String::new(),
            is_live: false,
            thumbnails: Vec::new(),
            subtitles: Vec::new(),
            automatic_captions: Vec::new(),
            formats: Vec::new(),
            hls_manifest_url: None,
            player_js_url: None,
        };
        let response: PlayerResponse = serde_json::from_value(json!({
            "videoDetails": {
                "videoId": "dQw4w9WgXcQ",
                "title": "Title",
                "author": "Uploader",
                "ownerChannelName": "Channel",
                "channelId": "UC123",
                "shortDescription": "Description",
                "lengthSeconds": "213",
                "viewCount": "42"
            },
            "microformat": {
                "playerMicroformatRenderer": {
                    "uploadDate": "2009-10-24T23:57:33-07:00",
                    "ownerChannelName": "Channel"
                }
            }
        }))
        .unwrap();
        merge_watch_metadata(&mut info, response);
        assert_eq!(info.title, "Title");
        assert_eq!(info.uploader, "Uploader");
        assert_eq!(info.channel, "Channel");
        assert_eq!(info.channel_id, "UC123");
        assert_eq!(info.description, "Description");
        assert_eq!(info.view_count, Some(42));
        assert_eq!(info.upload_date, "20091024");
    }

    #[test]
    fn orders_player_clients_for_auth_and_jsless() {
        let mut config = RequestConfig::default();
        assert_eq!(
            client_order(&config),
            &[ClientKind::TvSamsung, ClientKind::VisionOs]
        );

        config.sapisid = Some("sid".to_string());
        assert_eq!(client_order(&config), &[ClientKind::TvSamsung]);

        config.prefer_jsless = true;
        assert_eq!(
            client_order(&config),
            &[ClientKind::VisionOs, ClientKind::TvSamsung]
        );
    }

    #[test]
    fn caches_player_source_by_url() {
        let url = "https://www.youtube.com/s/player/test-source/base.js";
        remember_player_source(url, "source");
        assert_eq!(cached_player_source(url).as_deref(), Some("source"));
    }

    #[test]
    fn tv_samsung_context_matches_yt_dlp() {
        let (context, client_id, client_version) = ClientKind::TvSamsung.context();
        assert_eq!(client_id, "7");
        assert_eq!(client_version, "5.20260707");
        assert_eq!(context["clientName"], "TVHTML5");
        assert_eq!(context["clientVersion"], "5.20260707");
        assert_eq!(context["deviceMake"], "Samsung");
        assert_eq!(context["deviceModel"], "UKS9800");
        assert_eq!(context["osName"], "Tizen");
        assert_eq!(context["osVersion"], "2.4.0");
        assert_eq!(context["userAgent"], TV_SAMSUNG_UA);
    }

    #[test]
    fn only_cookie_capable_clients_send_cookies() {
        assert!(ClientKind::TvSamsung.supports_cookies());
        assert!(ClientKind::WebEmbedded.supports_cookies());
        assert!(!ClientKind::VisionOs.supports_cookies());
    }

    #[test]
    fn extracts_upload_date_from_page_html() {
        assert_eq!(
            upload_date_from_html(r#"{"uploadDate":"2009-10-25T07:00:00-07:00"}"#).as_deref(),
            Some("20091025")
        );
        assert_eq!(
            upload_date_from_html(r#"{"publishDate":"2024-01-02"}"#).as_deref(),
            Some("20240102")
        );
        assert_eq!(upload_date_from_html("<html></html>"), None);
    }
}
