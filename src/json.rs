use serde_json::{Value, json};

use crate::format::best_audio;
use crate::output::format_ext;
use crate::youtube::{Format, PlaylistEntry, PlaylistInfo, SubtitleTrack, Thumbnail, VideoInfo};

pub fn video_json(info: &VideoInfo, original_url: &str) -> Value {
    let thumbnails = thumbnails_json(&info.thumbnails);
    json!({
        "_type": "video",
        "id": info.id,
        "title": info.title,
        "description": info.description,
        "duration": info.duration_secs,
        "thumbnail": info.thumbnails.last().map(|thumb| thumb.url.clone()),
        "thumbnails": thumbnails,
        "webpage_url": info.webpage_url,
        "original_url": original_url,
        "uploader": info.uploader,
        "channel": info.channel,
        "channel_id": info.channel_id,
        "upload_date": info.upload_date,
        "view_count": info.view_count,
        "is_live": info.is_live,
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "epoch": unix_time(),
        "formats": info.formats.iter().map(format_json).collect::<Vec<_>>(),
        "best_audio": best_audio(info).map(format_json),
        "player_js_url": info.player_js_url,
        "subtitles": subtitles_json(&info.subtitles),
        "automatic_captions": subtitles_json(&info.automatic_captions),
    })
}

pub fn playlist_json(playlist: &PlaylistInfo, original_url: &str) -> Value {
    json!({
        "_type": "playlist",
        "id": playlist.id,
        "title": playlist.title,
        "webpage_url": original_url,
        "original_url": original_url,
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "epoch": unix_time(),
        "playlist_count": playlist.entries.len(),
        "entries": playlist.entries.iter().map(entry_json).collect::<Vec<_>>(),
    })
}

fn entry_json(entry: &PlaylistEntry) -> Value {
    let webpage_url = format!("https://www.youtube.com/watch?v={}", entry.id);
    json!({
        "_type": "url",
        "id": entry.id,
        "url": webpage_url,
        "webpage_url": webpage_url,
        "title": entry.title,
        "duration": entry.duration,
        "thumbnail": entry.thumbnails.last().map(|thumb| thumb.url.clone()),
        "thumbnails": thumbnails_json(&entry.thumbnails),
    })
}

fn format_json(format: &Format) -> Value {
    let ext = format_ext(format);
    let resolution = match (format.width, format.height) {
        (Some(width), Some(height)) => Some(format!("{width}x{height}")),
        (None, Some(height)) => Some(format!("{height}p")),
        _ => None,
    };
    let mut value = json!({
        "format_id": format.itag.to_string(),
        "format": format.itag.to_string(),
        "url": format.url,
        "ext": ext,
        "protocol": if format.is_hls { "m3u8_native" } else { "https" },
        "vcodec": format.video_codec.clone().unwrap_or_else(|| "none".to_string()),
        "acodec": format.audio_codec.clone().unwrap_or_else(|| "none".to_string()),
        "width": format.width,
        "height": format.height,
        "fps": format.fps,
        "tbr": format.bitrate / 1_000,
        "filesize": format.content_length,
        "resolution": resolution,
        "format_note": format.quality_label,
        "is_live": format.is_live,
        "http_headers": http_headers(format),
        "expires_at": format_expires_at(&format.url),
    });

    if let Some(abr) = format.audio_bitrate {
        value["abr"] = json!(abr / 1_000);
    }
    if format.audio_track_name.is_some() || format.audio_track_id.is_some() {
        value["audio_track"] = json!({
            "id": format.audio_track_id,
            "display_name": format.audio_track_name,
            "audio_is_default": format.audio_is_default,
        });
    }

    value
}

fn http_headers(format: &Format) -> Value {
    json!({
        "User-Agent": format.user_agent,
        "Referer": "https://www.youtube.com/",
    })
}

/// `expire` in a googlevideo URL is a Unix timestamp. Clients can use it to
/// refresh a resolved media URL before it starts returning HTTP 403.
fn format_expires_at(raw: &str) -> Option<u64> {
    let url = url::Url::parse(raw).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "expire")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

