use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha1::{Digest, Sha1};
use url::Url;

use crate::constants::{DEFAULT_TIMEOUT_SECS, WEB_UA};
use crate::error::{ErrorCode, Result, YtrustError};
use crate::util::unix_time;

pub(crate) const MAX_HTML_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PLAYER_JS_BYTES: usize = 32 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const CLIENT_CACHE_CAPACITY: usize = 8;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RequestConfig {
    pub proxy: Option<String>,
    pub cookies: Option<String>,
    pub cookie_jar: Option<crate::cookies::CookieJar>,
    pub sapisid: Option<String>,
    pub sapisid1p: Option<String>,
    pub sapisid3p: Option<String>,
    pub timeout: u64,
}

impl RequestConfig {
    pub fn cookies_for(&self, url: &str) -> Option<String> {
        if let Some(jar) = &self.cookie_jar {
            return jar.header_for_url(url);
        }
        if is_youtube_host(url) {
            self.cookies.clone()
        } else {
            None
        }
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            cookies: None,
            cookie_jar: None,
            sapisid: None,
            sapisid1p: None,
            sapisid3p: None,
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }
}

type ClientCache = Mutex<HashMap<(Option<String>, u64), (Instant, Client)>>;

static CLIENTS: OnceLock<ClientCache> = OnceLock::new();

// Reuse the same client for resolve and download paths instead of building a
// second connection pool.
pub fn client_for_config(proxy: Option<&str>, timeout: u64) -> Result<Client> {
    client(&RequestConfig {
        proxy: proxy.map(ToString::to_string),
        timeout,
        ..RequestConfig::default()
    })
}

/// Build (or reuse) an HTTP client for YouTube API/page traffic.
///
/// The client is keyed by proxy and effective timeout so that long-lived Node
/// processes reuse connections across resolves instead of creating a new pool
/// per call. The cache is bounded because Node callers control `timeout`.
pub fn client(config: &RequestConfig) -> Result<Client> {
    let key = (config.proxy.clone(), config.timeout.max(5));
    let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((inserted, client)) = cache.get_mut(&key) {
            *inserted = Instant::now();
            return Ok(client.clone());
        }
    }

    let timeout = Duration::from_secs(config.timeout.max(5));
    let mut builder = Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .user_agent(WEB_UA);
    if let Some(proxy) = &config.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("invalid proxy {}: {error}", redact_proxy(proxy)),
            )
        })?);
    }
    let client = builder.build()?;
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    if cache.len() >= CLIENT_CACHE_CAPACITY
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (inserted, _))| *inserted)
            .map(|(key, _)| key.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, (Instant::now(), client.clone()));
    Ok(client)
}

