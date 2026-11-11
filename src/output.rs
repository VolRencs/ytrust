use std::path::{Component, Path, PathBuf};

use crate::youtube::{Format, VideoInfo};

use crate::error::{Result, YtrustError};

#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    pub title: String,
    pub id: String,
    pub uploader: String,
    pub channel: String,
    pub ext: String,
    pub height: String,
    pub width: String,
    pub fps: String,
    pub format_id: String,
    pub duration: String,
    pub upload_date: String,
    pub view_count: String,
    pub description: String,
    pub playlist: String,
    pub playlist_index: String,
}

impl TemplateContext {
    pub fn from_info(info: &VideoInfo, ext: &str) -> Self {
        Self {
            title: info.title.clone(),
            id: info.id.clone(),
            uploader: info.uploader.clone(),
            channel: info.channel.clone(),
            ext: ext.to_string(),
            height: String::new(),
            width: String::new(),
            fps: String::new(),
            format_id: String::new(),
            duration: info
                .duration_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            upload_date: info.upload_date.clone(),
            view_count: info
                .view_count
                .map(|value| value.to_string())
                .unwrap_or_default(),
            description: info.description.clone(),
            playlist: String::new(),
            playlist_index: String::new(),
        }
    }

    pub fn with_format(mut self, format: &Format) -> Self {
        self.height = format.height.map(|v| v.to_string()).unwrap_or_default();
        self.width = format.width.map(|v| v.to_string()).unwrap_or_default();
        self.fps = format.fps.map(|v| v.to_string()).unwrap_or_default();
        self.format_id = format.itag.to_string();
        self
    }

    pub fn with_playlist(mut self, name: impl Into<String>, index: Option<usize>) -> Self {
        self.playlist = name.into();
        self.playlist_index = index.map(|v| v.to_string()).unwrap_or_default();
        self
    }
}

pub fn render_path(
    template: &str,
    ctx: &TemplateContext,
    output_dir: Option<&Path>,
    restrict: bool,
) -> Result<PathBuf> {
    if template == "-" {
        return Ok(PathBuf::from("-"));
    }

    let rendered = replace_template(template, ctx, restrict);
    let mut path = PathBuf::new();

    for component in Path::new(&rendered).components() {
        match component {
            Component::RootDir => path.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Prefix(prefix) => path.push(prefix.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => path.push("_"),
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                let safe = sanitize_component(&part, restrict);
                if !safe.is_empty() {
                    path.push(safe);
                }
            }
        }
    }

    if path.as_os_str().is_empty() {
        return Err(YtrustError::InvalidOutput(path));
    }

    if let Some(dir) = output_dir
        && !path.is_absolute()
    {
        path = dir.join(path);
    }

    Ok(path)
}

fn replace_template(template: &str, ctx: &TemplateContext, restrict: bool) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let key = &after[..end];
            out.push_str(&sanitize_component(template_value(key, ctx), restrict));
            rest = &after[end + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn template_value<'a>(key: &str, ctx: &'a TemplateContext) -> &'a str {
    match key {
        "title" => &ctx.title,
        "id" => &ctx.id,
        "uploader" => &ctx.uploader,
        "channel" => &ctx.channel,
        "ext" => &ctx.ext,
        "height" => &ctx.height,
        "width" => &ctx.width,
        "fps" => &ctx.fps,
        "format_id" => &ctx.format_id,
        "duration" => &ctx.duration,
        "upload_date" => &ctx.upload_date,
        "view_count" => &ctx.view_count,
        "description" => &ctx.description,
        "playlist" => &ctx.playlist,
        "playlist_index" => &ctx.playlist_index,
        _ => "",
    }
}

pub fn sanitize_component(value: &str, restrict: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let allowed = if restrict {
            ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ')
        } else {
            !matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !ch.is_control()
        };

        if allowed {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }

    let trimmed = out.trim().trim_matches('.').trim();
    let mut trimmed = trimmed.to_string();
    if trimmed.len() > 180 {
        trimmed.truncate(180);
        while trimmed.ends_with(' ') {
            trimmed.pop();
        }
    }

    trimmed
}

pub fn format_ext(format: &Format) -> &'static str {
    match format.container.as_str() {
        "mp4" if !format.has_video => "m4a",
        "mp4" => "mp4",
        "webm" => "webm",
        "3gpp" => "3gp",
        "x-matroska" | "matroska" => "mkv",
        "mpeg" => "mp3",
        "ogg" => "ogg",
        "quicktime" => "mov",
        _ => "bin",
    }
}

pub fn merged_ext(video_ext: &str, audio_ext: &str, override_ext: Option<&str>) -> String {
    if let Some(ext) = override_ext {
        return ext.trim_start_matches('.').to_string();
    }
    if video_ext == audio_ext {
        return video_ext.to_string();
    }
    match (video_ext, audio_ext) {
        ("mp4", "m4a") | ("m4a", "mp4") => "mp4".to_string(),
        ("webm", "webm") => "webm".to_string(),
        _ => "mkv".to_string(),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_and_renders() {
        let ctx = TemplateContext {
            title: "A/B: Test?".into(),
            id: "abc123".into(),
            ext: "mp4".into(),
            ..Default::default()
        };
        let path = render_path("{title} [{id}].{ext}", &ctx, None, false).unwrap();
        assert_eq!(path, PathBuf::from("A_B_ Test_ [abc123].mp4"));
    }

    #[test]
    fn blocks_parent_traversal() {
        let ctx = TemplateContext {
            title: "evil".into(),
            ..Default::default()
        };
        let path = render_path("../{title}", &ctx, None, false).unwrap();
        assert_eq!(path, PathBuf::from("_/evil"));
    }
}
