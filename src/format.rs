use std::cmp::Ordering;

use crate::youtube::{Format, VideoInfo};

use crate::error::{ErrorCode, Result, YtrustError};
use crate::output::{format_bytes, format_duration, format_ext};

#[derive(Debug, Clone)]
pub struct FormatSelection {
    pub video: Option<Format>,
    pub audio: Option<Format>,
}

impl FormatSelection {
    pub fn single(&self) -> Option<&Format> {
        if self.audio.is_none() {
            self.video.as_ref()
        } else if self.video.is_none() {
            self.audio.as_ref()
        } else {
            None
        }
    }

    pub fn needs_merge(&self) -> bool {
        self.video.is_some() && self.audio.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Best,
    BestVideo,
    BestVideoAny,
    BestAudio,
    Worst,
    WorstVideo,
    WorstAudio,
    BestExt,
    Exact,
}

#[derive(Debug, Clone, Default)]
struct Filter {
    height_lte: Option<u64>,
    height_gte: Option<u64>,
    width_lte: Option<u64>,
    width_gte: Option<u64>,
    fps_lte: Option<u64>,
    fps_gte: Option<u64>,
    ext: Option<String>,
    ext_not: Option<String>,
    vcodec: Option<String>,
    vcodec_not: Option<String>,
    vcodec_prefix: Option<String>,
    acodec: Option<String>,
    acodec_not: Option<String>,
    acodec_prefix: Option<String>,
}

impl Filter {
    fn matches(&self, value: &Format) -> bool {
        if value.url.is_empty() {
            return false;
        }
        if let Some(limit) = self.height_lte
            && value.height.unwrap_or(0) > limit
        {
            return false;
        }
        if let Some(limit) = self.height_gte
            && value.height.unwrap_or(0) < limit
        {
            return false;
        }
        if let Some(limit) = self.width_lte
            && value.width.unwrap_or(0) > limit
        {
            return false;
        }
        if let Some(limit) = self.width_gte
            && value.width.unwrap_or(0) < limit
        {
            return false;
        }
        if let Some(limit) = self.fps_lte
            && value.fps.unwrap_or(0) > limit
        {
            return false;
        }
        if let Some(limit) = self.fps_gte
            && value.fps.unwrap_or(0) < limit
        {
            return false;
        }

        let ext = format_ext(value);
        if let Some(expected) = &self.ext
            && !ext_matches(ext, expected)
        {
            return false;
        }
        if let Some(unexpected) = &self.ext_not
            && ext_matches(ext, unexpected)
        {
            return false;
        }

        let vcodec = value
            .video_codec
            .as_deref()
            .unwrap_or("none")
            .to_lowercase();
        let acodec = value
            .audio_codec
            .as_deref()
            .unwrap_or("none")
            .to_lowercase();
        if let Some(expected) = &self.vcodec
            && !vcodec.contains(expected)
        {
            return false;
        }
        if let Some(unexpected) = &self.vcodec_not
            && vcodec.contains(unexpected)
        {
            return false;
        }
        if let Some(prefix) = &self.vcodec_prefix
            && !vcodec.starts_with(prefix)
        {
            return false;
        }
        if let Some(expected) = &self.acodec
            && !acodec.contains(expected)
        {
            return false;
        }
        if let Some(unexpected) = &self.acodec_not
            && acodec.contains(unexpected)
        {
            return false;
        }
        if let Some(prefix) = &self.acodec_prefix
            && !acodec.starts_with(prefix)
        {
            return false;
        }

        true
    }
}

fn ext_matches(actual: &str, expected: &str) -> bool {
    let expected = expected.trim_start_matches('.').to_ascii_lowercase();
    if actual.eq_ignore_ascii_case(&expected) {
        return true;
    }
    matches!(
        (actual, expected.as_str()),
        ("m4a", "mp4") | ("mp4", "m4a") | ("webm", "opus") | ("mka", "m4a")
    )
}

#[derive(Debug, Clone)]
struct Component {
    kind: Kind,
    filter: Filter,
    argument: String,
}

impl Component {
    fn parse(raw: &str) -> Result<Self> {
        let (base, filter) = parse_filter(raw)?;
        let base = base.trim().to_ascii_lowercase();
        let (kind, argument) = match base.as_str() {
            "b" | "best" | "best*" => (Kind::Best, base.clone()),
            "bv" | "bestvideo" => (Kind::BestVideo, base.clone()),
            "bv*" | "bestvideo*" => (Kind::BestVideoAny, base.clone()),
            "ba" | "bestaudio" | "audio" => (Kind::BestAudio, base.clone()),
            "w" | "worst" => (Kind::Worst, base.clone()),
            "wv" | "worstvideo" => (Kind::WorstVideo, base.clone()),
            "wa" | "worstaudio" => (Kind::WorstAudio, base.clone()),
            _ if base.chars().all(|c| c.is_ascii_digit()) => (Kind::Exact, base.clone()),
            _ if is_known_ext(&base) => (Kind::BestExt, base.clone()),
            _ => {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("unsupported format selector component: {raw:?}"),
                ));
            }
        };

