use serde_json::{Value, json};

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
