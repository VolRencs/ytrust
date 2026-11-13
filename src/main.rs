mod cli;

use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::{Level, debug, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use ytrust::api::{self, ResolveOptions, Resolved};
use ytrust::challenge::{ChallengeSolver, SolveReport};
use ytrust::constants::{DEFAULT_AUDIO_FORMAT, DEFAULT_FORMAT};
use ytrust::cookies;
use ytrust::download::{DownloadConfig, DownloadRange, Downloader, audio_codec, parse_range};
use ytrust::error::{ErrorCode, Result, YtrustError};
use ytrust::format::{FormatSelection, FormatSelector, print_formats};
use ytrust::http::RequestConfig;
use ytrust::json;
use ytrust::output::{
    TemplateContext, format_duration, format_ext, merged_ext_multi, render_path,
    sanitize_component, write_json,
};
use ytrust::progress::{
    ComponentSpec, EventSink, ExtraFile, Progress, ProgressMeta, emit_error, emit_playlist,
};
use ytrust::youtube::{self, Format, VideoInfo, fetch_video};

use crate::cli::Cli;

macro_rules! status {
    ($cli:expr, $($arg:tt)*) => {
        if !$cli.quiet {
            let _ = writeln!(std::io::stderr(), $($arg)*);
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonMode {
    None,
    PerVideo,
    Single,
}

#[derive(Debug, Default)]
struct RunReport {
    failed: usize,
}

#[derive(Debug, Clone)]
struct PlaylistContext {
    title: String,
    index: usize,
    count: usize,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_logging(&cli);
    if let Err(error) = cli.validate() {
        let _ = writeln!(std::io::stderr(), "error: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let events = cli.progress_json.then(EventSink::new);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "error: failed to start async runtime: {error}"
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli, events.clone())) {
        Ok(report) if report.failed > 0 => std::process::ExitCode::FAILURE,
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            if err.is_broken_pipe() {
                return std::process::ExitCode::SUCCESS;
            }
            if let Some(events) = &events {
                emit_error(events, err.code().as_str(), &err.to_string());
            }
            let _ = writeln!(std::io::stderr(), "error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(mut cli: Cli, events: Option<EventSink>) -> Result<RunReport> {
    if cli.embed_subs && !cli.write_subs && !cli.write_auto_subs {
        cli.write_subs = true;
    }
    if let Some(audio_format) = &cli.audio_format {
        audio_codec(audio_format.trim_start_matches('.'))?;
    }
    validate_merge_format(cli.merge_output_format.as_deref())?;
    validate_merge_format(cli.remux_video.as_deref())?;
    if let Some(encoding) = &cli.encoding
        && !encoding.eq_ignore_ascii_case("utf-8")
        && !encoding.eq_ignore_ascii_case("utf8")
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("unsupported output encoding: {encoding}; only UTF-8 is supported"),
        ));
    }
    let cookies = cookies::load(cli.cookies.as_deref(), cli.cookies_from_browser.as_deref())?;
    if cookies.header.is_some() {
        debug!("loaded cookies");
    }
    if cli.retry_sleep.is_some() || cli.concurrent_fragments.is_some() {
        debug!("yt-dlp compatibility options are accepted but do not change ytrust scheduling");
    }
    let timeout = cli.socket_timeout.unwrap_or(cli.timeout);
    let request_config = RequestConfig {
        proxy: cli.proxy.clone(),
        cookies: cookies.header.clone(),
        cookie_jar: Some(cookies.cookie_jar.clone()),
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
        cookie_jar: Some(cookies.cookie_jar),
        ffmpeg: cli.ffmpeg_location.clone(),
        force_overwrites: cli.force_overwrites,
        quiet: cli.quiet,
    })?;
    let solver = ChallengeSolver::new(cli.js_runtimes.as_deref()).await?;

    let json_mode = if cli.dump_single_json {
        JsonMode::Single
    } else if cli.dump_json {
        JsonMode::PerVideo
    } else {
        JsonMode::None
    };
    let simulate = cli.simulate;
    let skip_media = cli.skip_download;
    let section = parse_range(cli.download_sections.as_deref().unwrap_or_default())?;
    let options = ResolveOptions {
        no_playlist: cli.no_playlist,
        playlist_limit: cli.playlist_limit,
        playlist_items: cli.playlist_items.clone(),
    };
    let format_expression = if cli.extract_audio || cli.audio_format.is_some() {
        cli.format.as_deref().unwrap_or(DEFAULT_AUDIO_FORMAT)
    } else {
        cli.format.as_deref().unwrap_or(DEFAULT_FORMAT)
    };
    let selector = FormatSelector::parse(format_expression)?;

    let context = RunContext {
        cli: &cli,
        request_config: &request_config,
        downloader: &downloader,
        solver: &solver,
        selector: &selector,
        json_mode,
        simulate,
        skip_media,
        section,
        events: events.as_ref(),
    };

    let mut report = RunReport::default();
    for raw_url in &cli.urls {
        let result = async {
            let resolved = api::resolve(raw_url, &request_config, &options).await?;
            match resolved {
                Resolved::Video { info, original_url } => {
                    process_info(*info, &original_url, None, &context).await
                }
                Resolved::Playlist { info, original_url } => {
                    let failed = process_playlist(info, &original_url, &context).await?;
                    report.failed += failed;
                    Ok(())
                }
            }
        }
        .await;
        if let Err(error) = result {
            if error.is_broken_pipe() {
                return Err(error);
            }
            if cli.ignore_errors {
                report_error(context.events, &error);
                report.failed += 1;
            } else {
                return Err(error);
            }
        }
    }

    Ok(report)
}

struct RunContext<'a> {
    cli: &'a Cli,
    request_config: &'a RequestConfig,
    downloader: &'a Downloader,
    solver: &'a ChallengeSolver,
    selector: &'a FormatSelector,
    json_mode: JsonMode,
    simulate: bool,
    skip_media: bool,
    section: Option<DownloadRange>,
    events: Option<&'a EventSink>,
}

async fn process_playlist(
    playlist: youtube::PlaylistInfo,
    original_url: &str,
    context: &RunContext<'_>,
) -> Result<usize> {
    if context.json_mode == JsonMode::Single {
        write_json(&json::playlist_json(&playlist, original_url))?;
        return Ok(0);
    }

    if context.cli.output == "-" && context.json_mode == JsonMode::None {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "stdout output cannot be used with a playlist; resolve a single video or write to files",
        ));
    }

    if let Some(events) = context.events {
        emit_playlist(
            events,
            &playlist.id,
            &playlist.title,
            playlist.entries.len(),
        );
    }

    if context.json_mode == JsonMode::PerVideo && context.cli.flat_playlist {
        for entry in &playlist.entries {
            write_json(&json::playlist_entry_json(
                entry,
                &playlist.title,
                playlist.entries.len(),
            ))?;
        }
        return Ok(0);
    }

    if playlist.entries.is_empty() {
        warn!("playlist {:?} has no matching entries", playlist.title);
        return Ok(0);
    }

    let total = playlist.entries.len();
    info!(
        "playlist {:?} ({}): processing {} entries",
        playlist.title, playlist.id, total
    );

    let mut failed = 0;
    for (position, entry) in playlist.entries.iter().enumerate() {
        if context.json_mode != JsonMode::PerVideo {
            info!("[{}/{}] {}", position + 1, total, entry.title);
        }
        let result = process_playlist_entry(entry, &playlist, context).await;
        if let Err(error) = result {
            let error = error.context(format!("playlist item {}", entry.index));
            if error.is_broken_pipe() {
                return Err(error);
            }
            if context.cli.ignore_errors {
                report_error(context.events, &error);
                failed += 1;
            } else {
                return Err(error);
            }
        }
    }
    Ok(failed)
}

