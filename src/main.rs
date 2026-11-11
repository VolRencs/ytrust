mod cli;
mod cookies;
mod download;
mod error;
mod format;
mod json;
mod output;
mod youtube;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::download::{DownloadConfig, Downloader};
use crate::error::{Result, YtrustError};
use crate::format::{FormatSelection, print_formats, select};
use crate::output::{TemplateContext, format_duration, format_ext, merged_ext, render_path};
use crate::youtube::{RequestConfig, Target, classify, fetch_playlist, fetch_search, fetch_video};

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
    let cookie_header = cookies::load(cli.cookies.as_deref(), cli.cookies_from_browser.as_deref())?;
    if cookie_header.is_some() {
        debug!("loaded cookies");
    }

    let timeout = cli.socket_timeout.unwrap_or(cli.timeout);
    let request_config = RequestConfig {
        proxy: cli.proxy.clone(),
        cookies: cookie_header.clone(),
        timeout,
    };
    let downloader = Downloader::new(DownloadConfig {
        retries: cli.retries,
        timeout,
        proxy: cli.proxy.clone(),
        cookies: cookie_header,
        ffmpeg: cli.ffmpeg_location.clone(),
        force_overwrites: cli.force_overwrites,
    })?;
    let ffmpeg_available = downloader.ffmpeg_available().await;

    let dump_json = cli.dump_json || cli.dump_single_json;
    let simulate = cli.simulate || cli.skip_download;
    let context = RunContext {
        cli: &cli,
        request_config: &request_config,
        downloader: &downloader,
        ffmpeg_available,
        simulate,
        dump_json,
    };

    for raw_url in &cli.urls {
        let target = classify(raw_url, cli.no_playlist)?;
        match target {
            Target::Video { id } => {
                process_video(&id, None, &context).await?;
            }
            Target::Playlist { id } => {
                debug!("fetching playlist {id}");
                let fetch_limit = max_playlist_item(&cli.playlist_items)
                    .map(|limit| (limit as u64).min(cli.playlist_limit))
                    .unwrap_or(cli.playlist_limit);
                let mut playlist = fetch_playlist(&id, &request_config, fetch_limit).await?;
                if dump_json {
                    filter_playlist_entries(&mut playlist, &cli.playlist_items)?;
                    println!(
                        "{}",
                        serde_json::to_string(&json::playlist_json(&playlist, raw_url))?
                    );
                    continue;
                }
                let selected = parse_item_ranges(&cli.playlist_items, playlist.entries.len())?;
                if selected.is_empty() {
                    warn!("playlist {:?} has no matching entries", playlist.title);
                    continue;
                }

                info!(
                    "playlist {:?} ({}): downloading {} of {} entries",
                    playlist.title,
                    playlist.id,
                    selected.len(),
                    playlist.entries.len()
                );

                for index in selected {
                    let Some(entry) = playlist.entries.get(index - 1) else {
                        continue;
                    };
                    info!(
                        "[{}/{}] {}",
                        entry.index,
                        playlist.entries.len(),
                        entry.title
                    );
                    let playlist_context = (playlist.title.clone(), entry.index);
                    process_video(&entry.id, Some(playlist_context), &context).await?;
                }
            }
            Target::Search { query, limit } => {
                let mut playlist = fetch_search(&query, limit, &request_config).await?;
                if dump_json {
                    filter_playlist_entries(&mut playlist, &cli.playlist_items)?;
                    println!(
                        "{}",
                        serde_json::to_string(&json::playlist_json(&playlist, raw_url))?
                    );
                    continue;
                }
                for entry in playlist.entries.iter().take(limit as usize) {
                    let playlist_context = (playlist.title.clone(), entry.index);
                    process_video(&entry.id, Some(playlist_context), &context).await?;
                }
            }
        }
    }

    Ok(())
}

struct RunContext<'a> {
    cli: &'a Cli,
    request_config: &'a RequestConfig,
    downloader: &'a Downloader,
    ffmpeg_available: bool,
    simulate: bool,
    dump_json: bool,
}

