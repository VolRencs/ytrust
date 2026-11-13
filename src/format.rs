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
            && value.height.is_none_or(|height| height > limit)
        {
            return false;
        }
        if let Some(limit) = self.height_gte
            && value.height.is_none_or(|height| height < limit)
        {
            return false;
        }
        if let Some(limit) = self.width_lte
            && value.width.is_none_or(|width| width > limit)
        {
            return false;
        }
        if let Some(limit) = self.width_gte
            && value.width.is_none_or(|width| width < limit)
        {
            return false;
        }
        if let Some(limit) = self.fps_lte
            && value.fps.is_none_or(|fps| fps > limit)
        {
            return false;
        }
        if let Some(limit) = self.fps_gte
            && value.fps.is_none_or(|fps| fps < limit)
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

        if self.has_video_codec_filter() {
            let vcodec = value
                .video_codec
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
        }

        if self.has_audio_codec_filter() {
            let acodec = value
                .audio_codec
                .as_deref()
                .unwrap_or("none")
                .to_lowercase();
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
        }

        if self.has_language_filter() {
            let language = value.language.as_deref().unwrap_or("none").to_lowercase();
            if let Some(expected) = &self.language
                && !language_matches(&language, expected)
            {
                return false;
            }
            if let Some(unexpected) = &self.language_not
                && language_matches(&language, unexpected)
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
        }

        true
    }

    fn has_video_codec_filter(&self) -> bool {
        self.vcodec.is_some() || self.vcodec_not.is_some() || self.vcodec_prefix.is_some()
    }

    fn has_audio_codec_filter(&self) -> bool {
        self.acodec.is_some() || self.acodec_not.is_some() || self.acodec_prefix.is_some()
    }

    fn has_language_filter(&self) -> bool {
        self.language.is_some()
            || self.language_not.is_some()
            || self.language_prefix.is_some()
            || self.language_contains.is_some()
    }
}

