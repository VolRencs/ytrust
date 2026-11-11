mod cli;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use ytrust::api::{self, ResolveOptions, Resolved};
use ytrust::constants::DEFAULT_FORMAT;
use ytrust::cookies;
use ytrust::download::{DownloadConfig, DownloadRange, Downloader, audio_codec, parse_range};
use ytrust::error::{ErrorCode, Result, YtrustError};
use ytrust::format::{FormatSelection, print_formats, select};
use ytrust::http::RequestConfig;
use ytrust::json;
use ytrust::output::{TemplateContext, format_duration, format_ext, merged_ext, render_path};
use ytrust::youtube::{self, Format, VideoInfo, fetch_video};

use crate::cli::Cli;

macro_rules! status {
    ($cli:expr, $($arg:tt)*) => {
        if !$cli.quiet {
            eprintln!($($arg)*);
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonMode {
    None,
    PerVideo,
    Single,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(&cli);

    if let Err(err) = run(cli).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let cookies = cookies::load(cli.cookies.as_deref(), cli.cookies_from_browser.as_deref())?;
    if cookies.header.is_some() {
        debug!("loaded cookies");
    }
    if let Some(audio_format) = &cli.audio_format {
        audio_codec(audio_format.trim_start_matches('.'))?;
    }
    validate_merge_format(cli.merge_output_format.as_deref())?;

    if cli.flat_playlist {
        debug!("--flat-playlist is the default behavior; playlists are always resolved flat");
    }
    let timeout = cli.socket_timeout.unwrap_or(cli.timeout);
    let request_config = RequestConfig {
        proxy: cli.proxy.clone(),
        cookies: cookies.header.clone(),
        sapisid: cookies.sapisid.clone(),
        sapisid1p: cookies.sapisid1p.clone(),
        sapisid3p: cookies.sapisid3p.clone(),
        timeout,
    };
    let retries = cli.fragment_retries.unwrap_or(cli.retries);
    let downloader = Downloader::new(DownloadConfig {
        retries,
        timeout,
        proxy: cli.proxy.clone(),
        cookies: cookies.header,
        ffmpeg: cli.ffmpeg_location.clone(),
        force_overwrites: cli.force_overwrites,
        quiet: cli.quiet,
    })?;

    let json_mode = if cli.dump_single_json {
        JsonMode::Single
    } else if cli.dump_json {
        JsonMode::PerVideo
    } else {
        JsonMode::None
    };
    let simulate = cli.simulate || cli.skip_download;
    let section = parse_range(cli.download_sections.as_deref().unwrap_or_default())?;
    let options = ResolveOptions {
        no_playlist: cli.no_playlist,
        playlist_limit: cli.playlist_limit,
        playlist_items: cli.playlist_items.clone(),
    };

    let context = RunContext {
        cli: &cli,
        request_config: &request_config,
        downloader: &downloader,
        json_mode,
        simulate,
        section,
    };

    for raw_url in &cli.urls {
        let resolved = api::resolve(raw_url, &request_config, &options).await?;
        match resolved {
            Resolved::Video { info, original_url } => {
                process_info(*info, &original_url, None, &context).await?;
            }
            Resolved::Playlist { info, original_url } => {
                process_playlist(info, &original_url, &context).await?;
            }
        }
    }

    Ok(())
}

struct RunContext<'a> {
    cli: &'a Cli,
    request_config: &'a RequestConfig,
    downloader: &'a Downloader,
    json_mode: JsonMode,
    simulate: bool,
    section: Option<DownloadRange>,
}

async fn process_playlist(
    playlist: youtube::PlaylistInfo,
    original_url: &str,
    context: &RunContext<'_>,
) -> Result<()> {
    if context.json_mode == JsonMode::Single {
        println!(
            "{}",
            serde_json::to_string(&json::playlist_json(&playlist, original_url))?
        );
        return Ok(());
    }

    if context.json_mode == JsonMode::PerVideo {
        for entry in &playlist.entries {
            let original = format!("https://www.youtube.com/watch?v={}", entry.id);
            let playlist_context = (playlist.title.clone(), entry.index);
            process_video_id(&entry.id, &original, Some(playlist_context), context).await?;
        }
        return Ok(());
    }

    if playlist.entries.is_empty() {
        warn!("playlist {:?} has no matching entries", playlist.title);
        return Ok(());
    }

    info!(
        "playlist {:?} ({}): downloading {} entries",
        playlist.title,
        playlist.id,
        playlist.entries.len()
    );

    for (position, entry) in playlist.entries.iter().enumerate() {
        info!(
            "[{}/{}] {}",
            position + 1,
            playlist.entries.len(),
            entry.title
        );
        let original = format!("https://www.youtube.com/watch?v={}", entry.id);
        let playlist_context = (playlist.title.clone(), entry.index);
        process_video_id(&entry.id, &original, Some(playlist_context), context).await?;
    }
    Ok(())
}

async fn process_video_id(
    video_id: &str,
    original_url: &str,
    playlist: Option<(String, usize)>,
    context: &RunContext<'_>,
) -> Result<()> {
    let info = fetch_video(video_id, context.request_config).await?;
    process_info(info, original_url, playlist, context).await
}

async fn process_info(
    mut info: VideoInfo,
    original_url: &str,
    playlist: Option<(String, usize)>,
    context: &RunContext<'_>,
) -> Result<()> {
    let cli = context.cli;
    let downloader = context.downloader;

    if cli.list_audio_tracks {
        print_audio_tracks(&info);
        return Ok(());
    }

    if let Some(track) = &cli.audio_track {
        apply_audio_track(&mut info, track)?;
    }

    if context.json_mode != JsonMode::None {
        println!(
            "{}",
            serde_json::to_string(&json::video_json(&info, original_url))?
        );
        return Ok(());
    }

    if cli.list_formats {
        println!("\n{} [{}]", info.title, info.id);
        print_formats(&info);
        return Ok(());
    }

    if context.simulate {
        status!(cli, "simulate: {} [{}]", info.title, info.id);
        return Ok(());
    }

    let expression = if context.extract_audio() {
        cli.format.as_deref().unwrap_or("bestaudio/best")
    } else {
        cli.format.as_deref().unwrap_or(DEFAULT_FORMAT)
    };
    let selection = select(&info, expression)?;

    if context.extract_audio()
        && selection.audio.is_none()
        && selection
            .video
            .as_ref()
            .is_some_and(|format| !format.has_audio)
    {
        return Err(YtrustError::coded(
            ErrorCode::NoAudio,
            format!("selected format has no audio for {}", info.id),
        ));
    }

    if selection.needs_merge() && !downloader.ffmpeg_available().await {
        return Err(YtrustError::coded(
            ErrorCode::Ffmpeg,
            "ffmpeg is required to merge separate video and audio streams; install ffmpeg or choose a single format with -f",
        ));
    }

    if cli.output == "-" {
        if context.extract_audio() && cli.audio_format.is_some() {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "audio conversion to stdout is not supported; write to a file instead",
            ));
        }
        let format = selection.single().ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                "streaming merged video+audio to stdout is not supported",
            )
        })?;
        return downloader
            .stream_to_stdout(format, context.section, &info.id)
            .await;
    }

    write_extras(&info, playlist.as_ref(), cli, downloader).await?;

    let duration_secs = info.duration_secs;
    if context.extract_audio() {
        return download_audio_only(
            &info,
            &selection,
            playlist,
            cli,
            downloader,
            duration_secs,
            context.section,
        )
        .await;
    }

    download_video(
        &info,
        &selection,
        playlist,
        cli,
        downloader,
        duration_secs,
        context.section,
    )
    .await
}