async fn process_video(
    video_id: &str,
    playlist: Option<(String, usize)>,
    context: &RunContext<'_>,
) -> Result<()> {
    let cli = context.cli;
    let downloader = context.downloader;
    let simulate = context.simulate;
    let dump_json = context.dump_json;
    let mut info = fetch_video(video_id, context.request_config).await?;

    if cli.list_audio_tracks {
        print_audio_tracks(&info);
        return Ok(());
    }

    if dump_json {
        println!(
            "{}",
            serde_json::to_string(&json::video_json(&info, &info.webpage_url))?
        );
        return Ok(());
    }

    if let Some(track) = &cli.audio_track {
        apply_audio_track(&mut info, track)?;
    }

    if cli.list_formats {
        println!("\n{} [{}]", info.title, info.id);
        print_formats(&info);
        return Ok(());
    }

    write_extras(&info, playlist.as_ref(), cli, downloader).await?;

    if simulate {
        eprintln!("simulate: {} [{}]", info.title, info.id);
        return Ok(());
    }

    let expression = if cli.extract_audio {
        "bestaudio"
    } else {
        cli.format.as_str()
    };

    let selection = select(&info, expression)?;

    if selection.needs_merge() && !context.ffmpeg_available {
        return Err(YtrustError::msg(
            "ffmpeg is required to merge separate video and audio streams; install ffmpeg or choose a single format with -f",
        ));
    }

    if cli.extract_audio && selection.audio.is_none() {
        return Err(YtrustError::msg(format!(
            "no audio-only format found for {}",
            info.id
        )));
    }

    if cli.output == "-" {
        let format = selection.single().ok_or_else(|| {
            YtrustError::msg("streaming merged video+audio to stdout is not supported")
        })?;
        let offset = parse_section_start(cli.download_sections.as_deref())?;
        return downloader.stream_to_stdout(format, offset, &info.id).await;
    }

    let duration_secs = info.duration_secs;

    if cli.extract_audio {
        return download_audio_only(&info, &selection, playlist, cli, downloader, duration_secs)
            .await;
    }

    download_video(&info, &selection, playlist, cli, downloader, duration_secs).await
}