pub struct InnertubeRequest<'a> {
    pub endpoint: &'a str,
    pub body: &'a Value,
    pub client_name: &'a str,
    pub client_version: &'a str,
    pub user_agent: &'a str,
    pub visitor_data: Option<&'a str>,
    pub user_session_id: Option<&'a str>,
    pub browser: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn post_innertube<T: DeserializeOwned>(
    client: &Client,
    request: InnertubeRequest<'_>,
    config: &RequestConfig,
) -> Result<T> {
    let timeout = Duration::from_secs(config.timeout.max(5));
    let mut last_error = None;

    for attempt in 0..=2u32 {
        let mut builder = client
            .post(format!(
                "https://www.youtube.com/youtubei/v1/{}?prettyPrint=false",
                request.endpoint
            ))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, "https://www.youtube.com")
            .header(reqwest::header::USER_AGENT, request.user_agent)
            .header(
                reqwest::header::ACCEPT_LANGUAGE,
                if request.browser {
                    "en-us,en;q=0.5"
                } else {
                    "en-US,en;q=0.9"
                },
            )
            .header("X-YouTube-Client-Name", request.client_name)
            .header("X-YouTube-Client-Version", request.client_version);
        if request.browser {
            builder = builder
                .header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header(reqwest::header::REFERER, "https://www.youtube.com/")
                .header("Sec-Fetch-Mode", "navigate");
        }
        let mut builder = builder.json(request.body);
        if let Some(cookies) = config.cookies_for("https://www.youtube.com/youtubei/v1/") {
            builder = builder.header(reqwest::header::COOKIE, cookies);
        }
        if let Some(visitor_data) = request.visitor_data {
            builder = builder.header("X-Goog-Visitor-Id", visitor_data);
        }
        if let Some(auth) = authorization_header(config, request.user_session_id) {
            builder = builder
                .header(reqwest::header::AUTHORIZATION, auth)
                .header("X-Origin", "https://www.youtube.com")
                .header("X-Goog-AuthUser", "0")
                .header("X-Youtube-Bootstrap-Logged-In", "true");
        }

        let mut retry_after = None;
        match send_text(builder, timeout, MAX_JSON_BYTES).await {
            Ok((status, after, text)) => {
                retry_after = after;
                tracing::debug!(
                    "innertube {} HTTP {status} ({} bytes)",
                    request.endpoint,
                    text.len()
                );

                if status.is_success() {
                    if text.trim_start().starts_with('<') {
                        return Err(YtrustError::coded(
                            ErrorCode::BotCheck,
                            "YouTube returned an HTML page instead of JSON; cookies may be required",
                        ));
                    }
                    return serde_json::from_str(&text).map_err(YtrustError::from);
                }

                let error = YtrustError::coded(
                    ErrorCode::Http,
                    format!("YouTube API returned HTTP {status}"),
                );
                if retryable_status(status) {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
            Err(error) => {
                if !error.is_retryable() {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }

        if attempt < 2 {
            tokio::time::sleep(retry_delay(attempt, retry_after)).await;
        }
    }

    Err(last_error
        .unwrap_or_else(|| YtrustError::coded(ErrorCode::Network, "YouTube request failed")))
}

pub async fn fetch_text(
    client: &Client,
    url: &str,
    config: &RequestConfig,
    user_agent: &str,
    referer: Option<&str>,
) -> Result<String> {
    fetch_text_limited(client, url, config, user_agent, referer, MAX_HTML_BYTES).await
}

pub async fn fetch_text_limited(
    client: &Client,
    url: &str,
    config: &RequestConfig,
    user_agent: &str,
    referer: Option<&str>,
    limit: usize,
) -> Result<String> {
    let timeout = Duration::from_secs(config.timeout.max(5));
    let mut last_error = None;

    for attempt in 0..=2u32 {
        let mut request = client
            .get(url)
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-us,en;q=0.5")
            .header("Sec-Fetch-Mode", "navigate");
        if let Some(referer) = referer {
            request = request.header(reqwest::header::REFERER, referer);
        }
        if let Some(cookies) = config.cookies_for(url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        let mut retry_after = None;
        match send_text(request, timeout, limit).await {
            Ok((status, after, text)) => {
                retry_after = after;
                if status.is_success() {
                    return Ok(text);
                }
                let error = YtrustError::coded(
                    ErrorCode::Http,
                    format!("HTTP {status} for {}", redact_url(url)),
                );
                if retryable_status(status) {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
            Err(error) => {
                if !error.is_retryable() {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }

        if attempt < 2 {
            tokio::time::sleep(retry_delay(attempt, retry_after)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        YtrustError::coded(
            ErrorCode::Network,
            format!("request failed for {}", redact_url(url)),
        )
    }))
}

async fn send_text(
    builder: reqwest::RequestBuilder,
    timeout: Duration,
    limit: usize,
) -> Result<(StatusCode, Option<Duration>, String)> {
    tokio::time::timeout(timeout, async {
        let response = builder.send().await?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
        let strict = status.is_success();
        let limit = if strict { limit } else { MAX_ERROR_BODY_BYTES };
        let text = read_body(response, limit, strict).await?;
        Ok((status, retry_after, text))
    })
    .await
    .map_err(|_| YtrustError::coded(ErrorCode::Timeout, "YouTube request timed out"))?
}

async fn read_body(response: reqwest::Response, limit: usize, strict: bool) -> Result<String> {
    if let Some(length) = response.content_length()
        && length > limit as u64
        && strict
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidResponse,
            format!("response is too large ({length} bytes)"),
        ));
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(1024 * 1024) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > limit {
            if strict {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidResponse,
                    format!("response exceeded {limit} bytes"),
                ));
            }
            let remaining = limit.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    if strict {
        String::from_utf8(body).map_err(|_| {
            YtrustError::coded(ErrorCode::InvalidResponse, "response was not valid UTF-8")
        })
    } else {
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_EARLY
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

const WARMUP_ATTEMPTS: u32 = 5;
const WARMUP_BASE_DELAY_MS: u64 = 500;

/// Probe a googlevideo media URL until its edge has warmed up.
///
/// Freshly resolved URLs are redirected to an edge that temporarily answers
/// `403` while it fetches the stream. The same original URL starts returning
/// `2xx` after a short pause, so callers that expose URLs (for example the Node
/// addon) can use this to guarantee that consumers will not hit the cold edge.
pub async fn warm_up_media_url(
    config: &RequestConfig,
    raw_url: &str,
    user_agent: Option<&str>,
    referer: Option<&str>,
) -> Result<()> {
    let url = Url::parse(raw_url)
        .map_err(|_| YtrustError::coded(ErrorCode::InvalidInput, "invalid media URL"))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url.scheme() != "https" || !(host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "media URL must use https on a googlevideo host",
        ));
    }

    let client = client(config)?;
    let user_agent = user_agent.unwrap_or(WEB_UA);
    let referer = referer.unwrap_or("https://www.youtube.com/");
    let mut last_error = None;

    for attempt in 0..WARMUP_ATTEMPTS {
        if attempt > 0 {
            let delay = Duration::from_millis(WARMUP_BASE_DELAY_MS << (attempt - 1).min(3));
            tokio::time::sleep(delay).await;
        }

        let mut request = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::REFERER, referer)
            .header(reqwest::header::RANGE, "bytes=0-0");
        if let Some(cookies) = config.cookies_for(raw_url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                // Do not read the body: a server that ignores Range could send
                // the whole media file. Dropping the response cancels it.
                drop(response);
                return Ok(());
            }
            Ok(response) => {
                let status = response.status();
                let retryable = status == StatusCode::FORBIDDEN
                    || status == StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error();
                last_error = Some(YtrustError::HttpStatus {
                    status: status.as_u16(),
                    message: format!("HTTP {status} for {}", redact_url(raw_url)),
                });
                if !retryable {
                    break;
                }
            }
            Err(error) => {
                last_error = Some(error.into());
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        YtrustError::coded(ErrorCode::Network, "media URL did not become reachable")
    }))
}

pub(crate) fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(retry_after) = retry_after {
        return retry_after;
    }
    let base = 1u64 << attempt.min(4);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos() % 250))
        .unwrap_or(0);
    Duration::from_millis(base * 1000 + jitter)
}