        Ok(Self {
            kind,
            filter,
            argument,
        })
    }
}

fn is_known_ext(value: &str) -> bool {
    matches!(
        value,
        "mp4" | "webm" | "m4a" | "mp3" | "opus" | "flac" | "wav" | "3gp" | "mkv" | "ogg"
    )
}

fn parse_filter(raw: &str) -> Result<(&str, Filter)> {
    let mut filter = Filter::default();
    let base = raw.find('[').map(|index| &raw[..index]).unwrap_or(raw);

    let mut rest = &raw[base.len()..];
    while let Some(start) = rest.find('[') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(']') else {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("unclosed format filter in {raw:?}"),
            ));
        };
        apply_filter(&mut filter, &after[..end])?;
        rest = &after[end + 1..];
    }

    Ok((base, filter))
}

fn apply_filter(filter: &mut Filter, expression: &str) -> Result<()> {
    let (name, op, value) = if let Some((name, value)) = expression.split_once("<=") {
        (name.trim(), "<=", value.trim())
    } else if let Some((name, value)) = expression.split_once(">=") {
        (name.trim(), ">=", value.trim())
    } else if let Some((name, value)) = expression.split_once("!=") {
        (name.trim(), "!=", value.trim())
    } else if let Some((name, value)) = expression.split_once("^=") {
        (name.trim(), "^=", value.trim())
    } else if let Some((name, value)) = expression.split_once('=') {
        (name.trim(), "=", value.trim())
    } else if let Some((name, value)) = expression.split_once('~') {
        (name.trim(), "~", value.trim())
    } else {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("unsupported format filter: [{expression}]"),
        ));
    };

    let number = || {
        value
            .trim_end_matches(['k', 'K', 'm', 'M'])
            .parse::<u64>()
            .map_err(|_| {
                YtrustError::coded(
                    ErrorCode::InvalidInput,
                    format!("invalid numeric filter value: {value}"),
                )
            })
    };

    match (name, op) {
        ("height", "<=") => filter.height_lte = Some(number()?),
        ("height", ">=") => filter.height_gte = Some(number()?),
        ("width", "<=") => filter.width_lte = Some(number()?),
        ("width", ">=") => filter.width_gte = Some(number()?),
        ("fps", "<=") => filter.fps_lte = Some(number()?),
        ("fps", ">=") => filter.fps_gte = Some(number()?),
        ("ext", "=") => filter.ext = Some(value.to_ascii_lowercase()),
        ("ext", "!=") => filter.ext_not = Some(value.to_ascii_lowercase()),
        ("vcodec", "=") | ("vcodec", "~") => filter.vcodec = Some(value.to_ascii_lowercase()),
        ("vcodec", "!=") => filter.vcodec_not = Some(value.to_ascii_lowercase()),
        ("vcodec", "^=") => filter.vcodec_prefix = Some(value.to_ascii_lowercase()),
        ("acodec", "=") | ("acodec", "~") => filter.acodec = Some(value.to_ascii_lowercase()),
        ("acodec", "!=") => filter.acodec_not = Some(value.to_ascii_lowercase()),
        ("acodec", "^=") => filter.acodec_prefix = Some(value.to_ascii_lowercase()),
        _ => {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("unsupported format filter: [{expression}]"),
            ));
        }
    }

    Ok(())
}

