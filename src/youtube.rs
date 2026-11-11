use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::constants::{
    TV_CLIENT_NAME, TV_CLIENT_VERSION, TV_UA, VISIONOS_CLIENT_NAME, VISIONOS_CLIENT_VERSION,
    VISIONOS_UA, WEB_CLIENT_NAME, WEB_CLIENT_VERSION, WEB_EMBEDDED_CLIENT_NAME,
    WEB_EMBEDDED_CLIENT_VERSION, WEB_EMBEDDED_UA, WEB_UA,
};
use crate::error::{ErrorCode, Result, YtrustError};
use crate::http::{InnertubeRequest, client as build_client, fetch_text, post_innertube};

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
    pub mime_type: String,
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
    pub user_agent: String,
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
    let list = parsed
        .query_pairs()
        .find(|(key, _)| key == "list")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty());
    let video = parsed
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty());

    if let Some(ref list) = list
        && !no_playlist
    {
        return Ok(Target::Playlist {
            id: list.clone(),
            video_id: video,
        });
    }

    if let Some(video_id) = video {
        let video_id = valid_id(&video_id).ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("invalid video ID: {video_id}"),
            )
        })?;
        return Ok(Target::Video { id: video_id });
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

    if let Some(id) = video_id_from_url(&parsed) {
        return Ok(Target::Video { id });
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

pub fn video_id_from_url(url: &Url) -> Option<String> {
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
    let id = id.split(['&', '?', '/']).next().unwrap_or(id);
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
    let mut fallback = None;

    for client_kind in [ClientKind::VisionOs, ClientKind::Tv] {
        match fetch_player_response(&client, video_id, client_kind, config).await {
            Ok(response) => match parse_video_info(video_id, response, client_kind, None) {
                Ok(mut info) if !info.formats.is_empty() => {
                    if !needs_player_js(&info) {
                        return Ok(info);
                    }
                    match player_js_url(video_id, config).await {
                        Ok(url) => {
                            info.player_js_url = Some(url);
                            return Ok(info);
                        }
                        Err(error) => errors.push(error),
                    }
                    fallback = Some(info);
                }
                Ok(_) => {
                    errors.push(YtrustError::coded(
                        ErrorCode::Unavailable,
                        "YouTube returned no formats",
                    ));
                }
                Err(error) => errors.push(error),
            },
            Err(error) => errors.push(error),
        }
    }

    match fetch_web_embedded_video(video_id, config).await {
        Ok(info) if !info.formats.is_empty() => return Ok(info),
        Ok(_) => {
            errors.push(YtrustError::coded(
                ErrorCode::Unavailable,
                "YouTube returned no formats",
            ));
        }
        Err(error) => errors.push(error),
    }

    if let Some(info) = fallback
        && info.formats.iter().any(|format| !format.url.is_empty())
    {
        return Ok(info);
    }

    Err(errors.pop().unwrap_or_else(|| {
        YtrustError::coded(
            ErrorCode::Unavailable,
            "YouTube did not return a playable response",
        )
    }))
}

fn needs_player_js(info: &VideoInfo) -> bool {
    info.formats.iter().any(|format| {
        format.signature_cipher.is_some()
            || format.cipher.is_some()
            || (!format.url.is_empty() && url_has_n(&format.url))
    })
}

fn url_has_n(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .is_some_and(|url| url.query_pairs().any(|(key, _)| key == "n"))
}

const PLAYER_JS_URL_TTL: Duration = Duration::from_secs(30 * 60);

static PLAYER_JS_URL_CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

fn cached_player_js_url() -> Option<String> {
    let cache = PLAYER_JS_URL_CACHE.get_or_init(|| Mutex::new(None));
    let guard = cache.lock().ok()?;
    let (cached_at, url) = guard.as_ref()?;
    (cached_at.elapsed() < PLAYER_JS_URL_TTL).then(|| url.clone())
}

fn remember_player_js_url(url: &str) {
    let cache = PLAYER_JS_URL_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), url.to_string()));
    }
}

/// Resolve the current player JS URL from the watch or embed page.
pub async fn player_js_url(video_id: &str, config: &RequestConfig) -> Result<String> {
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
    let client = build_client(config)?;
    fetch_text(&client, url, config, WEB_UA, None).await
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
            if url.starts_with("http") {
                url.to_string()
            } else if url.starts_with("//") {
                format!("https:{url}")
            } else {
                format!("https://www.youtube.com{url}")
            }
        })
}