fn language_matches(actual: &str, expected: &str) -> bool {
    actual == expected
        || actual
            .strip_prefix(expected)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn ext_matches(actual: &str, expected: &str) -> bool {
    let expected = expected.trim_start_matches('.');
    if actual.eq_ignore_ascii_case(expected) {
        return true;
    }
    matches!(
        (
            actual.to_ascii_lowercase().as_str(),
            expected.to_ascii_lowercase().as_str()
        ),
        ("m4a", "mp4") | ("mp4", "m4a") | ("webm", "opus") | ("mka", "m4a")
    )
}

#[derive(Debug, Clone)]
struct Component {
    kind: Kind,
    filter: Filter,
    argument: String,
    itag: Option<u64>,
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
        let itag = (kind == Kind::Exact)
            .then(|| base.parse::<u64>().ok())
            .flatten();

        Ok(Self {
            kind,
            filter,
            argument,
            itag,
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

pub struct FormatSelector {
    expression: String,
    alternatives: Vec<Vec<Component>>,
}

impl FormatSelector {
    pub fn parse(expression: &str) -> Result<Self> {
        let expression = if expression.trim().is_empty() {
            crate::constants::DEFAULT_FORMAT
        } else {
            expression.trim()
        };
        let mut alternatives = Vec::new();
        for alternative in expression.split('/') {
            let parts: Vec<Component> = alternative
                .split('+')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(Component::parse)
                .collect::<Result<_>>()?;
            if parts.is_empty() {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    "empty format selector",
                ));
            }
            alternatives.push(parts);
        }
        Ok(Self {
            expression: expression.to_string(),
            alternatives,
        })
    }

    pub fn select(&self, formats: &[Format]) -> Result<FormatSelection> {
        let mut last_error = None;
        for parts in &self.alternatives {
            match select_alternative(formats, parts) {
                Ok(selection) => return Ok(selection),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            YtrustError::FormatNotFound(format!("no format matched {:?}", self.expression))
        }))
    }
}

pub fn select(info: &VideoInfo, expression: &str) -> Result<FormatSelection> {
    FormatSelector::parse(expression)?.select(&info.formats)
}

fn select_alternative(formats: &[Format], parts: &[Component]) -> Result<FormatSelection> {
    let mut video = None;
    let mut audios = Vec::new();
    for component in parts {
        match component.kind {
            Kind::BestVideo | Kind::BestVideoAny | Kind::WorstVideo => {
                let format = choose_video(formats, component)?;
                set_video(&mut video, format, &component.argument)?;
            }
            Kind::BestAudio | Kind::WorstAudio => {
                let format = choose_audio(formats, component)?;
                push_audio(&mut audios, format);
            }
            Kind::BestAudioAny => {
                let format = choose_audio_any(formats, component)?;
                push_audio(&mut audios, format);
            }
            Kind::Best | Kind::BestAny | Kind::Worst | Kind::BestExt | Kind::Exact => {
                let format = choose_single(formats, component)?;
                if is_audio_only(&format) {
                    push_audio(&mut audios, format);
                } else {
                    set_video(&mut video, format, &component.argument)?;
                }
            }
        }
    }

    if video.is_none() && audios.is_empty() {
        return Err(YtrustError::FormatNotFound(format!(
            "no format matched {:?}",
            parts
                .iter()
                .map(|part| part.argument.as_str())
                .collect::<Vec<_>>()
                .join("+")
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
    let candidates = formats.iter().filter(|format| {
        component.filter.matches(format)
            && format_matches_kind(format, component)
            && !(matches!(component.kind, Kind::Worst) && format.is_hls)
    });
    let selected = if matches!(component.kind, Kind::Worst) {
        candidates.min_by(|a, b| compare_video(a, b))
    } else {
        candidates.max_by(|a, b| compare_video(a, b))
    };
    selected
        .cloned()
        .ok_or_else(|| YtrustError::FormatNotFound(component.argument.clone()))
}

fn choose_video(formats: &[Format], component: &Component) -> Result<Format> {
    let candidates = formats.iter().filter(|format| {
        format.has_video
            && component.filter.matches(format)
            && format_matches_video_kind(format, component)
    });
    let selected = if matches!(component.kind, Kind::WorstVideo) {
        candidates.min_by(|a, b| compare_video(a, b))
    } else {
        candidates.max_by(|a, b| compare_video(a, b))
    };
    selected
        .cloned()
        .ok_or_else(|| YtrustError::FormatNotFound(format!("{} (video)", component.argument)))
}

fn choose_audio(formats: &[Format], component: &Component) -> Result<Format> {
    let candidates = formats
        .iter()
        .filter(|format| format.has_audio && !format.has_video && component.filter.matches(format));
    let selected = if matches!(component.kind, Kind::WorstAudio) {
        candidates.min_by(|a, b| compare_audio(a, b))
    } else {
        candidates.max_by(|a, b| compare_audio(a, b))
    };
    selected
        .cloned()
        .ok_or_else(|| YtrustError::FormatNotFound(format!("{} (audio)", component.argument)))
}

fn choose_audio_any(formats: &[Format], component: &Component) -> Result<Format> {
    formats
        .iter()
        .filter(|format| format.has_audio && component.filter.matches(format))
        .max_by(|a, b| compare_audio(a, b))
        .cloned()
        .ok_or_else(|| YtrustError::FormatNotFound(format!("{} (audio)", component.argument)))
}

fn format_matches_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::Exact => component.itag == Some(format.itag),
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

/// A selection that is already a single file with both video and audio.
pub fn is_muxed_selection(selection: &FormatSelection) -> bool {
    selection
        .single()
        .is_some_and(|format| format.has_video && format.has_audio)
}

/// Builds a muxed fallback selection for adaptive streams rejected by YouTube.
///
/// Prefers a muxed format no taller than the originally requested height and
/// falls back to the best muxed format when the requested height has no muxed
/// variant.
pub fn muxed_fallback(info: &VideoInfo, selection: &FormatSelection) -> Option<FormatSelection> {
    if is_muxed_selection(selection) {
        return None;
    }
    let height = selection
        .video
        .as_ref()
        .and_then(|format| format.height)
        .or_else(|| {
            selection
                .audios
                .iter()
                .filter_map(|format| format.height)
                .max()
        });
    let mut expressions = Vec::new();
    if let Some(height) = height {
        expressions.push(format!("best[height<={height}]"));
    }
    expressions.push("best".to_owned());
    let selected_itags = selection_itags(selection);
    for expression in expressions {
        let Ok(selector) = FormatSelector::parse(&expression) else {
            continue;
        };
        let Ok(candidate) = selector.select(&info.formats) else {
            continue;
        };
        if selection_itags(&candidate) == selected_itags {
            continue;
        }
        // Preserve an audio-only request: use the muxed stream as the audio
        // source so `download_audio_only` extracts it instead of saving video.
        if selection.video.is_none() {
            let Some(audio) = candidate.single().cloned() else {
                continue;
            };
            return Some(FormatSelection {
                video: None,
                audios: vec![audio],
            });
        }
        return Some(candidate);
    }
    None
}

fn selection_itags(selection: &FormatSelection) -> Vec<u64> {
    let mut itags: Vec<u64> = selection
        .video
        .iter()
        .chain(selection.audios.iter())
        .map(|format| format.itag)
        .collect();
    itags.sort_unstable();
    itags
}

fn format_matches_video_kind(format: &Format, component: &Component) -> bool {
    match component.kind {
        Kind::BestVideo => is_video_only(format),
        _ => true,
    }
}

/// Pick the best audio stream: playable URLs first, then the default track,
/// then audio-only over muxed, then Opus, then bitrate. HLS manifests are
/// excluded from the first pass because callers cannot seek in them like a
/// regular media URL.
pub fn best_audio(info: &VideoInfo) -> Option<&Format> {
    let rate = |format: &Format| {
        format
            .audio_bitrate
            .filter(|rate| *rate > 0)
            .unwrap_or(format.bitrate)
    };
    let key = |format: &Format| {
        (
            u8::from(format.audio_is_default),
            u8::from(!format.has_video),
            u8::from(is_opus(format)),
            rate(format),
        )
    };
    info.formats
        .iter()
        .filter(|format| format.has_audio && !format.is_hls && !format.url.is_empty())
        .max_by_key(|format| key(format))
        .or_else(|| {
            info.formats
                .iter()
                .filter(|format| format.has_audio && !format.url.is_empty())
                .max_by_key(|format| key(format))
        })
        .or_else(|| {
            info.formats
                .iter()
                .filter(|format| format.has_audio && !format.is_hls)
                .max_by_key(|format| key(format))
        })
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
    write_formats(&mut out, info)?;
    out.flush()?;
    Ok(())
}

pub fn write_formats(out: &mut impl std::io::Write, info: &VideoInfo) -> std::io::Result<()> {
    writeln!(
        out,
        "{:<6} {:<6} {:<12} {:<6} {:<14} {:<14} {:<8} {:>10}  NOTE",
        "ID", "EXT", "RESOLUTION", "FPS", "VCODEC", "ACODEC", "LANG", "SIZE"
    )?;
    for format in &info.formats {
        let resolution = match (format.width, format.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            (Some(w), None) => format!("{w}x?"),
            (None, Some(h)) => format!("{h}p"),
            _ if format.is_hls => String::from("HLS"),
            _ if format.has_video => String::from("video"),
            _ => String::from("audio"),
        };
        let fps = format.fps.map(|v| v.to_string()).unwrap_or_default();
        let size = match (
            format.content_length,
            format.estimated_size(info.duration_secs),
        ) {
            (Some(length), _) => format_bytes(length),
            (None, Some(estimate)) => format!("~{}", format_bytes(estimate)),
            _ => "?".to_string(),
        };
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
            user_agent: "test",
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

    #[test]
    fn matches_regional_language_tags() {
        let mut en_us = format(251, false, true, None, Some(160_000));
        en_us.language = Some("en-US".to_string());
        let info = info(vec![en_us]);
        assert!(select(&info, "bestaudio[language=en]").is_ok());
        assert!(select(&info, "bestaudio[language!=en]").is_err());
    }

    #[test]
    fn unknown_dimensions_do_not_pass_upper_bounds() {
        let info = info(vec![format(137, true, false, None, None)]);
        assert!(select(&info, "bestvideo[height<=1080]").is_err());
        assert!(select(&info, "bestvideo[height>=1080]").is_err());
    }

    #[test]
    fn best_audio_prefers_playable_urls() {
        let mut playable = format(140, false, true, None, Some(64_000));
        playable.audio_is_default = true;
        let mut broken = format(251, false, true, None, Some(160_000));
        broken.url.clear();
        broken.signature_cipher = Some("s=x&url=https%3A%2F%2Fexample.com".to_string());
        let info = info(vec![broken, playable]);
        let best = best_audio(&info).unwrap();
        assert_eq!(best.itag, 140);
        assert!(!best.url.is_empty());
    }

    #[test]
    fn best_audio_falls_back_to_unresolved_when_nothing_is_playable() {
        let mut broken = format(251, false, true, None, Some(160_000));
        broken.url.clear();
        broken.signature_cipher = Some("s=x&url=https%3A%2F%2Fexample.com".to_string());
        let info = info(vec![broken]);
        assert_eq!(best_audio(&info).unwrap().itag, 251);
    }

    #[test]
    fn muxed_fallback_replaces_adaptive_selection() {
        let info = info(vec![
            format(137, true, false, Some(1080), None),
            format(251, false, true, None, Some(160_000)),
            format(18, true, true, Some(360), Some(96_000)),
        ]);
        let primary = select(&info, "bestvideo+bestaudio").unwrap();
        assert!(primary.needs_merge());
        let fallback = muxed_fallback(&info, &primary).unwrap();
        assert!(is_muxed_selection(&fallback));
        assert_eq!(fallback.single().unwrap().itag, 18);
    }

    #[test]
    fn muxed_fallback_ignores_height_when_no_muxed_variant_matches() {
        let info = info(vec![
            format(160, true, false, Some(144), None),
            format(251, false, true, None, Some(160_000)),
            format(18, true, true, Some(360), Some(96_000)),
        ]);
        let primary = select(&info, "bestvideo[height<=144]+bestaudio").unwrap();
        let fallback = muxed_fallback(&info, &primary).unwrap();
        assert_eq!(fallback.single().unwrap().itag, 18);
    }

    #[test]
    fn muxed_fallback_is_not_used_for_muxed_single() {
        let info = info(vec![format(18, true, true, Some(360), Some(96_000))]);
        let primary = select(&info, "best").unwrap();
        assert!(is_muxed_selection(&primary));
        assert!(muxed_fallback(&info, &primary).is_none());
    }

    #[test]
    fn muxed_fallback_handles_audio_only_selection() {
        let info = info(vec![
            format(251, false, true, None, Some(160_000)),
            format(18, true, true, Some(360), Some(96_000)),
        ]);
        let primary = select(&info, "bestaudio").unwrap();
        let fallback = muxed_fallback(&info, &primary).unwrap();
        assert!(fallback.video.is_none());
        assert_eq!(fallback.audios.len(), 1);
        assert_eq!(fallback.audios[0].itag, 18);
        assert!(fallback.audios[0].has_video);
        assert!(fallback.audios[0].has_audio);
    }

    #[test]
    fn parses_selector_once_and_reuses_it() {
        let selector = FormatSelector::parse("bestvideo+bestaudio/best").unwrap();
        let info = info(vec![
            format(137, true, false, Some(1080), None),
            format(251, false, true, None, Some(160_000)),
        ]);
        let selection = selector.select(&info.formats).unwrap();
        assert!(selection.needs_merge());
        assert_eq!(selection.video.unwrap().itag, 137);
        assert_eq!(selection.audios[0].itag, 251);
    }
}