async fn download_video(
    info: &youtube::VideoInfo,
    selection: &FormatSelection,
    playlist: Option<(String, usize)>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
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
            .ok_or_else(|| YtrustError::msg("no format selected"))?;
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
        return Err(YtrustError::msg(
            "stdout output is not supported for video downloads yet",
        ));
    }

    if destination.exists() && !cli.force_overwrites {
        eprintln!("already downloaded: {}", destination.to_string_lossy());
        return Ok(());
    }

    let label = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&info.id)
        .to_string();

    debug!("selected format: {selection:?}");

    if !selection.needs_merge() {
        let format = selection
            .single()
            .ok_or_else(|| YtrustError::msg("no format selected"))?;
        eprintln!(
            "downloading: {} [{}] ({})",
            info.title,
            info.id,
            format_duration(duration_secs.unwrap_or(0))
        );
        return downloader
            .download_format(format, &destination, &label, duration_secs)
            .await;
    }

    let video = selection.video.as_ref().expect("video");
    let audio = selection.audio.as_ref().expect("audio");
    let video_path = intermediate_path(&destination, "video", video.itag, format_ext(video));
    let audio_path = intermediate_path(&destination, "audio", audio.itag, format_ext(audio));

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    eprintln!(
        "downloading: {} [{}] ({} + {})",
        info.title,
        info.id,
        format_ext(video),
        format_ext(audio)
    );

    let video_label = format!("{} [video]", label);
    let audio_label = format!("{} [audio]", label);
    let video_download =
        downloader.download_format(video, &video_path, &video_label, duration_secs);
    let audio_download =
        downloader.download_format(audio, &audio_path, &audio_label, duration_secs);

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
    info: &youtube::VideoInfo,
    selection: &FormatSelection,
    playlist: Option<(String, usize)>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
) -> Result<()> {
    let audio = selection
        .audio
        .as_ref()
        .or(selection.video.as_ref())
        .ok_or_else(|| YtrustError::msg("no audio format selected"))?;

    let source_ext = format_ext(audio);
    let target_ext = cli
        .audio_format
        .as_deref()
        .unwrap_or(source_ext)
        .trim_start_matches('.')
        .to_string();

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
        return Err(YtrustError::msg(
            "stdout output is not supported for audio downloads yet",
        ));
    }

    if destination.exists() && !cli.force_overwrites {
        eprintln!("already downloaded: {}", destination.to_string_lossy());
        return Ok(());
    }

    let label = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&info.id)
        .to_string();

    eprintln!("downloading audio: {} [{}]", info.title, info.id);

    if cli.audio_format.is_none() {
        return downloader
            .download_format(audio, &destination, &label, duration_secs)
            .await;
    }

    let source_path = intermediate_path(&destination, "source", audio.itag, source_ext);
    downloader
        .download_format(
            audio,
            &source_path,
            &format!("{label} [audio]"),
            duration_secs,
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
    info: &youtube::VideoInfo,
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
        for (index, thumb) in info.thumbnails.iter().enumerate() {
            let ext = thumbnail_ext(&thumb.url);
            let base = render_path(
                &cli.output,
                &TemplateContext {
                    ext: ext.to_string(),
                    ..ctx.clone()
                },
                cli.output_dir.as_deref(),
                cli.restrict_filenames,
            )?;
            let suffix = match (thumb.width, thumb.height) {
                (Some(width), Some(height)) => format!("{width}x{height}"),
                _ => index.to_string(),
            };
            let destination = append_to_stem(&base, &suffix, ext);
            downloader
                .download_url(&thumb.url, &destination, &destination.to_string_lossy())
                .await?;
        }
    } else if cli.write_thumbnail
        && let Some(thumb) = info.thumbnails.last()
    {
        let ext = thumbnail_ext(&thumb.url);
        let destination = render_path(
            &cli.output,
            &TemplateContext {
                ext: ext.to_string(),
                ..ctx.clone()
            },
            cli.output_dir.as_deref(),
            cli.restrict_filenames,
        )?;
        downloader
            .download_url(&thumb.url, &destination, &destination.to_string_lossy())
            .await?;
    }

    let mut tracks = Vec::new();
    if cli.write_subs {
        tracks.extend(info.subtitles.iter());
    }
    if cli.write_auto_subs {
        tracks.extend(info.automatic_captions.iter());
    }
    if tracks.is_empty() {
        return Ok(());
    }

    let format = cli.sub_format.to_ascii_lowercase();
    if format != "vtt" && format != "srt" {
        return Err(YtrustError::msg("--sub-format supports vtt or srt"));
    }

    for track in tracks {
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
            &TemplateContext {
                ext: format.clone(),
                ..ctx.clone()
            },
            cli.output_dir.as_deref(),
            cli.restrict_filenames,
        )?;
        let destination = append_to_stem(&base, &track.language, &format);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&destination, body).await?;
        eprintln!("subtitle: {}", destination.to_string_lossy());
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
    selection.split(',').map(str::trim).any(|item| {
        !item.is_empty()
            && (language.eq_ignore_ascii_case(item)
                || language
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}-", item.to_ascii_lowercase())))
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
                output.push_str(&line.replace('.', ","));
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

fn print_audio_tracks(info: &youtube::VideoInfo) {
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

fn apply_audio_track(info: &mut youtube::VideoInfo, requested: &str) -> Result<()> {
    let requested = requested.trim();
    let matches = |format: &youtube::Format| {
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
        return Err(YtrustError::msg(format!(
            "audio track {requested:?} was not found; use --list-audio-tracks"
        )));
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

fn max_playlist_item(expression: &str) -> Option<usize> {
    let mut max = None;
    for part in expression
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            if start.trim().is_empty() || end.trim().is_empty() {
                return None;
            }
            let end = end.trim().parse::<usize>().ok()?;
            max = Some(max.map_or(end, |current: usize| current.max(end)));
        } else {
            let index = part.parse::<usize>().ok()?;
            max = Some(max.map_or(index, |current: usize| current.max(index)));
        }
    }
    max
}

fn filter_playlist_entries(playlist: &mut youtube::PlaylistInfo, selection: &str) -> Result<()> {
    let selected = parse_item_ranges(selection, playlist.entries.len())?;
    if selected.is_empty() {
        playlist.entries.clear();
        return Ok(());
    }
    let selected: BTreeSet<usize> = selected.into_iter().collect();
    playlist
        .entries
        .retain(|entry| selected.contains(&entry.index));
    Ok(())
}

fn parse_section_start(value: Option<&str>) -> Result<Option<u64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let first = value.split(',').next().unwrap_or(value).trim();
    let first = first.strip_prefix('*').unwrap_or(first);
    let (start, _) = first
        .split_once('-')
        .ok_or_else(|| YtrustError::msg(format!("invalid download section: {value}")))?;
    if start.trim().is_empty() {
        return Ok(None);
    }
    let seconds = start
        .trim()
        .parse::<f64>()
        .map_err(|_| YtrustError::msg(format!("invalid download section start: {start}")))?;
    Ok((seconds > 0.0).then_some(seconds.floor() as u64))
}

fn parse_item_ranges(expression: &str, total: usize) -> Result<Vec<usize>> {
    let mut selected = BTreeSet::new();

    for part in expression
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start = if start.trim().is_empty() {
                1
            } else {
                start
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| YtrustError::msg(format!("invalid playlist range: {part}")))?
            };
            let end = if end.trim().is_empty() {
                total
            } else {
                end.trim()
                    .parse::<usize>()
                    .map_err(|_| YtrustError::msg(format!("invalid playlist range: {part}")))?
            };
            if start == 0 || end == 0 || start > end {
                return Err(YtrustError::msg(format!("invalid playlist range: {part}")));
            }
            for index in start..=end.min(total) {
                selected.insert(index);
            }
        } else {
            let index = part
                .parse::<usize>()
                .map_err(|_| YtrustError::msg(format!("invalid playlist item: {part}")))?;
            if index == 0 {
                return Err(YtrustError::msg("playlist items start at 1"));
            }
            if index <= total {
                selected.insert(index);
            }
        }
    }

    Ok(selected.into_iter().collect())
}

fn init_logging(cli: &Cli) {
    let default = if cli.quiet || cli.no_warnings {
        "error"
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

    #[test]
    fn parses_playlist_ranges() {
        assert_eq!(parse_item_ranges("1-3,5", 10).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_item_ranges("5-", 7).unwrap(), vec![5, 6, 7]);
    }
}