static SIGNATURE_TIMESTAMPS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

async fn fetch_web_embedded_video(video_id: &str, config: &RequestConfig) -> Result<VideoInfo> {
    let client = build_client(config)?;
    let embed_url = format!("https://www.youtube.com/embed/{video_id}?html5=1");
    let html = fetch_text(
        &client,
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
    let signature_timestamp = signature_timestamp(&client, &player_js_url, config).await?;
    let encrypted_flags = ytcfg
        .pointer("/WEB_PLAYER_CONTEXT_CONFIGS/WEB_PLAYER_CONTEXT_CONFIG_ID_EMBEDDED_PLAYER/encryptedHostFlags")
        .and_then(Value::as_str);
    let mut playback = json!({
        "html5Preference": "HTML5_PREF_WANTS",
        "signatureTimestamp": signature_timestamp,
    });
    if let Some(flags) = encrypted_flags {
        playback["encryptedHostFlags"] = json!(flags);
    }
    let client_version = ytcfg
        .pointer("/INNERTUBE_CONTEXT/client/clientVersion")
        .and_then(Value::as_str)
        .unwrap_or(WEB_EMBEDDED_CLIENT_VERSION);
    let body = json!({
        "context": context,
        "videoId": video_id,
        "playbackContext": {"contentPlaybackContext": playback},
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let response: PlayerResponse = post_innertube(
        &client,
        InnertubeRequest {
            endpoint: "player",
            body: &body,
            client_name: WEB_EMBEDDED_CLIENT_NAME,
            client_version,
            user_agent: WEB_EMBEDDED_UA,
            visitor_data,
            user_session_id: ytcfg.get("USER_SESSION_ID").and_then(Value::as_str),
            browser: true,
        },
        config,
    )
    .await?;
    parse_video_info(
        video_id,
        response,
        ClientKind::WebEmbedded,
        Some(player_js_url),
    )
}

async fn signature_timestamp(
    client: &reqwest::Client,
    player_js_url: &str,
    config: &RequestConfig,
) -> Result<u64> {
    if let Some(cached) = SIGNATURE_TIMESTAMPS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(player_js_url).copied())
    {
        return Ok(cached);
    }
    let source = fetch_text(client, player_js_url, config, WEB_EMBEDDED_UA, None).await?;
    let marker = "signatureTimestamp";
    let at = source.find(marker).ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "player JS has no signatureTimestamp",
        )
    })?;
    let digits = source[at + marker.len()..]
        .trim_start_matches([':', '=', ' '])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let value = digits.parse::<u64>().map_err(|_| {
        YtrustError::coded(
            ErrorCode::InvalidResponse,
            "player JS signatureTimestamp is invalid",
        )
    })?;
    if let Ok(mut cache) = SIGNATURE_TIMESTAMPS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        cache.insert(player_js_url.to_string(), value);
    }
    Ok(value)
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
    let mut seen = BTreeSet::new();
    let mut continuation = None;
    let mut title = String::from("Playlist");

    for page in 0..40 {
        let body = match &continuation {
            Some(token) => json!({
                "context": web_context(),
                "continuation": token,
            }),
            None => json!({
                "context": web_context(),
                "browseId": browse_id,
            }),
        };

        let response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "browse",
                body: &body,
                client_name: WEB_CLIENT_NAME,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
            },
            config,
        )
        .await?;

        if page == 0
            && let Some(found_title) = find_playlist_title(&response)
        {
            title = found_title;
        }
        collect_playlist_videos(&response, &mut entries, &mut seen, limit as usize);
        tracing::debug!(
            "playlist browse response: entries={}, continuation={}",
            entries.len(),
            find_continuation_token(&response).is_some()
        );

        if entries.len() >= limit as usize {
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
    let radio = if playlist_id.starts_with("RD") {
        "&start_radio=1"
    } else {
        ""
    };
    let watch_url =
        format!("https://www.youtube.com/watch?v={video_id}&list={playlist_id}{radio}&hl=en&gl=US");
    let html = fetch_text(&client, &watch_url, config, WEB_UA, None).await?;
    let data = extract_json_assignment(&html, "ytInitialData")?;
    let mut playlist = data
        .pointer("/contents/twoColumnWatchNextResults/playlist/playlist")
        .cloned()
        .ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidResponse,
                "YouTube returned no inline playlist",
            )
        })?;
    let title = playlist
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Playlist")
        .to_string();

    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for _ in 0..40 {
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
            if entries.len() >= limit as usize {
                break;
            }
        }
        if entries.len() >= limit as usize {
            break;
        }
        let Some(endpoint) = endpoint else { break };
        let body = json!({
            "context": web_context(),
            "playlistId": playlist_id,
            "videoId": endpoint.video_id,
            "index": endpoint.index.unwrap_or(entries.len() as u64),
            "params": endpoint.params.unwrap_or_else(|| "OAE%3D".to_string()),
        });
        let response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "next",
                body: &body,
                client_name: WEB_CLIENT_NAME,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
            },
            config,
        )
        .await?;
        let Some(next) = response
            .pointer("/contents/twoColumnWatchNextResults/playlist/playlist")
            .cloned()
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
                depth -= 1;
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

