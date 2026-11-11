use serde_json::Value;

use crate::constants::{DEFAULT_PLAYLIST_ITEMS, DEFAULT_PLAYLIST_LIMIT};
use crate::error::Result;
use crate::http::RequestConfig;
use crate::json;
use crate::youtube::{
    PlaylistInfo, Target, VideoInfo, classify, fetch_inline_playlist, fetch_playlist, fetch_search,
    fetch_video, filter_playlist_entries, max_playlist_item,
};

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub no_playlist: bool,
    pub playlist_limit: u64,
    pub playlist_items: String,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            no_playlist: false,
            playlist_limit: DEFAULT_PLAYLIST_LIMIT,
            playlist_items: DEFAULT_PLAYLIST_ITEMS.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum Resolved {
    Video {
        info: Box<VideoInfo>,
        original_url: String,
    },
    Playlist {
        info: PlaylistInfo,
        original_url: String,
    },
}

impl Resolved {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Video { info, original_url } => json::video_json(info, original_url),
            Self::Playlist { info, original_url } => json::playlist_json(info, original_url),
        }
    }
}

/// Shared resolve pipeline used by both the CLI and the Node addon.
pub async fn resolve(
    input: &str,
    config: &RequestConfig,
    options: &ResolveOptions,
) -> Result<Resolved> {
    let target = classify(input, options.no_playlist)?;
    match target {
        Target::Video { id } => {
            let info = fetch_video(&id, config).await?;
            Ok(Resolved::Video {
                info: Box::new(info),
                original_url: input.to_string(),
            })
        }
        Target::Playlist { id, video_id } => {
            let fetch_limit = playlist_fetch_limit(&options.playlist_items, options.playlist_limit);
            let mut playlist = match video_id {
                Some(video_id) => {
                    fetch_inline_playlist(&video_id, &id, config, fetch_limit).await?
                }
                None => fetch_playlist(&id, config, fetch_limit).await?,
            };
            filter_playlist_entries(&mut playlist, &options.playlist_items)?;
            Ok(Resolved::Playlist {
                info: playlist,
                original_url: input.to_string(),
            })
        }
        Target::Search { query, limit } => {
            let limit = limit.min(options.playlist_limit.max(1));
            let mut playlist = fetch_search(&query, limit, config).await?;
            filter_playlist_entries(&mut playlist, &options.playlist_items)?;
            Ok(Resolved::Playlist {
                info: playlist,
                original_url: input.to_string(),
            })
        }
    }
}

fn playlist_fetch_limit(items: &str, limit: u64) -> u64 {
    max_playlist_item(items)
        .map(|value| (value as u64).min(limit))
        .unwrap_or(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_playlist_fetch_limit() {
        assert_eq!(playlist_fetch_limit("1-3,5", 500), 5);
        assert_eq!(playlist_fetch_limit("1-", 500), 500);
        assert_eq!(playlist_fetch_limit("10-", 5), 5);
        assert_eq!(playlist_fetch_limit("", 50), 50);
    }
}