pub fn select(info: &VideoInfo, expression: &str) -> Result<FormatSelection> {
    let expression = if expression.trim().is_empty() {
        "bestvideo+bestaudio/best"
    } else {
        expression.trim()
    };

    let mut last_error = None;
    for alternative in expression.split('/') {
        match select_alternative(&info.formats, alternative.trim()) {
            Ok(selection) => return Ok(selection),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        YtrustError::FormatNotFound(format!("no format matched {expression:?}"))
    }))
}

fn select_alternative(formats: &[Format], alternative: &str) -> Result<FormatSelection> {
    let mut parts = alternative
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let first = parts
        .next()
        .ok_or_else(|| YtrustError::coded(ErrorCode::InvalidInput, "empty format selector"))?;
    let second = parts.next();

    if parts.next().is_some() {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("too many components in format selector: {alternative:?}"),
        ));
    }

    let first = Component::parse(first)?;
    if let Some(second) = second {
        let second = Component::parse(second)?;
        let (video_component, audio_component) = if matches!(
            first.kind,
            Kind::BestVideo | Kind::BestVideoAny | Kind::WorstVideo
        ) {
            (first, second)
        } else if matches!(
            second.kind,
            Kind::BestVideo | Kind::BestVideoAny | Kind::WorstVideo
        ) {
            (second, first)
        } else {
            (first, second)
        };

        let video = choose_video(formats, &video_component)?;
        let audio = choose_audio(formats, &audio_component)?;
        return Ok(FormatSelection {
            video: Some(video),
            audio: Some(audio),
        });
    }

    match first.kind {
        Kind::BestAudio | Kind::WorstAudio => Ok(FormatSelection {
            video: None,
            audio: Some(choose_audio(formats, &first)?),
        }),
        Kind::BestVideo | Kind::BestVideoAny | Kind::WorstVideo => Ok(FormatSelection {
            video: Some(choose_video(formats, &first)?),
            audio: None,
        }),
        Kind::Best | Kind::Worst | Kind::BestExt | Kind::Exact => {
            let format = choose_single(formats, &first)?;
            if is_audio_only(&format) {
                Ok(FormatSelection {
                    video: None,
                    audio: Some(format),
                })
            } else {
                Ok(FormatSelection {
                    video: Some(format),
                    audio: None,
                })
            }
        }
    }
}

fn choose_single(formats: &[Format], component: &Component) -> Result<Format> {
    let mut candidates: Vec<&Format> = formats
        .iter()
        .filter(|format| component.filter.matches(format) && format_matches_kind(format, component))
        .collect();

    if candidates.is_empty() {
        return Err(YtrustError::FormatNotFound(component.argument.clone()));
    }

    candidates.sort_by(|a, b| compare_video(a, b));
    let selected = if matches!(component.kind, Kind::Worst) {
        candidates.first()
    } else {
        candidates.last()
    };

    Ok((*selected.expect("non-empty candidate list")).clone())
}

fn choose_video(formats: &[Format], component: &Component) -> Result<Format> {
    let mut candidates: Vec<&Format> = formats
        .iter()
        .filter(|format| {
            format.has_video
                && component.filter.matches(format)
                && format_matches_video_kind(format, component)
        })
        .collect();

    if candidates.is_empty() {
        return Err(YtrustError::FormatNotFound(format!(
            "{} (video)",
            component.argument
        )));
    }

    candidates.sort_by(|a, b| compare_video(a, b));
    let selected = if matches!(component.kind, Kind::WorstVideo) {
        candidates.first()
    } else {
        candidates.last()
    };

    Ok((*selected.expect("non-empty candidate list")).clone())
}