fn text_of(value: &Value) -> Option<String> {
    if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
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
    for part in text.trim().split(':') {
        total = total
            .checked_mul(60)?
            .checked_add(part.trim().parse::<u64>().ok()?)?;
    }
    Some(total)
}

pub async fn fetch_search(query: &str, limit: u64, config: &RequestConfig) -> Result<PlaylistInfo> {
    let client = build_client(config)?;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut continuation = None;

    for _ in 0..40 {
        let body = match &continuation {
            Some(token) => json!({
                "context": web_context(),
                "continuation": token,
            }),
            None => json!({
                "context": web_context(),
                "query": query,
            }),
        };
        let response: Value = post_innertube(
            &client,
            InnertubeRequest {
                endpoint: "search",
                body: &body,
                client_name: WEB_CLIENT_NAME,
                client_version: WEB_CLIENT_VERSION,
                user_agent: WEB_UA,
                visitor_data: None,
                user_session_id: None,
                browser: false,
            },
            config,
        )
        .await?;
        collect_playlist_videos(&response, &mut entries, &mut seen, limit as usize);
        if entries.len() >= limit as usize {
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

pub fn max_playlist_item(expression: &str) -> Option<usize> {
    let mut max = None;
    for part in expression
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            if start.trim().is_empty() || end.trim().is_empty() {
                return None;
            }
            let end = end.trim().parse::<usize>().ok()?;
            max = Some(max.map_or(end, |current: usize| current.max(end)));
        } else {
            let index = part.parse::<usize>().ok()?;
            max = Some(max.map_or(index, |current: usize| current.max(index)));
        }
    }
    max
}

pub fn filter_playlist_entries(playlist: &mut PlaylistInfo, selection: &str) -> Result<()> {
    let selected = parse_item_ranges(selection, playlist.entries.len())?;
    if selected.is_empty() {
        playlist.entries.clear();
        return Ok(());
    }
    let selected: BTreeSet<usize> = selected.into_iter().collect();
    playlist
        .entries
        .retain(|entry| selected.contains(&entry.index));
    Ok(())
}

pub fn parse_item_ranges(expression: &str, total: usize) -> Result<Vec<usize>> {
    let mut selected = BTreeSet::new();

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
                start_raw.parse::<usize>().map_err(|_| {
                    YtrustError::coded(
                        ErrorCode::InvalidInput,
                        format!("invalid playlist range: {part}"),
                    )
                })?
            };
            let open_ended = end_raw.is_empty();
            let end = if open_ended {
                total
            } else {
                end_raw.parse::<usize>().map_err(|_| {
                    YtrustError::coded(
                        ErrorCode::InvalidInput,
                        format!("invalid playlist range: {part}"),
                    )
                })?
            };
            if start == 0 || (!open_ended && (end == 0 || start > end)) {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("invalid playlist range: {part}"),
                ));
            }
            let upper = end.min(total);
            if start <= upper {
                for index in start..=upper {
                    selected.insert(index);
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
            if index <= total {
                selected.insert(index);
            }
        }
    }

    Ok(selected.into_iter().collect())
}

