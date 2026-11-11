use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::error::{Result, YtrustError};

const VISIONOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default)]
pub struct RequestConfig {
    pub proxy: Option<String>,
    pub cookies: Option<String>,
    pub timeout: u64,
}

#[derive(Debug, Clone)]
pub enum Target {
    Video { id: String },
    Playlist { id: String },
    Search { query: String, limit: u64 },
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

#[derive(Debug, Clone, serde::Serialize)]
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
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Thumbnail {
    pub url: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SubtitleTrack {
    pub language: String,
    pub name: String,
    pub url: String,
    pub is_auto: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Format {
    pub itag: u64,
    pub url: String,
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
            YtrustError::msg("search syntax is ytsearchN:query or ytsearch:query")
        })?;
        let limit = if count.is_empty() {
            1
        } else {
            count
                .parse::<u64>()
                .map_err(|_| YtrustError::msg(format!("invalid search count: {count}")))?
        };
        if query.trim().is_empty() {
            return Err(YtrustError::msg("search query is empty"));
        }
        return Ok(Target::Search {
            query: query.to_string(),
            limit: limit.max(1),
        });
    }

    let parsed = Url::parse(url).map_err(|_| YtrustError::msg(format!("invalid URL: {url}")))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_start_matches("m.");

    if !matches!(
        host,
        "youtube.com" | "youtu.be" | "music.youtube.com" | "youtube-nocookie.com"
    ) {
        return Err(YtrustError::msg(format!(
            "not a YouTube URL: {url}\nOnly YouTube is supported."
        )));
    }

