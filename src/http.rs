use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha1::{Digest, Sha1};
use url::Url;

use crate::constants::{DEFAULT_TIMEOUT_SECS, WEB_UA};
use crate::error::{ErrorCode, Result, YtrustError};
use crate::util::unix_time;

#[derive(Debug, Clone)]
pub struct RequestConfig {
    pub proxy: Option<String>,
    pub cookies: Option<String>,
    pub sapisid: Option<String>,
    pub sapisid1p: Option<String>,
    pub sapisid3p: Option<String>,
    pub timeout: u64,
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            cookies: None,
            sapisid: None,
            sapisid1p: None,
            sapisid3p: None,
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }
}

type ClientCache = Mutex<HashMap<(Option<String>, u64), Client>>;

static CLIENTS: OnceLock<ClientCache> = OnceLock::new();

/// Build (or reuse) an HTTP client for YouTube API/page traffic.
///
/// The client is keyed by proxy and timeout so that long-lived Node processes
/// reuse connections across resolves instead of creating a new pool per call.
pub fn client(config: &RequestConfig) -> Result<Client> {
    let key = (config.proxy.clone(), config.timeout);
    let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(client) = cache.get(&key)
    {
        return Ok(client.clone());
    }

    let timeout = Duration::from_secs(config.timeout.max(5));
    let mut builder = Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .user_agent(WEB_UA);
    if let Some(proxy) = &config.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| {
            YtrustError::coded(ErrorCode::InvalidInput, format!("invalid proxy: {error}"))
        })?);
    }
    let client = builder.build()?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, client.clone());
    }
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
        if let Some(cookies) = &config.cookies {
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

        match tokio::time::timeout(timeout, builder.send()).await {
            Ok(Ok(response)) => {
                let status = response.status();
                let text = match tokio::time::timeout(timeout, response.text()).await {
                    Ok(Ok(text)) => text,
                    Ok(Err(error)) => {
                        last_error = Some(YtrustError::from(error));
                        if attempt < 2 {
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        }
                        continue;
                    }
                    Err(_) => {
                        last_error = Some(YtrustError::coded(
                            ErrorCode::Timeout,
                            "YouTube request timed out",
                        ));
                        if attempt < 2 {
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        }
                        continue;
                    }
                };
                tracing::debug!(
                    "innertube {} HTTP {status} ({} bytes)",
                    request.endpoint,
                    text.len()
                );

                if status.is_success() {
                    return serde_json::from_str(&text).map_err(YtrustError::from);
                }

                let error = YtrustError::coded(
                    ErrorCode::Http,
                    format!("YouTube API returned HTTP {status}"),
                );
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    last_error = Some(error);
                } else {
                    return Err(error);
                }
            }
            Ok(Err(error)) => {
                last_error = Some(YtrustError::from(error));
            }
            Err(_) => {
                last_error = Some(YtrustError::coded(
                    ErrorCode::Timeout,
                    "YouTube request timed out",
                ));
            }
        }

        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
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
        if is_youtube_host(url)
            && let Some(cookies) = &config.cookies
        {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        match tokio::time::timeout(timeout, request.send()).await {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    let error = YtrustError::coded(
                        ErrorCode::Http,
                        format!("HTTP {status} for {}", redact_url(url)),
                    );
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        last_error = Some(error);
                    } else {
                        return Err(error);
                    }
                } else {
                    match tokio::time::timeout(timeout, response.text()).await {
                        Ok(Ok(text)) => return Ok(text),
                        Ok(Err(error)) => last_error = Some(YtrustError::from(error)),
                        Err(_) => {
                            last_error = Some(YtrustError::coded(
                                ErrorCode::Timeout,
                                format!("request timed out for {}", redact_url(url)),
                            ));
                        }
                    }
                }
            }
            Ok(Err(error)) => last_error = Some(YtrustError::from(error)),
            Err(_) => {
                last_error = Some(YtrustError::coded(
                    ErrorCode::Timeout,
                    format!("request timed out for {}", redact_url(url)),
                ));
            }
        }

        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        YtrustError::coded(
            ErrorCode::Network,
            format!("request failed for {}", redact_url(url)),
        )
    }))
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
}