impl RunContext<'_> {
    fn extract_audio(&self) -> bool {
        self.cli.extract_audio || self.cli.audio_format.is_some()
    }
}

async fn download_video(
    info: &VideoInfo,
    selection: &FormatSelection,
    playlist: Option<(String, usize)>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
    section: Option<DownloadRange>,
) -> Result<()> {
    let (ext, context_format) = if selection.needs_merge() {
        let video = selection.video.as_ref().expect("video");
        let audio = selection.audio.as_ref().expect("audio");
        let ext = merged_ext(
            format_ext(video),
            format_ext(audio),
            cli.merge_output_format.as_deref(),
        );
        (ext, video.clone())
    } else {
        let format = selection
            .single()
            .ok_or_else(|| YtrustError::coded(ErrorCode::InvalidInput, "no format selected"))?;
        (format_ext(format).to_string(), format.clone())
    };

    let mut context = TemplateContext::from_info(info, &ext).with_format(&context_format);
    if let Some((name, index)) = &playlist {
        context = context.with_playlist(name, Some(*index));
    }
    let destination = render_path(
        &cli.output,
        &context,
        cli.output_dir.as_deref(),
        cli.restrict_filenames,
    )?;

    if destination == Path::new("-") {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "stdout output is not supported for video downloads yet",
        ));
    }

    if destination.exists() && !cli.force_overwrites {
        status!(cli, "already downloaded: {}", destination.to_string_lossy());
        return Ok(());
    }

    let label = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&info.id)
        .to_string();

    debug!(
        "selected formats: video={:?}, audio={:?}",
        selection.video.as_ref().map(|format| format.itag),
        selection.audio.as_ref().map(|format| format.itag)
    );

    if !selection.needs_merge() {
        let format = selection
            .single()
            .ok_or_else(|| YtrustError::coded(ErrorCode::InvalidInput, "no format selected"))?;
        status!(
            cli,
            "downloading: {} [{}] ({})",
            info.title,
            info.id,
            format_duration(duration_secs.unwrap_or(0))
        );
        return downloader
            .download_format(format, &destination, &label, duration_secs, section)
            .await;
    }

    let video = selection.video.as_ref().expect("video");
    let audio = selection.audio.as_ref().expect("audio");
    let video_path = intermediate_path(&destination, "video", video.itag, format_ext(video));
    let audio_path = intermediate_path(&destination, "audio", audio.itag, format_ext(audio));

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    status!(
        cli,
        "downloading: {} [{}] ({} + {})",
        info.title,
        info.id,
        format_ext(video),
        format_ext(audio)
    );

    let video_label = format!("{} [video]", label);
    let audio_label = format!("{} [audio]", label);
    let video_download =
        downloader.download_format(video, &video_path, &video_label, duration_secs, section);
    let audio_download =
        downloader.download_format(audio, &audio_path, &audio_label, duration_secs, section);

    let (video_result, audio_result) = tokio::join!(video_download, audio_download);
    video_result?;
    audio_result?;

    let result = downloader
        .merge(
            &video_path,
            &audio_path,
            &destination,
            &format!("{} [merge]", label),
            duration_secs,
        )
        .await;

    let _ = tokio::fs::remove_file(&video_path).await;
    let _ = tokio::fs::remove_file(&audio_path).await;
    result
}