#[derive(Debug, Clone, Copy)]
enum ClientKind {
    WebEmbedded,
    VisionOs,
    Tv,
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
                WEB_EMBEDDED_CLIENT_NAME,
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
                VISIONOS_CLIENT_NAME,
                VISIONOS_CLIENT_VERSION,
            ),
            Self::Tv => (
                json!({
                    "clientName": "TVHTML5",
                    "clientVersion": TV_CLIENT_VERSION,
                    "userAgent": TV_UA,
                    "hl": "en",
                    "gl": "US",
                }),
                TV_CLIENT_NAME,
                TV_CLIENT_VERSION,
            ),
        }
    }

    fn user_agent(self) -> &'static str {
        match self {
            Self::VisionOs => VISIONOS_UA,
            Self::WebEmbedded => WEB_EMBEDDED_UA,
            Self::Tv => TV_UA,
        }
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

async fn fetch_player_response(
    client: &reqwest::Client,
    video_id: &str,
    client_kind: ClientKind,
    config: &RequestConfig,
) -> Result<PlayerResponse> {
    let (context, client_name, client_version) = client_kind.context();
    let body = json!({
        "context": {"client": context},
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    post_innertube(
        client,
        InnertubeRequest {
            endpoint: "player",
            body: &body,
            client_name,
            client_version,
            user_agent: client_kind.user_agent(),
            visitor_data: None,
            user_session_id: None,
            browser: false,
        },
        config,
    )
    .await
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
    status: String,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamingData {
    formats: Option<Vec<RawFormat>>,
    #[serde(rename = "adaptiveFormats")]
    adaptive_formats: Option<Vec<RawFormat>>,
    #[serde(rename = "hlsManifestUrl")]
    hls_manifest_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    itag: u64,
    url: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: String,
    bitrate: Option<u64>,
    #[serde(rename = "averageBitrate")]
    average_bitrate: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    fps: Option<u64>,
    #[serde(rename = "contentLength")]
    content_length: Option<String>,
    #[serde(rename = "qualityLabel")]
    quality_label: Option<String>,
    #[serde(rename = "audioQuality")]
    audio_quality: Option<String>,
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
    #[serde(rename = "videoId")]
    video_id: String,
    title: String,
    author: Option<String>,
    #[serde(rename = "ownerChannelName")]
    owner_channel_name: Option<String>,
    #[serde(rename = "shortDescription")]
    short_description: Option<String>,
    #[serde(rename = "lengthSeconds")]
    length_seconds: Option<String>,
    #[serde(rename = "viewCount")]
    view_count: Option<String>,
    #[serde(rename = "isLiveContent")]
    is_live_content: Option<bool>,
    #[serde(rename = "channelId")]
    channel_id: Option<String>,
    thumbnail: Option<ThumbnailList>,
}

#[derive(Debug, Deserialize)]
struct ThumbnailList {
    thumbnails: Option<Vec<RawThumbnail>>,
}

#[derive(Debug, Deserialize)]
struct RawThumbnail {
    url: String,
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
    #[serde(rename = "captionTracks")]
    caption_tracks: Option<Vec<RawCaptionTrack>>,
}

#[derive(Debug, Deserialize)]
struct RawCaptionTrack {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "languageCode")]
    language_code: String,
    name: Option<Value>,
    kind: Option<String>,
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
        if status.status != "OK" {
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

    let hls_manifest_url = streaming.hls_manifest_url.clone();
    let format_count = streaming.formats.as_ref().map_or(0, Vec::len)
        + streaming.adaptive_formats.as_ref().map_or(0, Vec::len);
    tracing::debug!("player response has {format_count} raw formats");

    let mut formats = Vec::new();
    for raw in streaming
        .formats
        .unwrap_or_default()
        .into_iter()
        .chain(streaming.adaptive_formats.unwrap_or_default())
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
            mime_type: "application/vnd.apple.mpegurl".to_string(),
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
            user_agent: client_kind.user_agent().to_string(),
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

    let duration_secs = details
        .length_seconds
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);

    let thumbnails = details
        .thumbnail
        .as_ref()
        .and_then(|list| list.thumbnails.as_ref())
        .map(|items| {
            items
                .iter()
                .map(|item| Thumbnail {
                    url: item.url.clone(),
                    width: item.width,
                    height: item.height,
                })
                .collect()
        })
        .unwrap_or_default();

    let (subtitles, automatic_captions) = parse_captions(captions);
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
        view_count: details
            .view_count
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok()),
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
    if url.is_empty() && raw.signature_cipher.is_none() && raw.cipher.is_none() {
        return None;
    }

    let (container, video_codec, audio_codec, has_video, has_audio) =
        parse_mime_and_codecs(&raw.mime_type);
    let audio_bitrate = if has_audio && !has_video {
        raw.audio_quality.as_ref().map(|_| raw.bitrate.unwrap_or(0))
    } else {
        None
    };

    Some(Format {
        itag: raw.itag,
        url,
        signature_cipher: raw.signature_cipher,
        cipher: raw.cipher,
        container,
        mime_type: raw.mime_type,
        video_codec,
        audio_codec,
        has_video,
        has_audio,
        width: raw.width,
        height: raw.height,
        fps: raw.fps,
        bitrate: raw.average_bitrate.or(raw.bitrate).unwrap_or(0),
        audio_bitrate,
        content_length: raw
            .content_length
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok()),
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
        user_agent: client_kind.user_agent().to_string(),
    })
}