fn redact_proxy(proxy: &str) -> String {
    let Ok(mut url) = Url::parse(proxy) else {
        return "<invalid proxy>".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

pub fn authorization_header(
    config: &RequestConfig,
    user_session_id: Option<&str>,
) -> Option<String> {
    let timestamp = unix_time();
    let origin = "https://www.youtube.com";
    let extra = user_session_id
        .map(|id| format!("{id} "))
        .unwrap_or_default();
    let suffix = if user_session_id.is_some() { "_u" } else { "" };
    let hash = |sid: &str| {
        let mut hasher = Sha1::new();
        hasher.update(format!("{extra}{timestamp} {sid} {origin}"));
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let mut parts = Vec::new();
    if let Some(sid) = &config.sapisid {
        parts.push(format!("SAPISIDHASH {timestamp}_{}{suffix}", hash(sid)));
    }
    if let Some(sid) = &config.sapisid1p {
        parts.push(format!("SAPISID1PHASH {timestamp}_{}{suffix}", hash(sid)));
    }
    if let Some(sid) = &config.sapisid3p {
        parts.push(format!("SAPISID3PHASH {timestamp}_{}{suffix}", hash(sid)));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

pub fn is_youtube_host(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    host == "youtube.com"
        || host == "www.youtube.com"
        || host == "m.youtube.com"
        || host == "music.youtube.com"
        || host == "youtube-nocookie.com"
        || host == "www.youtube-nocookie.com"
}

pub fn redact_url(url: &str) -> String {
    let Ok(mut url) = Url::parse(url) else {
        return "<invalid URL>".to_string();
    };
    if url.query().is_some() {
        url.set_query(Some("<redacted>"));
    }
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_youtube_hosts() {
        assert!(is_youtube_host("https://www.youtube.com/watch?v=x"));
        assert!(is_youtube_host("https://music.youtube.com/watch?v=x"));
        assert!(!is_youtube_host(
            "https://rr1---sn.googlevideo.com/videoplayback"
        ));
    }

    #[test]
    fn builds_sapisid_authorization_without_session_id() {
        let config = RequestConfig {
            sapisid: Some("sid".to_string()),
            ..RequestConfig::default()
        };
        let header = authorization_header(&config, None).unwrap();
        assert!(header.starts_with("SAPISIDHASH "));
        assert!(!header.ends_with('_'));
        assert!(!header.ends_with("_u"));
    }

    #[test]
    fn redacts_query_strings() {
        let redacted = redact_url("https://rr1.googlevideo.com/videoplayback?sig=secret&expire=1");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("expire=1"));
    }

    #[tokio::test]
    async fn warm_up_rejects_non_googlevideo_urls() {
        let config = RequestConfig::default();
        let error = warm_up_media_url(&config, "https://example.com/video", None, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        let error = warm_up_media_url(&config, "http://rr1.googlevideo.com/video", None, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidInput);
    }
}