async fn download_audio_only(
    info: &VideoInfo,
    selection: &FormatSelection,
    playlist: Option<(String, usize)>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
    section: Option<DownloadRange>,
) -> Result<()> {
    let audio = selection
        .audio
        .as_ref()
        .or(selection.video.as_ref())
        .ok_or_else(|| YtrustError::coded(ErrorCode::NoAudio, "no audio format selected"))?;

    let source_ext = format_ext(audio);
    let extracted = selection.audio.is_none();
    let target_ext = if let Some(audio_format) = cli.audio_format.as_deref() {
        audio_format.trim_start_matches('.').to_ascii_lowercase()
    } else if extracted {
        default_audio_ext(audio).to_string()
    } else {
        source_ext.to_string()
    };
    let convert = extracted || (cli.audio_format.is_some() && target_ext != source_ext);

    let mut context = TemplateContext::from_info(info, &target_ext).with_format(audio);
    if let Some((name, index)) = &playlist {
        context = context.with_playlist(name, Some(*index));
    }
    let destination = render_path(
        &cli.output,
        &context,
        cli.output_dir.as_deref(),
        cli.restrict_filenames,
    )?;

    if destination == Path::new("-") {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "stdout output is not supported for audio downloads yet",
        ));
    }

    if destination.exists() && !cli.force_overwrites {
        status!(cli, "already downloaded: {}", destination.to_string_lossy());
        return Ok(());
    }

    let label = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&info.id)
        .to_string();

    status!(cli, "downloading audio: {} [{}]", info.title, info.id);

    if !convert {
        return downloader
            .download_format(audio, &destination, &label, duration_secs, section)
            .await;
    }

    if !downloader.ffmpeg_available().await {
        return Err(YtrustError::FfmpegMissing);
    }

    let source_path = intermediate_path(&destination, "source", audio.itag, source_ext);
    downloader
        .download_format(
            audio,
            &source_path,
            &format!("{label} [audio]"),
            duration_secs,
            section,
        )
        .await?;

    let result = downloader
        .convert_audio(
            &source_path,
            &destination,
            &target_ext,
            &format!("{label} [{target_ext}]"),
        )
        .await;
    let _ = tokio::fs::remove_file(&source_path).await;
    result
}

