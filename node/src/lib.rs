use napi::bindgen_prelude::Result;
use napi_derive::napi;
use ytrust::cookies;
use ytrust::json;
use ytrust::youtube::{
    RequestConfig, Target, classify, fetch_playlist, fetch_search, fetch_video,
    filter_playlist_entries, max_playlist_item,
};

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
            timeout: Some(30),
            playlist_limit: Some(500),
            playlist_items: Some("1-".to_string()),
            no_playlist: Some(false),
        }
    }
}

#[napi]
pub fn probe() -> String {
    ytrust::version().to_string()
}

#[napi]
pub fn version() -> String {
    ytrust::version().to_string()
}

#[napi]
pub async fn resolve(input: String, options: Option<ResolveOptions>) -> Result<String> {
    resolve_inner(input, options.unwrap_or_default())
        .await
        .map_err(|err| napi::Error::from_reason(err.to_string()))
}

async fn resolve_inner(input: String, options: ResolveOptions) -> ytrust::error::Result<String> {
    let cookie_header = cookies::load(
        options.cookies_path.as_deref().map(std::path::Path::new),
        options.cookies_from_browser.as_deref(),
    )?;
    let config = RequestConfig {
        proxy: options.proxy.clone(),
        cookies: cookie_header,
        timeout: u64::from(options.timeout.unwrap_or(30)),
    };
    let playlist_limit = u64::from(options.playlist_limit.unwrap_or(500));
    let playlist_items = options
        .playlist_items
        .clone()
        .unwrap_or_else(|| "1-".to_string());

    let target = classify(&input, options.no_playlist.unwrap_or(false))?;
    let value = match target {
        Target::Video { id } => {
            let info = fetch_video(&id, &config).await?;
            json::video_json(&info, &info.webpage_url)
        }
        Target::Playlist { id } => {
            let fetch_limit = max_playlist_item(&playlist_items)
                .map(|limit| (limit as u64).min(playlist_limit))
                .unwrap_or(playlist_limit);
            let mut playlist = fetch_playlist(&id, &config, fetch_limit).await?;
            filter_playlist_entries(&mut playlist, &playlist_items)?;
            json::playlist_json(&playlist, &input)
        }
        Target::Search { query, limit } => {
            let mut playlist = fetch_search(&query, limit, &config).await?;
            filter_playlist_entries(&mut playlist, &playlist_items)?;
            json::playlist_json(&playlist, &input)
        }
    };

    Ok(serde_json::to_string(&value)?)
}
