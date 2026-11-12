use std::cmp::Ordering;

use crate::youtube::{Format, VideoInfo};

use crate::error::{ErrorCode, Result, YtrustError};
use crate::output::{format_bytes, format_duration, format_ext};

#[derive(Debug, Clone)]
pub struct FormatSelection {
    pub video: Option<Format>,
    pub audios: Vec<Format>,
}

impl FormatSelection {
    pub fn single(&self) -> Option<&Format> {
        match (self.video.as_ref(), self.audios.as_slice()) {
            (Some(video), []) => Some(video),
            (None, [audio]) => Some(audio),
            _ => None,
        }
    }

    pub fn needs_merge(&self) -> bool {
        self.video.is_some() && !self.audios.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Best,
    BestAny,
    BestVideo,
    BestVideoAny,
    BestAudio,
    BestAudioAny,
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
    language: Option<String>,
    language_not: Option<String>,
    language_prefix: Option<String>,
    language_contains: Option<String>,
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

        let language = value.language.as_deref().unwrap_or("none").to_lowercase();
        if let Some(expected) = &self.language
            && language != *expected
        {
            return false;
        }
        if let Some(unexpected) = &self.language_not
            && language == *unexpected
        {
            return false;
        }
        if let Some(prefix) = &self.language_prefix
            && !language.starts_with(prefix)
        {
            return false;
        }
        if let Some(fragment) = &self.language_contains
            && !language.contains(fragment)
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
            "b" | "best" => (Kind::Best, base.clone()),
            "b*" | "best*" => (Kind::BestAny, base.clone()),
            "bv" | "bestvideo" => (Kind::BestVideo, base.clone()),
            "bv*" | "bestvideo*" => (Kind::BestVideoAny, base.clone()),
            "ba" | "bestaudio" | "audio" => (Kind::BestAudio, base.clone()),
            "ba*" | "bestaudio*" => (Kind::BestAudioAny, base.clone()),
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
        let (digits, multiplier) = match value.as_bytes().last().copied() {
            Some(b'k' | b'K') => (&value[..value.len() - 1], 1_000u64),
            Some(b'm' | b'M') => (&value[..value.len() - 1], 1_000_000u64),
            _ => (value, 1u64),
        };
        digits
            .parse::<u64>()
            .map(|number| number.saturating_mul(multiplier))
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
        ("language", "=") => filter.language = Some(value.to_ascii_lowercase()),
        ("language", "!=") => filter.language_not = Some(value.to_ascii_lowercase()),
        ("language", "^=") => filter.language_prefix = Some(value.to_ascii_lowercase()),
        ("language", "~") => filter.language_contains = Some(value.to_ascii_lowercase()),
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
    let parts: Vec<&str> = alternative
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "empty format selector",
        ));
    }

    let mut video = None;
    let mut audios = Vec::new();
    for part in parts {
        let component = Component::parse(part)?;
        match component.kind {
            Kind::BestVideo | Kind::BestVideoAny | Kind::WorstVideo => {
                let format = choose_video(formats, &component)?;
                set_video(&mut video, format, alternative)?;
            }
            Kind::BestAudio | Kind::WorstAudio => {
                let format = choose_audio(formats, &component)?;
                push_audio(&mut audios, format);
            }
            Kind::BestAudioAny => {
                let format = choose_audio_any(formats, &component)?;
                push_audio(&mut audios, format);
            }
            Kind::Best | Kind::BestAny | Kind::Worst | Kind::BestExt | Kind::Exact => {
                let format = choose_single(formats, &component)?;
                if is_audio_only(&format) {
                    push_audio(&mut audios, format);
                } else {
                    set_video(&mut video, format, alternative)?;
                }
            }
        }
    }

    if video.is_none() && audios.is_empty() {
        return Err(YtrustError::FormatNotFound(format!(
            "no format matched {alternative:?}"
        )));
    }

    Ok(FormatSelection { video, audios })
}

fn set_video(video: &mut Option<Format>, format: Format, alternative: &str) -> Result<()> {
    if video.is_some() {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("multiple video formats selected in {alternative:?}"),
        ));
    }
    *video = Some(format);
    Ok(())
}

fn push_audio(audios: &mut Vec<Format>, format: Format) {
    let duplicate = audios.iter().any(|existing| {
        existing.itag == format.itag
            && existing.language == format.language
            && existing.audio_track_id == format.audio_track_id
    });
    if !duplicate {
        audios.push(format);
    }
}