async fn write_extras(
    info: &VideoInfo,
    playlist: Option<&(String, usize)>,
    cli: &Cli,
    downloader: &Downloader,
) -> Result<()> {
    if !cli.write_thumbnail && !cli.write_all_thumbnails && !cli.write_subs && !cli.write_auto_subs
    {
        return Ok(());
    }

    let mut ctx = TemplateContext::from_info(info, "jpg");
    if let Some((name, index)) = playlist {
        ctx = ctx.with_playlist(name, Some(*index));
    }

    if cli.write_all_thumbnails {
        let mut used = BTreeSet::new();
        for (index, thumb) in info.thumbnails.iter().enumerate() {
            let ext = thumbnail_ext(&thumb.url);
            let base = render_path(
                &cli.output,
                &ctx.clone().with_ext(ext),
                cli.output_dir.as_deref(),
                cli.restrict_filenames,
            )?;
            let mut suffix = match (thumb.width, thumb.height) {
                (Some(width), Some(height)) => format!("{width}x{height}"),
                _ => index.to_string(),
            };
            if !used.insert(suffix.clone()) {
                suffix.push_str(&format!(".{index}"));
            }
            let destination = append_to_stem(&base, &suffix, ext);
            downloader
                .download_url(&thumb.url, &destination, &destination.to_string_lossy())
                .await?;
        }
    } else if cli.write_thumbnail
        && let Some(thumb) = info.thumbnails.last()
    {
        let ext = thumbnail_ext(&thumb.url);
        let base = render_path(
            &cli.output,
            &ctx.clone().with_ext(ext),
            cli.output_dir.as_deref(),
            cli.restrict_filenames,
        )?;
        let destination = ensure_extension(&base, ext);
        downloader
            .download_url(&thumb.url, &destination, &destination.to_string_lossy())
            .await?;
    }

    let mut tracks = Vec::new();
    if cli.write_subs {
        tracks.extend(info.subtitles.iter().map(|track| (track, false)));
    }
    if cli.write_auto_subs {
        tracks.extend(info.automatic_captions.iter().map(|track| (track, true)));
    }
    if tracks.is_empty() {
        return Ok(());
    }

    let format = cli.sub_format.to_ascii_lowercase();
    if format != "vtt" && format != "srt" {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "--sub-format supports vtt or srt",
        ));
    }

    let mut used = std::collections::BTreeMap::new();
    for (track, is_auto) in tracks {
        if !subtitle_language_selected(&cli.sub_langs, &track.language) {
            continue;
        }
        let url = with_subtitle_format(&track.url, "vtt")?;
        let body = downloader.fetch_text(&url).await?;
        let body = if format == "srt" {
            vtt_to_srt(&body)
        } else {
            body
        };
        let base = render_path(
            &cli.output,
            &ctx.clone().with_ext(format.clone()),
            cli.output_dir.as_deref(),
            cli.restrict_filenames,
        )?;
        let mut suffix = track.language.clone();
        if is_auto {
            suffix.push_str(".auto");
        }
        let count = used.entry(suffix.clone()).or_insert(0usize);
        *count += 1;
        if *count > 1 {
            suffix.push_str(&format!(".{}", *count));
        }
        let destination = append_to_stem(&base, &suffix, &format);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&destination, body).await?;
        status!(cli, "subtitle: {}", destination.to_string_lossy());
    }

    Ok(())
}

fn default_audio_ext(format: &Format) -> &'static str {
    let codec = format
        .audio_codec
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if codec.starts_with("opus") {
        "opus"
    } else if codec.starts_with("mp4a") || codec.starts_with("aac") {
        "m4a"
    } else if codec.starts_with("vorbis") {
        "ogg"
    } else if codec.starts_with("flac") {
        "flac"
    } else {
        "m4a"
    }
}

fn validate_merge_format(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value.trim_start_matches('.').to_ascii_lowercase();
    if !matches!(value.as_str(), "mp4" | "mkv" | "webm") {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("--merge-output-format supports mp4, mkv or webm, got {value}"),
        ));
    }
    Ok(())
}

fn thumbnail_ext(url: &str) -> &'static str {
    let path = url::Url::parse(url)
        .ok()
        .map(|url| url.path().to_ascii_lowercase())
        .unwrap_or_default();
    if path.ends_with(".webp") {
        "webp"
    } else if path.ends_with(".png") {
        "png"
    } else {
        "jpg"
    }
}

fn ensure_extension(path: &Path, ext: &str) -> PathBuf {
    if path
        .extension()
        .is_some_and(|existing| existing.eq_ignore_ascii_case(ext))
    {
        path.to_path_buf()
    } else {
        path.with_extension(ext)
    }
}

fn append_to_stem(path: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    path.with_file_name(format!("{stem}.{suffix}.{ext}"))
}

fn subtitle_language_selected(selection: &str, language: &str) -> bool {
    let selection = selection.trim();
    if selection.eq_ignore_ascii_case("all") {
        return true;
    }
    let language = language.to_ascii_lowercase();
    selection.split(',').map(str::trim).any(|item| {
        if item.is_empty() {
            return false;
        }
        let item = item.to_ascii_lowercase();
        language == item
            || language.starts_with(&format!("{item}-"))
            || item.starts_with(&format!("{language}-"))
    })
}