fn choose_audio(formats: &[Format], component: &Component) -> Result<Format> {
    let mut candidates: Vec<&Format> = formats
        .iter()
        .filter(|format| {
            format.has_audio
                && !format.has_video
                && component.filter.matches(format)
                && format_matches_audio_kind(format, component)
        })
        .collect();

    if candidates.is_empty() {
        return Err(YtrustError::FormatNotFound(format!(
            "{} (audio)",
            component.argument
        )));
    }

    candidates.sort_by(|a, b| compare_audio(a, b));
    let selected = if matches!(component.kind, Kind::WorstAudio) {
        candidates.first()
    } else {
        candidates.last()
    };

    Ok((*selected.expect("non-empty candidate list")).clone())
}

fn format_matches_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::Exact => format.itag.to_string() == component.argument,
        Kind::BestExt if is_audio_ext(&component.argument) => is_audio_only(format),
        Kind::BestExt => {
            format.has_video
                && format.has_audio
                && ext_matches(format_ext(format), &component.argument)
        }
        Kind::Best | Kind::Worst => format.has_video && format.has_audio,
        _ => true,
    }
}

fn is_audio_ext(value: &str) -> bool {
    matches!(value, "m4a" | "mp3" | "opus" | "flac" | "wav" | "ogg")
}

fn format_matches_video_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::BestVideo => is_video_only(format),
        Kind::BestVideoAny => true,
        Kind::WorstVideo => true,
        Kind::Exact => format.itag.to_string() == component.argument,
        Kind::BestExt => ext_matches(format_ext(format), &component.argument),
        _ => true,
    }
}

fn format_matches_audio_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::Exact => format.itag.to_string() == component.argument,
        Kind::BestExt => ext_matches(format_ext(format), &component.argument),
        _ => true,
    }
}

/// Pick the best playable audio stream: the default audio track first,
/// then audio-only over muxed, then Opus, then bitrate. HLS manifests are
/// excluded from the first pass because callers cannot seek in them like a
/// regular media URL.
pub fn best_audio(info: &VideoInfo) -> Option<&Format> {
    let rate = |format: &Format| format.audio_bitrate.unwrap_or(format.bitrate);
    let key = |format: &Format| {
        (
            u8::from(!format.has_video),
            u8::from(format.audio_is_default),
            u8::from(is_opus(format)),
            rate(format),
            u8::from(!format.url.is_empty()),
        )
    };
    info.formats
        .iter()
        .filter(|format| format.has_audio && !format.is_hls)
        .max_by_key(|format| key(format))
        .or_else(|| {
            info.formats
                .iter()
                .filter(|format| format.has_audio)
                .max_by_key(|format| key(format))
        })
}

fn is_opus(format: &Format) -> bool {
    format
        .audio_codec
        .as_deref()
        .is_some_and(|codec| codec.to_ascii_lowercase().starts_with("opus"))
}

fn is_audio_only(format: &Format) -> bool {
    format.has_audio && !format.has_video
}

fn is_video_only(format: &Format) -> bool {
    format.has_video && !format.has_audio
}

fn compare_video(a: &Format, b: &Format) -> Ordering {
    (a.height.unwrap_or(0), a.fps.unwrap_or(0), a.bitrate).cmp(&(
        b.height.unwrap_or(0),
        b.fps.unwrap_or(0),
        b.bitrate,
    ))
}

fn compare_audio(a: &Format, b: &Format) -> Ordering {
    (
        a.audio_is_default,
        a.audio_bitrate.unwrap_or(0),
        a.bitrate,
        a.content_length.unwrap_or(0),
    )
        .cmp(&(
            b.audio_is_default,
            b.audio_bitrate.unwrap_or(0),
            b.bitrate,
            b.content_length.unwrap_or(0),
        ))
}