async fn process_playlist_entry(
    entry: &youtube::PlaylistEntry,
    playlist: &youtube::PlaylistInfo,
    context: &RunContext<'_>,
) -> Result<()> {
    let original = format!("https://www.youtube.com/watch?v={}", entry.id);
    let playlist_context = PlaylistContext {
        title: playlist.title.clone(),
        index: entry.index,
        count: playlist.entries.len(),
    };
    process_video_id(&entry.id, &original, Some(&playlist_context), context).await
}

async fn process_video_id(
    video_id: &str,
    original_url: &str,
    playlist: Option<&PlaylistContext>,
    context: &RunContext<'_>,
) -> Result<()> {
    let info = fetch_video(video_id, context.request_config).await?;
    process_info(info, original_url, playlist, context).await
}

async fn process_info(
    mut info: VideoInfo,
    original_url: &str,
    playlist: Option<&PlaylistContext>,
    context: &RunContext<'_>,
) -> Result<()> {
    let cli = context.cli;
    let downloader = context.downloader;

    if cli.list_audio_tracks {
        print_audio_tracks(&info)?;
        return Ok(());
    }

    if !cli.audio_tracks.is_empty() {
        apply_audio_tracks(&mut info, &cli.audio_tracks)?;
    }

    let solve_needed = context.json_mode != JsonMode::None
        || (!cli.list_formats && !context.simulate && !context.skip_media);
    let solve_report = if solve_needed {
        Some(
            context
                .solver
                .solve_formats(&mut info, context.request_config)
                .await?,
        )
    } else {
        None
    };

    if context.json_mode != JsonMode::None {
        let value = json::video_json_with_playlist(
            &info,
            original_url,
            playlist.map(|playlist| (playlist.title.as_str(), playlist.index, playlist.count)),
        );
        write_json(&value)?;
        return Ok(());
    }

    if cli.list_formats {
        ytrust::output::write_line(&format!("\n{} [{}]", info.title, info.id))?;
        print_formats(&info)?;
        return Ok(());
    }

    if context.events.is_some() && cli.output == "-" {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "--progress-json cannot be combined with streaming media to stdout",
        ));
    }

    if context.simulate {
        let progress = make_progress(context.events, &info, original_url, playlist, Vec::new());
        if let Some(progress) = &progress {
            progress.done(None, "media", false, true, None);
        }
        status!(cli, "simulate: {} [{}]", info.title, info.id);
        return Ok(());
    }

    if context.skip_media {
        if cli.output == "-" {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "stdout output requires a media download",
            ));
        }
        let progress = make_progress(context.events, &info, original_url, playlist, Vec::new());
        let extras = write_extras(&info, playlist, cli, downloader, progress.as_ref()).await?;
        if let Some(progress) = &progress {
            if extras.is_empty() {
                progress.done(None, "skip", false, false, None);
            } else {
                for extra in &extras {
                    progress.done(
                        Some(&extra.path),
                        &extra.kind,
                        false,
                        false,
                        extra.language.as_deref(),
                    );
                }
            }
        }
        return Ok(());
    }

    let selection = context
        .selector
        .select(&info.formats)
        .map_err(|error| challenge_context(error, solve_report.as_ref()))?;
    let audio_mode = context.extract_audio() || selection.video.is_none();

    if !audio_mode
        && selection.video.is_some()
        && selection.audios.len() > 1
        && !cli.audio_multistreams
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "multiple audio tracks selected; pass --audio-multistreams to merge them",
        ));
    }

    if audio_mode
        && selection.audios.is_empty()
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

    let duration_secs = context
        .section
        .and_then(|range| range.duration(info.duration_secs))
        .or(info.duration_secs);
    let use_content_length = context.section.is_none();
    let components = if audio_mode {
        audio_components(&selection, duration_secs, use_content_length)
    } else {
        video_components(&selection, duration_secs, use_content_length)
    };
    let progress = make_progress(context.events, &info, original_url, playlist, components);

    if cli.output == "-" {
        if cli.embed_subs || cli.remux_video.is_some() {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "--embed-subs/--remux-video are not supported with stdout output",
            ));
        }
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

    let extras = write_extras(&info, playlist, cli, downloader, progress.as_ref()).await?;

    if audio_mode {
        return download_audio_only(
            &info,
            &selection,
            playlist,
            cli,
            downloader,
            duration_secs,
            context.section,
            progress.as_ref(),
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
        &extras,
        progress.as_ref(),
    )
    .await
}