fn thumbnails_json(thumbnails: &[Thumbnail]) -> Vec<Value> {
    thumbnails
        .iter()
        .map(|thumb| {
            json!({
                "url": thumb.url,
                "width": thumb.width,
                "height": thumb.height,
            })
        })
        .collect()
}

fn subtitles_json(tracks: &[SubtitleTrack]) -> Value {
    let mut result = serde_json::Map::new();
    for track in tracks {
        let entry = json!({
            "ext": "vtt",
            "url": track.url,
            "name": track.name,
        });
        result
            .entry(track.language.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("subtitle entry is an array")
            .push(entry);
    }
    Value::Object(result)
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(
        itag: u64,
        audio_codec: Option<&str>,
        has_video: bool,
        bitrate: u64,
        audio_bitrate: Option<u64>,
    ) -> Format {
        Format {
            itag,
            url: format!(
                "https://rr1---sn-test.googlevideo.com/videoplayback?itag={itag}&expire=1794413769"
            ),
            container: if audio_codec == Some("opus") {
                "webm".to_string()
            } else {
                "mp4".to_string()
            },
            mime_type: "audio/mp4".to_string(),
            video_codec: has_video.then(|| "avc1.640028".to_string()),
            audio_codec: audio_codec.map(str::to_string),
            has_video,
            has_audio: audio_codec.is_some(),
            width: has_video.then_some(1920),
            height: has_video.then_some(1080),
            fps: has_video.then_some(30),
            bitrate,
            audio_bitrate,
            content_length: Some(1_000_000),
            quality_label: None,
            is_hls: false,
            is_live: false,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: false,
            user_agent: "test-agent".to_string(),
        }
    }

    fn video_info(formats: Vec<Format>) -> VideoInfo {
        VideoInfo {
            id: "dQw4w9WgXcQ".to_string(),
            title: "test".to_string(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            webpage_url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            description: String::new(),
            duration_secs: Some(213),
            view_count: None,
            upload_date: String::new(),
            is_live: false,
            thumbnails: Vec::new(),
            subtitles: Vec::new(),
            automatic_captions: Vec::new(),
            formats,
            hls_manifest_url: None,
            player_js_url: None,
        }
    }

    #[test]
    fn video_json_exposes_audio_headers_and_expiry() {
        let info = video_info(vec![
            format(18, Some("mp4a.40.2"), true, 500_000, None),
            format(140, Some("mp4a.40.2"), false, 130_000, Some(130_000)),
            format(251, Some("opus"), false, 136_000, Some(136_000)),
        ]);

        let value = video_json(&info, &info.webpage_url);
        assert_eq!(value["best_audio"]["format_id"], "251");
        assert_eq!(value["best_audio"]["acodec"], "opus");
        assert_eq!(
            value["best_audio"]["http_headers"]["User-Agent"],
            "test-agent"
        );
        assert_eq!(
            value["best_audio"]["http_headers"]["Referer"],
            "https://www.youtube.com/"
        );
        assert_eq!(value["best_audio"]["expires_at"], 1_794_413_769);
        assert_eq!(
            value["formats"][0]["http_headers"]["Referer"],
            "https://www.youtube.com/"
        );
    }

    #[test]
    fn video_json_has_no_best_audio_for_video_only_formats() {
        let info = video_info(vec![format(137, None, true, 4_000_000, None)]);
        let value = video_json(&info, &info.webpage_url);
        assert!(value["best_audio"].is_null());
    }

    #[test]
    fn format_expires_at_reads_googlevideo_expire() {
        assert_eq!(
            format_expires_at("https://x.googlevideo.com/videoplayback?expire=42&itag=251"),
            Some(42)
        );
        assert_eq!(
            format_expires_at("https://x.googlevideo.com/no-expire"),
            None
        );
        assert_eq!(format_expires_at("not a url"), None);
    }
}