fn normalize_upload_date(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

fn parse_captions(captions: Option<Captions>) -> (Vec<SubtitleTrack>, Vec<SubtitleTrack>) {
    let mut subtitles = Vec::new();
    let mut automatic = Vec::new();

    let tracks = captions
        .and_then(|captions| captions.player_captions_tracklist_renderer)
        .and_then(|tracklist| tracklist.caption_tracks)
        .unwrap_or_default();

    for track in tracks {
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
            url: track.base_url,
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

fn parse_mime_and_codecs(mime_type: &str) -> (String, Option<String>, Option<String>, bool, bool) {
    let mime = mime_type.split(';').next().unwrap_or(mime_type).trim();
    let mut mime_parts = mime.split('/');
    let kind = mime_parts.next().unwrap_or_default();
    let container = match mime_parts.next().unwrap_or_default() {
        "mp4" => "mp4",
        "webm" => "webm",
        "3gpp" => "3gp",
        "x-matroska" => "mkv",
        other => other,
    }
    .to_string();

    let codecs = mime_type
        .split("codecs=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(|value| {
            value
                .split(',')
                .map(|codec| codec.trim().to_string())
                .filter(|codec| !codec.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let has_video = kind == "video";
    let has_audio = kind == "audio" || (has_video && codecs.len() > 1);
    let video_codec = if has_video {
        codecs.first().cloned()
    } else {
        None
    };
    let audio_codec = if kind == "audio" {
        codecs.first().cloned()
    } else if codecs.len() > 1 {
        codecs.get(1).cloned()
    } else {
        None
    };

    (container, video_codec, audio_codec, has_video, has_audio)
}

fn find_playlist_title(value: &Value) -> Option<String> {
    find_key_value(value, "playlistMetadataRenderer")
        .and_then(|metadata| metadata.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| find_string(value, &["playlistHeaderRenderer", "title", "simpleText"]))
        .or_else(|| {
            value
                .pointer("/sidebar/playlistSidebarRenderer/items/0/playlistSidebarPrimaryInfoRenderer/title/runs/0/text")
                .and_then(Value::as_str)
                .map(str::to_string)
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
    seen: &mut BTreeSet<String>,
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
                    .unwrap_or("")
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
    let title =
        text_of(renderer.get("title").unwrap_or(&Value::Null)).unwrap_or_else(|| id.to_string());
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
    seen: &mut BTreeSet<String>,
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
    find_continuation_token_in(value, false).or_else(|| find_continuation_token_in(value, true))
}

fn find_continuation_token_in(value: &Value, in_playlist: bool) -> Option<String> {
    match value {
        Value::Object(map) => {
            let in_playlist = in_playlist
                || map.contains_key("playlistVideoListRenderer")
                || map.contains_key("playlistPanelRenderer");
            if in_playlist
                && let Some(token) = map
                    .get("continuationCommand")
                    .and_then(|command| command.get("token"))
                    .and_then(Value::as_str)
            {
                return Some(token.to_string());
            }
            for child in map.values() {
                if let Some(token) = find_continuation_token_in(child, in_playlist) {
                    return Some(token);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_continuation_token_in(child, in_playlist)),
        _ => None,
    }
}

fn find_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