impl RunContext<'_> {
    fn extract_audio(&self) -> bool {
        self.cli.extract_audio || self.cli.audio_format.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_video(
    info: &VideoInfo,
    selection: &FormatSelection,
    playlist: Option<&PlaylistContext>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
    section: Option<DownloadRange>,
    extras: &[ExtraFile],
    progress: Option<&Progress>,
) -> Result<()> {
    let video = selection
        .video
        .as_ref()
        .ok_or_else(|| YtrustError::coded(ErrorCode::InvalidInput, "no video format selected"))?;
    let container_override = cli
        .remux_video
        .as_deref()
        .or(cli.merge_output_format.as_deref());
    let (ext, context_format) = if selection.needs_merge() {
        let audio_exts: Vec<&str> = selection
            .audios
            .iter()
            .map(|audio| format_ext(audio))
            .collect();
        let ext = merged_ext_multi(format_ext(video), &audio_exts, container_override);
        (ext, video.clone())
    } else {
        (format_ext(video).to_string(), video.clone())
    };

    let mut context = TemplateContext::from_info(info, &ext).with_format(&context_format);
    if let Some(playlist) = playlist {
        context = context.with_playlist(&playlist.title, Some(playlist.index));
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

    if path_is_file(&destination).await && !cli.force_overwrites {
        status!(cli, "already downloaded: {}", destination.to_string_lossy());
        if let Some(progress) = progress {
            progress.done(Some(&destination), "video", true, false, None);
        }
        return Ok(());
    }

    let label = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&info.id)
        .to_string();

    debug!(
        "selected formats: video={:?}, audios={:?}",
        selection.video.as_ref().map(|format| format.itag),
        selection
            .audios
            .iter()
            .map(|format| format.itag)
            .collect::<Vec<_>>()
    );

    if !selection.needs_merge() {
        status!(
            cli,
            "downloading: {} [{}] ({})",
            info.title,
            info.id,
            format_duration(duration_secs.unwrap_or(0))
        );
        downloader
            .download_format(
                video,
                &destination,
                &label,
                duration_secs,
                section,
                progress,
                "media",
            )
            .await?;
        let final_path = postprocess_video(
            &destination,
            cli,
            downloader,
            duration_secs,
            extras,
            progress,
        )
        .await?;
        if let Some(progress) = progress {
            progress.done(Some(&final_path), "video", false, false, None);
        }
        return Ok(());
    }

    let video_path = intermediate_path(&destination, "video", video.itag, None, format_ext(video));
    let audio_paths: Vec<PathBuf> = selection
        .audios
        .iter()
        .enumerate()
        .map(|(index, audio)| {
            intermediate_path(
                &destination,
                "audio",
                audio.itag,
                Some(index),
                format_ext(audio),
            )
        })
        .collect();

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let audio_exts = selection
        .audios
        .iter()
        .map(format_ext)
        .collect::<Vec<_>>()
        .join(" + ");
    status!(
        cli,
        "downloading: {} [{}] ({} + {})",
        info.title,
        info.id,
        format_ext(video),
        audio_exts
    );

    let video_label = format!("{label} [video]");
    let audio_labels: Vec<String> = (0..selection.audios.len())
        .map(|index| format!("{label} [audio {}]", index + 1))
        .collect();
    let audio_components: Vec<String> = (0..selection.audios.len())
        .map(audio_component_name)
        .collect();
    let mut downloads = Vec::with_capacity(1 + selection.audios.len());
    downloads.push(downloader.download_format(
        video,
        &video_path,
        &video_label,
        duration_secs,
        section,
        progress,
        "video",
    ));
    for (index, audio) in selection.audios.iter().enumerate() {
        downloads.push(downloader.download_format(
            audio,
            &audio_paths[index],
            &audio_labels[index],
            duration_secs,
            section,
            progress,
            &audio_components[index],
        ));
    }
    for result in futures_util::future::join_all(downloads).await {
        result?;
    }

    let audio_tracks: Vec<(PathBuf, Option<String>)> = audio_paths
        .iter()
        .cloned()
        .zip(selection.audios.iter().map(|audio| audio.language.clone()))
        .collect();
    let result = downloader
        .merge(
            &video_path,
            &audio_tracks,
            &destination,
            &format!("{} [merge]", label),
            duration_secs,
            progress,
        )
        .await;

    let _ = tokio::fs::remove_file(&video_path).await;
    for path in &audio_paths {
        let _ = tokio::fs::remove_file(path).await;
    }
    result?;
    let final_path = postprocess_video(
        &destination,
        cli,
        downloader,
        duration_secs,
        extras,
        progress,
    )
    .await?;
    if let Some(progress) = progress {
        progress.done(Some(&final_path), "video", false, false, None);
    }
    Ok(())
}

async fn postprocess_video(
    destination: &Path,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
    extras: &[ExtraFile],
    progress: Option<&Progress>,
) -> Result<PathBuf> {
    let mut current = destination.to_path_buf();
    if let Some(container) = cli.remux_video.as_deref() {
        current = downloader
            .remux(&current, container, duration_secs, progress)
            .await?;
    }
    if cli.embed_subs {
        current = downloader
            .embed_subtitles(&current, extras, duration_secs, progress)
            .await?;
    }
    Ok(current)
}

#[allow(clippy::too_many_arguments)]
async fn download_audio_only(
    info: &VideoInfo,
    selection: &FormatSelection,
    playlist: Option<&PlaylistContext>,
    cli: &Cli,
    downloader: &Downloader,
    duration_secs: Option<u64>,
    section: Option<DownloadRange>,
    progress: Option<&Progress>,
) -> Result<()> {
    let sources: Vec<&Format> = if !selection.audios.is_empty() {
        selection.audios.iter().collect()
    } else if let Some(video) = selection.video.as_ref() {
        vec![video]
    } else {
        return Err(YtrustError::coded(
            ErrorCode::NoAudio,
            "no audio format selected",
        ));
    };
    let multiple = sources.len() > 1;
    let mut used_suffixes = BTreeSet::new();

    status!(cli, "downloading audio: {} [{}]", info.title, info.id);

    for (index, audio) in sources.iter().enumerate() {
        let source_ext = format_ext(audio);
        let extracted = audio.has_video;
        let target_ext = if let Some(audio_format) = cli.audio_format.as_deref() {
            audio_format.trim_start_matches('.').to_ascii_lowercase()
        } else if extracted {
            default_audio_ext(audio).to_string()
        } else {
            source_ext.to_string()
        };
        let convert = extracted || (cli.audio_format.is_some() && target_ext != source_ext);

        let mut context = TemplateContext::from_info(info, &target_ext).with_format(audio);
        if let Some(playlist) = playlist {
            context = context.with_playlist(&playlist.title, Some(playlist.index));
        }
        let mut destination = render_path(
            &cli.output,
            &context,
            cli.output_dir.as_deref(),
            cli.restrict_filenames,
        )?;

        if multiple {
            let mut suffix = audio
                .language
                .clone()
                .unwrap_or_else(|| format!("track{}", audio.itag));
            if !used_suffixes.insert(suffix.clone()) {
                suffix.push_str(&format!(".{}", index + 1));
            }
            destination = append_to_stem(&destination, &suffix, &target_ext);
        }

        if destination == Path::new("-") {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "stdout output is not supported for audio downloads yet",
            ));
        }

        if path_is_file(&destination).await && !cli.force_overwrites {
            status!(cli, "already downloaded: {}", destination.to_string_lossy());
            if let Some(progress) = progress {
                progress.done(
                    Some(&destination),
                    "audio",
                    true,
                    false,
                    audio.language.as_deref(),
                );
            }
            continue;
        }

        let label = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&info.id)
            .to_string();
        let component = if selection.audios.is_empty() {
            "media".to_string()
        } else {
            audio_component_name(index)
        };

        if !convert {
            if cli.audio_quality.is_some() {
                status!(
                    cli,
                    "note: --audio-quality is ignored because the source audio is already {}",
                    target_ext
                );
            }
            downloader
                .download_format(
                    audio,
                    &destination,
                    &label,
                    duration_secs,
                    section,
                    progress,
                    &component,
                )
                .await?;
            if let Some(progress) = progress {
                progress.done(
                    Some(&destination),
                    "audio",
                    false,
                    false,
                    audio.language.as_deref(),
                );
            }
            continue;
        }

        if !downloader.ffmpeg_available().await {
            return Err(YtrustError::FfmpegMissing);
        }

        let source_path =
            intermediate_path(&destination, "source", audio.itag, Some(index), source_ext);
        downloader
            .download_format(
                audio,
                &source_path,
                &format!("{label} [audio]"),
                duration_secs,
                section,
                progress,
                &component,
            )
            .await?;

        let result = downloader
            .convert_audio(
                &source_path,
                &destination,
                &target_ext,
                &format!("{label} [{target_ext}]"),
                duration_secs,
                cli.audio_quality.as_deref(),
                progress,
            )
            .await;
        let _ = tokio::fs::remove_file(&source_path).await;
        result?;
        if let Some(progress) = progress {
            progress.done(
                Some(&destination),
                "audio",
                false,
                false,
                audio.language.as_deref(),
            );
        }
    }
    Ok(())
}