    let path = parsed.path();
    let list = parsed
        .query_pairs()
        .find(|(key, _)| key == "list")
        .map(|(_, value)| value.to_string());
    let video = parsed
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.to_string());

    if let Some(ref list) = list
        && !no_playlist
    {
        return Ok(Target::Playlist { id: list.clone() });
    }

    if let Some(video_id) = video {
        return Ok(Target::Video { id: video_id });
    }

    if path.starts_with("/playlist") {
        let list = list.ok_or_else(|| YtrustError::msg("playlist URL has no list parameter"))?;
        return Ok(Target::Playlist { id: list });
    }

    if let Some(id) = video_id_from_url(&parsed) {
        return Ok(Target::Video { id });
    }

    if path.starts_with("/@")
        || path.starts_with("/channel/")
        || path.starts_with("/user/")
        || path.starts_with("/c/")
    {
        return Err(YtrustError::msg(
            "channel URLs are not supported yet; use a video or playlist URL",
        ));
    }

    Err(YtrustError::msg(format!("unsupported YouTube URL: {url}")))
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
    let clients = [ClientKind::VisionOs, ClientKind::Tv];
    let mut last_error = None;

    for client_kind in clients {
        match fetch_player_response(&client, video_id, client_kind, config).await {
            Ok(response) => match parse_video_info(video_id, response, client_kind) {
                Ok(info) if !info.formats.is_empty() => return Ok(info),
                Ok(_) => last_error = Some(YtrustError::msg("YouTube returned no formats")),
                Err(err) => last_error = Some(err),
            },
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error
        .unwrap_or_else(|| YtrustError::msg("YouTube did not return a playable response")))
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
    let mut seen = std::collections::BTreeSet::new();
    let mut continuation = None;
    let mut title = String::from("Playlist");

    for _ in 0..40 {
        let mut body = json!({
            "context": web_context(),
            "browseId": browse_id,
        });
        if let Some(token) = &continuation {
            body = json!({
                "context": web_context(),
                "continuation": token,
            });
        }

        let response = post_innertube(
            &client,
            "browse",
            &body,
            "1",
            "2.20260707.01.00",
            WEB_UA,
            config,
        )
        .await?;

        if let Some(found_title) = find_playlist_title(&response) {
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

pub async fn fetch_search(query: &str, limit: u64, config: &RequestConfig) -> Result<PlaylistInfo> {
    let client = build_client(config)?;
    let body = json!({
        "context": web_context(),
        "query": query,
    });
    let response = post_innertube(
        &client,
        "search",
        &body,
        "1",
        "2.20260707.01.00",
        WEB_UA,
        config,
    )
    .await?;

    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    collect_playlist_videos(&response, &mut entries, &mut seen, limit as usize);

    Ok(PlaylistInfo {
        id: format!("search:{query}"),
        title: format!("Search: {query}"),
        entries,
    })
}

#[derive(Debug, Clone, Copy)]
enum ClientKind {
    VisionOs,
    Tv,
}

impl ClientKind {
    fn context(self) -> (Value, &'static str, &'static str) {
        match self {
            Self::VisionOs => (
                json!({
                    "clientName": "VISIONOS",
                    "clientVersion": "1.02",
                    "deviceMake": "Apple",
                    "deviceModel": "RealityDevice17,1",
                    "userAgent": VISIONOS_UA,
                    "osName": "visionOS",
                    "osVersion": "26.5.23O471",
                    "hl": "en",
                    "gl": "US",
                }),
                "101",
                "1.02",
            ),
            Self::Tv => (
                json!({
                    "clientName": "TVHTML5",
                    "clientVersion": "7.20260707.07.00",
                    "userAgent": "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)",
                    "hl": "en",
                    "gl": "US",
                }),
                "7",
                "7.20260707.07.00",
            ),
        }
    }

    fn user_agent(self) -> &'static str {
        match self {
            Self::VisionOs => VISIONOS_UA,
            Self::Tv => {
                "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)"
            }
        }
    }
}

fn web_context() -> Value {
    json!({
        "client": {
            "clientName": "WEB",
            "clientVersion": "2.20260707.01.00",
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

    let response = post_innertube(
        client,
        "player",
        &body,
        client_name,
        client_version,
        client_kind.user_agent(),
        config,
    )
    .await?;

    serde_json::from_value(response).map_err(YtrustError::from)
}

async fn post_innertube(
    client: &reqwest::Client,
    endpoint: &str,
    body: &Value,
    client_name: &str,
    client_version: &str,
    user_agent: &str,
    config: &RequestConfig,
) -> Result<Value> {
    tracing::debug!("innertube {endpoint} request: {}", body);
    let mut last_error = None;

    for attempt in 0..=2u32 {
        let mut request = client
            .post(format!(
                "https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, "https://www.youtube.com")
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header("X-YouTube-Client-Name", client_name)
            .header("X-YouTube-Client-Version", client_version)
            .json(body);

        if let Some(cookies) = &config.cookies {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        match tokio::time::timeout(Duration::from_secs(config.timeout.max(5)), request.send()).await
        {
            Ok(Ok(response)) => {
                let status = response.status();
                let text = response.text().await?;
                tracing::debug!(
                    "innertube {endpoint} HTTP {status}: {}",
                    text.chars().take(500).collect::<String>()
                );

                if status.is_success() {
                    return serde_json::from_str(&text).map_err(YtrustError::from);
                }

                let message = format!(
                    "YouTube API returned HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                );
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    last_error = Some(YtrustError::msg(message));
                } else {
                    return Err(YtrustError::msg(message));
                }
            }
            Ok(Err(err)) => {
                last_error = Some(YtrustError::from(err));
            }
            Err(_) => {
                last_error = Some(YtrustError::msg("YouTube request timed out"));
            }
        }

        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| YtrustError::msg("YouTube request failed")))
}

fn build_client(config: &RequestConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.timeout.max(5)))
        .user_agent(VISIONOS_UA);
    if let Some(proxy) = &config.proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|err| YtrustError::msg(format!("invalid proxy: {err}")))?,
        );
    }
    Ok(builder.build()?)
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

fn parse_video_info(
    requested_id: &str,
    response: PlayerResponse,
    client_kind: ClientKind,
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
            return Err(YtrustError::msg(format!("YouTube: {reason}")));
        }
    }

    let captions = response.captions;
    let microformat = response.microformat;
    let details = response
        .video_details
        .ok_or_else(|| YtrustError::msg("YouTube response has no video details"))?;
    let streaming = response
        .streaming_data
        .ok_or_else(|| YtrustError::msg("YouTube response has no streaming data"))?;

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
        if let Some(format) = parse_format(raw, client_kind) {
            formats.push(format);
        }
    }
    tracing::debug!("parsed {} formats", formats.len());

    if let Some(url) = &hls_manifest_url {
        formats.push(Format {
            itag: 0,
            url: url.clone(),
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
            is_live: details.is_live_content.unwrap_or(false),
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
        is_live: details.is_live_content.unwrap_or(false),
        thumbnails,
        subtitles,
        automatic_captions,
        formats,
        hls_manifest_url,
    })
}

fn parse_format(raw: RawFormat, client_kind: ClientKind) -> Option<Format> {
    let url = raw.url?;
    if raw.signature_cipher.is_some() || raw.cipher.is_some() {
        return None;
    }
    if url.is_empty() {
        return None;
    }

    let (container, video_codec, audio_codec, has_video, has_audio) =
        parse_mime_and_codecs(&raw.mime_type);

    Some(Format {
        itag: raw.itag,
        url,
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
        audio_bitrate: raw.audio_quality.as_ref().map(|_| raw.bitrate.unwrap_or(0)),
        content_length: raw
            .content_length
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok()),
        quality_label: raw.quality_label,
        is_hls: false,
        is_live: false,
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
    seen: &mut std::collections::BTreeSet<String>,
    limit: usize,
) {
    if entries.len() >= limit {
        return;
    }

    match value {
        Value::Object(map) => {
            if let Some(renderer) = map.get("videoRenderer")
                && let Some(id) = renderer.get("videoId").and_then(Value::as_str)
            {
                let title = renderer
                    .pointer("/title/runs/0/text")
                    .or_else(|| renderer.pointer("/title/simpleText"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let duration = renderer
                    .pointer("/lengthText/simpleText")
                    .and_then(Value::as_str)
                    .and_then(parse_duration_text);
                let thumbnails = renderer
                    .pointer("/thumbnail/thumbnails")
                    .and_then(Value::as_array)
                    .map(|items| parse_thumbnail_array(items))
                    .unwrap_or_default();
                push_playlist_entry(entries, seen, limit, id, title, duration, thumbnails);
            }
            if let Some(renderer) = map.get("playlistVideoRenderer")
                && let Some(id) = renderer.get("videoId").and_then(Value::as_str)
            {
                let title = renderer
                    .pointer("/title/runs/0/text")
                    .or_else(|| renderer.pointer("/title/simpleText"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let duration = renderer
                    .pointer("/lengthText/simpleText")
                    .and_then(Value::as_str)
                    .and_then(parse_duration_text);
                let thumbnails = renderer
                    .pointer("/thumbnail/thumbnails")
                    .and_then(Value::as_array)
                    .map(|items| parse_thumbnail_array(items))
                    .unwrap_or_default();
                push_playlist_entry(entries, seen, limit, id, title, duration, thumbnails);
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

fn push_playlist_entry(
    entries: &mut Vec<PlaylistEntry>,
    seen: &mut std::collections::BTreeSet<String>,
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
    let mut seconds = 0u64;
    for part in value.trim().split(':') {
        let part = part.trim().parse::<u64>().ok()?;
        seconds = seconds.saturating_mul(60).saturating_add(part);
    }
    (seconds > 0).then_some(seconds)
}

fn find_continuation_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(token) = map
                .get("continuationCommand")
                .and_then(|command| command.get("token"))
                .and_then(Value::as_str)
            {
                return Some(token.to_string());
            }
            for child in map.values() {
                if let Some(token) = find_continuation_token(child) {
                    return Some(token);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_continuation_token),
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
    fn parses_muxed_mime() {
        let (container, vcodec, acodec, has_video, has_audio) =
            parse_mime_and_codecs("video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"");
        assert_eq!(container, "mp4");
        assert_eq!(vcodec.as_deref(), Some("avc1.42001E"));
        assert_eq!(acodec.as_deref(), Some("mp4a.40.2"));
        assert!(has_video && has_audio);
    }
}