pub fn print_formats(info: &VideoInfo) {
    println!(
        "{:<6} {:<6} {:<12} {:<6} {:<14} {:<14} {:>10}  NOTE",
        "ID", "EXT", "RESOLUTION", "FPS", "VCODEC", "ACODEC", "SIZE"
    );
    for format in &info.formats {
        let resolution = match (format.width, format.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            (None, Some(h)) => format!("{h}p"),
            _ => String::from("audio"),
        };
        let fps = format.fps.map(|v| v.to_string()).unwrap_or_default();
        let size = format
            .content_length
            .map(format_bytes)
            .unwrap_or_else(|| "?".to_string());
        let note = if format.is_hls {
            "HLS"
        } else if format.is_live {
            "LIVE"
        } else if !format.has_audio {
            "video only"
        } else if !format.has_video {
            "audio only"
        } else {
            ""
        };
        println!(
            "{:<6} {:<6} {:<12} {:<6} {:<14} {:<14} {:>10}  {}",
            format.itag,
            format_ext(format),
            resolution,
            fps,
            format.video_codec.as_deref().unwrap_or("none"),
            format.audio_codec.as_deref().unwrap_or("none"),
            size,
            note
        );
    }

    let duration = info
        .duration_secs
        .map(format_duration)
        .unwrap_or_else(|| "?".to_string());
    println!("\nDuration: {duration}");
    if let Some(hls) = &info.hls_manifest_url {
        println!("HLS manifest: {hls}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_height_filter() {
        let (base, filter) = parse_filter("bestvideo[height<=1080]").unwrap();
        assert_eq!(base, "bestvideo");
        assert_eq!(filter.height_lte, Some(1080));
    }

    fn format(
        itag: u64,
        has_video: bool,
        has_audio: bool,
        height: Option<u64>,
        audio_bitrate: Option<u64>,
    ) -> Format {
        Format {
            itag,
            url: format!("https://rr1.googlevideo.com/videoplayback?itag={itag}"),
            signature_cipher: None,
            cipher: None,
            container: if has_video { "mp4" } else { "webm" }.to_string(),
            mime_type: "video/mp4".to_string(),
            video_codec: has_video.then(|| "avc1".to_string()),
            audio_codec: has_audio.then(|| "opus".to_string()),
            has_video,
            has_audio,
            width: height.map(|height| height * 16 / 9),
            height,
            fps: has_video.then_some(30),
            bitrate: 1_000_000,
            audio_bitrate,
            content_length: Some(1_000),
            quality_label: None,
            is_hls: false,
            is_live: false,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: false,
            user_agent: "test".to_string(),
        }
    }

    fn info(formats: Vec<Format>) -> VideoInfo {
        VideoInfo {
            id: "dQw4w9WgXcQ".to_string(),
            title: "test".to_string(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            webpage_url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            description: String::new(),
            duration_secs: None,
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
    fn falls_back_to_muxed_audio() {
        let info = info(vec![format(18, true, true, Some(360), None)]);
        let selection = select(&info, "bestaudio/best").unwrap();
        assert!(!selection.needs_merge());
        assert_eq!(selection.single().unwrap().itag, 18);
    }

    #[test]
    fn selects_separate_streams() {
        let info = info(vec![
            format(137, true, false, Some(1080), None),
            format(251, false, true, None, Some(160_000)),
        ]);
        let selection = select(&info, "bestvideo+bestaudio").unwrap();
        assert!(selection.needs_merge());
        assert_eq!(selection.video.unwrap().itag, 137);
        assert_eq!(selection.audio.unwrap().itag, 251);
    }

    #[test]
    fn selects_exact_itag() {
        let info = info(vec![
            format(137, true, false, Some(1080), None),
            format(251, false, true, None, Some(160_000)),
        ]);
        let selection = select(&info, "251").unwrap();
        assert_eq!(selection.single().unwrap().itag, 251);
    }

    #[test]
    fn parses_codec_filter() {
        let (_, filter) = parse_filter("bestvideo[vcodec!=none]").unwrap();
        assert_eq!(filter.vcodec_not.as_deref(), Some("none"));
    }
}