async fn write_extras(
    info: &VideoInfo,
    playlist: Option<&PlaylistContext>,
    cli: &Cli,
    downloader: &Downloader,
    progress: Option<&Progress>,
) -> Result<Vec<ExtraFile>> {
    let mut extras = Vec::new();
    if !cli.write_thumbnail && !cli.write_all_thumbnails && !cli.write_subs && !cli.write_auto_subs
    {
        return Ok(extras);
    }

    let mut ctx = TemplateContext::from_info(info, "jpg");
    if let Some(playlist) = playlist {
        ctx = ctx.with_playlist(&playlist.title, Some(playlist.index));
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
            if let Some(progress) = progress {
                progress.file("thumbnail", &destination, None, false);
            }
            extras.push(ExtraFile {
                kind: "thumbnail".to_string(),
                path: destination,
                language: None,
                auto: false,
            });
        }
    } else if cli.write_thumbnail
        && let Some(thumb) = info
            .thumbnails
            .iter()
            .max_by_key(|thumb| thumb.width.unwrap_or(0) * thumb.height.unwrap_or(0))
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
        if let Some(progress) = progress {
            progress.file("thumbnail", &destination, None, false);
        }
        extras.push(ExtraFile {
            kind: "thumbnail".to_string(),
            path: destination,
            language: None,
            auto: false,
        });
    }

    let mut tracks = Vec::new();
    if cli.write_subs {
        tracks.extend(info.subtitles.iter().map(|track| (track, false)));
    }
    if cli.write_auto_subs {
        tracks.extend(info.automatic_captions.iter().map(|track| (track, true)));
    }
    if tracks.is_empty() {
        return Ok(extras);
    }

    let format = cli
        .convert_subs
        .as_deref()
        .unwrap_or(&cli.sub_format)
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if format != "vtt" && format != "srt" {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "--sub-format/--convert-subs supports vtt or srt",
        ));
    }

    let mut used = std::collections::BTreeMap::new();
    for (track, is_auto) in tracks {
        if !subtitle_language_selected(&cli.sub_langs, &track.language) {
            continue;
        }
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
        if path_is_file(&destination).await && !cli.force_overwrites {
            status!(
                cli,
                "subtitle already exists: {}",
                destination.to_string_lossy()
            );
        } else {
            let url = with_subtitle_format(&track.url, "vtt")?;
            let body = downloader.fetch_text(&url).await?;
            let body = if format == "srt" {
                vtt_to_srt(&body)
            } else {
                body
            };
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let temp = destination.with_extension(format!("{format}.part"));
            tokio::fs::write(&temp, body).await?;
            if path_is_file(&destination).await {
                let _ = tokio::fs::remove_file(&destination).await;
            }
            tokio::fs::rename(&temp, &destination).await?;
            status!(cli, "subtitle: {}", destination.to_string_lossy());
        }
        if let Some(progress) = progress {
            progress.file("subtitle", &destination, Some(&track.language), is_auto);
        }
        extras.push(ExtraFile {
            kind: "subtitle".to_string(),
            path: destination,
            language: Some(track.language.clone()),
            auto: is_auto,
        });
    }

    Ok(extras)
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
    let suffix = sanitize_component(suffix, true);
    let suffix = if suffix.is_empty() {
        "file".to_string()
    } else {
        suffix
    };
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