fn choose_single(formats: &[Format], component: &Component) -> Result<Format> {
    let mut candidates: Vec<&Format> = formats
        .iter()
        .filter(|format| {
            component.filter.matches(format)
                && format_matches_kind(format, component)
                && !(matches!(component.kind, Kind::Worst) && format.is_hls)
        })
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

fn choose_audio_any(formats: &[Format], component: &Component) -> Result<Format> {
    let mut candidates: Vec<&Format> = formats
        .iter()
        .filter(|format| {
            format.has_audio
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
    Ok((*candidates.last().expect("non-empty candidate list")).clone())
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
        Kind::BestAny => true,
        _ => true,
    }
}

fn is_audio_ext(value: &str) -> bool {
    matches!(value, "m4a" | "mp3" | "opus" | "flac" | "wav" | "ogg")
}

fn format_matches_video_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::BestVideo => is_video_only(format),
        Kind::BestVideoAny | Kind::WorstVideo => true,
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

pub fn print_formats(info: &VideoInfo) -> Result<()> {
    use std::io::Write;

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{:<6} {:<6} {:<12} {:<6} {:<14} {:<14} {:<8} {:>10}  NOTE",
        "ID", "EXT", "RESOLUTION", "FPS", "VCODEC", "ACODEC", "LANG", "SIZE"
    )?;
    for format in &info.formats {
        let resolution = match (format.width, format.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            (None, Some(h)) => format!("{h}p"),
            _ if format.is_hls => String::from("HLS"),
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
        writeln!(
            out,
            "{:<6} {:<6} {:<12} {:<6} {:<14} {:<14} {:<8} {:>10}  {}",
            format.itag,
            format_ext(format),
            resolution,
            fps,
            format.video_codec.as_deref().unwrap_or("none"),
            format.audio_codec.as_deref().unwrap_or("none"),
            format.language.as_deref().unwrap_or("-"),
            size,
            note
        )?;
    }

    let duration = info
        .duration_secs
        .map(format_duration)
        .unwrap_or_else(|| "?".to_string());
    writeln!(out, "\nDuration: {duration}")?;
    if let Some(hls) = &info.hls_manifest_url {
        writeln!(out, "HLS manifest: {hls}")?;
    }
    out.flush()?;
    Ok(())
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
            language: None,
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
        assert_eq!(selection.audios[0].itag, 251);
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

    #[test]
    fn parses_language_filters() {
        let (_, filter) = parse_filter("bestaudio[language=en]").unwrap();
        assert_eq!(filter.language.as_deref(), Some("en"));
        let (_, filter) = parse_filter("bestaudio[language^=en]").unwrap();
        assert_eq!(filter.language_prefix.as_deref(), Some("en"));
        let (_, filter) = parse_filter("bestaudio[language!=de]").unwrap();
        assert_eq!(filter.language_not.as_deref(), Some("de"));
    }

    #[test]
    fn parses_numeric_suffixes() {
        let (_, filter) = parse_filter("bestvideo[height<=1k]").unwrap();
        assert_eq!(filter.height_lte, Some(1000));
        let (_, filter) = parse_filter("bestvideo[height<=1M]").unwrap();
        assert_eq!(filter.height_lte, Some(1_000_000));
    }

    #[test]
    fn selects_multiple_audio_languages() {
        let mut en = format(251, false, true, None, Some(160_000));
        en.language = Some("en".to_string());
        let mut de = format(252, false, true, None, Some(160_000));
        de.language = Some("de".to_string());
        let info = info(vec![format(137, true, false, Some(1080), None), en, de]);
        let selection = select(
            &info,
            "bestvideo+bestaudio[language=en]+bestaudio[language=de]",
        )
        .unwrap();
        assert_eq!(selection.video.unwrap().itag, 137);
        assert_eq!(selection.audios.len(), 2);
        assert_eq!(selection.audios[0].language.as_deref(), Some("en"));
        assert_eq!(selection.audios[1].language.as_deref(), Some("de"));
    }

    #[test]
    fn bestaudio_star_accepts_muxed_fallback() {
        let info = info(vec![format(18, true, true, Some(360), None)]);
        let selection = select(&info, "bestaudio*").unwrap();
        assert_eq!(selection.single().unwrap().itag, 18);
    }

    #[test]
    fn worst_skips_hls() {
        let mut hls = format(0, true, true, None, None);
        hls.is_hls = true;
        let info = info(vec![hls]);
        assert!(select(&info, "worst").is_err());
    }
}