fn with_subtitle_format(base_url: &str, format: &str) -> Result<String> {
    let mut url = url::Url::parse(base_url)?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "fmt")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    url.query_pairs_mut().append_pair("fmt", format);
    Ok(url.to_string())
}

fn vtt_to_srt(input: &str) -> String {
    let mut output = String::new();
    let mut index = 1usize;
    let mut in_cue = false;
    let mut cue_lines = Vec::new();

    let flush = |output: &mut String, index: &mut usize, lines: &mut Vec<String>| {
        if lines.is_empty() {
            return;
        }
        output.push_str(&format!("{index}\n"));
        for (line_index, line) in lines.iter().enumerate() {
            if line_index == 0 {
                output.push_str(&normalize_vtt_timestamp(line));
            } else {
                output.push_str(line);
            }
            output.push('\n');
        }
        output.push('\n');
        *index += 1;
        lines.clear();
    };

    for line in input.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if in_cue {
                flush(&mut output, &mut index, &mut cue_lines);
                in_cue = false;
            }
            continue;
        }
        if line == "WEBVTT" || line.starts_with("NOTE") || line.starts_with("STYLE") {
            continue;
        }
        if line.contains("-->") {
            if in_cue {
                flush(&mut output, &mut index, &mut cue_lines);
            }
            in_cue = true;
            cue_lines.push(line.to_string());
        } else if in_cue {
            cue_lines.push(line.to_string());
        }
    }
    flush(&mut output, &mut index, &mut cue_lines);
    output
}

fn normalize_vtt_timestamp(line: &str) -> String {
    let Some((start, rest)) = line.split_once("-->") else {
        return line.replace('.', ",");
    };
    let end = rest.split_whitespace().next().unwrap_or(rest);
    format!(
        "{} --> {}",
        start.trim().replace('.', ","),
        end.replace('.', ",")
    )
}

fn print_audio_tracks(info: &VideoInfo) {
    let mut seen = BTreeSet::new();
    for format in &info.formats {
        if !format.has_audio || format.has_video {
            continue;
        }
        let id = format.audio_track_id.clone().unwrap_or_default();
        let name = format.audio_track_name.clone().unwrap_or_default();
        if seen.insert((id.clone(), name.clone())) {
            println!(
                "{}{}{}",
                if id.is_empty() { "-" } else { &id },
                if name.is_empty() {
                    String::new()
                } else {
                    format!("  {name}")
                },
                if format.audio_is_default {
                    "  (default)"
                } else {
                    ""
                }
            );
        }
    }
}

fn apply_audio_track(info: &mut VideoInfo, requested: &str) -> Result<()> {
    let requested = requested.trim();
    let matches = |format: &Format| {
        if requested.eq_ignore_ascii_case("default") {
            return format.audio_is_default;
        }
        format
            .audio_track_id
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(requested))
            || format
                .audio_track_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(requested))
    };

    if !info
        .formats
        .iter()
        .any(|format| format.has_audio && !format.has_video && matches(format))
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("audio track {requested:?} was not found; use --list-audio-tracks"),
        ));
    }

    info.formats
        .retain(|format| !format.has_audio || format.has_video || matches(format));
    Ok(())
}

fn intermediate_path(destination: &Path, kind: &str, itag: u64, ext: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());

    destination.with_file_name(format!(".{name}.{kind}.{itag}.{ext}"))
}

fn init_logging(cli: &Cli) {
    let default = if cli.quiet {
        "error"
    } else if cli.no_warnings {
        "info"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter = std::env::var("YTRUST_LOG")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok())
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new(format!("ytrust={default}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ytrust::youtube::parse_item_ranges;

    #[test]
    fn parses_playlist_ranges() {
        assert_eq!(parse_item_ranges("1-3,5", 10).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_item_ranges("5-", 7).unwrap(), vec![5, 6, 7]);
        assert_eq!(parse_item_ranges("10-", 5).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn converts_vtt_timestamps_to_srt() {
        let srt = vtt_to_srt(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 align:start\nHello\n\n00:00:03.500 --> 00:00:04.000\nWorld\n",
        );
        assert!(srt.contains("00:00:01,000 --> 00:00:02,000\nHello"));
        assert!(!srt.contains("align:start"));
        assert!(srt.contains("00:00:03,500 --> 00:00:04,000\nWorld"));
    }
}