fn print_audio_tracks(info: &VideoInfo) -> Result<()> {
    use std::io::Write;

    let mut out = std::io::stdout().lock();
    let mut seen = BTreeSet::new();
    for format in &info.formats {
        if !format.has_audio || format.has_video {
            continue;
        }
        let id = format.audio_track_id.clone().unwrap_or_default();
        let name = format.audio_track_name.clone().unwrap_or_default();
        let language = format.language.clone().unwrap_or_default();
        if seen.insert((id.clone(), name.clone(), language.clone())) {
            writeln!(
                out,
                "{}{}{}{}",
                if id.is_empty() { "-" } else { &id },
                if language.is_empty() {
                    String::new()
                } else {
                    format!("  [{language}]")
                },
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
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

fn apply_audio_tracks(info: &mut VideoInfo, requested: &[String]) -> Result<()> {
    let requested: Vec<&str> = requested
        .iter()
        .map(|track| track.trim())
        .filter(|track| !track.is_empty())
        .collect();
    if requested.is_empty() {
        return Ok(());
    }
    let matches = |format: &Format, requested: &str| {
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
            || format
                .language
                .as_deref()
                .is_some_and(|language| language.eq_ignore_ascii_case(requested))
    };

    for requested_track in &requested {
        if !info
            .formats
            .iter()
            .any(|format| format.has_audio && !format.has_video && matches(format, requested_track))
        {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("audio track {requested_track:?} was not found; use --list-audio-tracks"),
            ));
        }
    }

    info.formats.retain(|format| {
        !format.has_audio
            || format.has_video
            || requested.iter().any(|requested| matches(format, requested))
    });
    Ok(())
}

fn audio_component_name(index: usize) -> String {
    if index == 0 {
        "audio".to_string()
    } else {
        format!("audio{}", index + 1)
    }
}

fn video_components(
    selection: &FormatSelection,
    duration_secs: Option<u64>,
    use_content_length: bool,
) -> Vec<ComponentSpec> {
    let size = |format: &Format| {
        if use_content_length {
            format.estimated_size(duration_secs)
        } else {
            format.progress_size(duration_secs)
        }
    };
    if let Some(video) = selection.video.as_ref() {
        if selection.audios.is_empty() {
            return vec![ComponentSpec::new("media", size(video))];
        }
        let mut components = Vec::with_capacity(1 + selection.audios.len());
        components.push(ComponentSpec::new("video", size(video)));
        components.extend(
            selection
                .audios
                .iter()
                .enumerate()
                .map(|(index, audio)| ComponentSpec::new(audio_component_name(index), size(audio))),
        );
        return components;
    }
    Vec::new()
}

fn audio_components(
    selection: &FormatSelection,
    duration_secs: Option<u64>,
    use_content_length: bool,
) -> Vec<ComponentSpec> {
    let size = |format: &Format| {
        if use_content_length {
            format.estimated_size(duration_secs)
        } else {
            format.progress_size(duration_secs)
        }
    };
    if selection.audios.is_empty() {
        return selection
            .video
            .as_ref()
            .map(|video| vec![ComponentSpec::new("media", size(video))])
            .unwrap_or_default();
    }
    selection
        .audios
        .iter()
        .enumerate()
        .map(|(index, audio)| ComponentSpec::new(audio_component_name(index), size(audio)))
        .collect()
}

fn make_progress(
    events: Option<&EventSink>,
    info: &VideoInfo,
    original_url: &str,
    playlist: Option<&PlaylistContext>,
    components: Vec<ComponentSpec>,
) -> Option<Progress> {
    events.map(|events| {
        Progress::new(
            events.clone(),
            ProgressMeta {
                id: info.id.clone(),
                title: info.title.clone(),
                original_url: original_url.to_string(),
                playlist_index: playlist.map(|playlist| playlist.index),
                playlist_count: playlist.map(|playlist| playlist.count),
            },
            components,
        )
    })
}

fn report_error(events: Option<&EventSink>, error: &YtrustError) {
    if let Some(events) = events {
        emit_error(events, error.code().as_str(), &error.to_string());
    } else {
        let _ = writeln!(std::io::stderr(), "error: {error}");
    }
}

fn challenge_context(error: YtrustError, report: Option<&SolveReport>) -> YtrustError {
    let Some(report) = report else {
        return error;
    };
    if report.runtime_missing {
        return YtrustError::coded(
            ErrorCode::ChallengeFailed,
            format!(
                "ciphered formats need a JavaScript runtime; install Node.js 20+ or pass --js-runtimes ({error})"
            ),
        );
    }
    if let Some(message) = &report.error {
        return YtrustError::coded(
            ErrorCode::ChallengeFailed,
            format!("challenge solving failed: {message} ({error})"),
        );
    }
    if report.unresolved > 0 {
        return YtrustError::coded(
            ErrorCode::ChallengeFailed,
            format!(
                "challenge solving left {} unresolved format(s) ({error})",
                report.unresolved
            ),
        );
    }
    error
}

async fn path_is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn intermediate_path(
    destination: &Path,
    kind: &str,
    itag: u64,
    index: Option<usize>,
    ext: &str,
) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let index = index
        .map(|index| format!(".{}", index + 1))
        .unwrap_or_default();
    let pid = std::process::id();

    destination.with_file_name(format!(".{name}.{kind}.{itag}{index}.{pid}.{ext}"))
}

fn init_logging(cli: &Cli) {
    let default = if cli.quiet {
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
    let suppress_warnings = cli.no_warnings && !cli.quiet;
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .with_filter(filter);
    let no_warnings =
        filter_fn(move |metadata| !suppress_warnings || *metadata.level() != Level::WARN);
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(no_warnings)
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
