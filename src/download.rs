use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::constants::WEB_UA;
use crate::error::{ErrorCode, Result, YtrustError};
use crate::http::is_youtube_host;
use crate::progress::{ExtraFile, Progress};
use crate::youtube::Format;

const MAX_FFMPEG_STDERR_BYTES: usize = 64 * 1024;
/// Fresh googlevideo URLs are redirected to a cold edge that answers 403
/// until it has fetched the stream. YouTube expects clients to retry the
/// original URL after a short pause, so keep a small budget even when the
/// caller disabled user-level retries.
const COLD_START_MIN_ATTEMPTS: u32 = 5;
const COLD_START_BASE_DELAY_MS: u64 = 500;

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub retries: u32,
    pub timeout: u64,
    pub proxy: Option<String>,
    pub cookies: Option<String>,
    pub cookie_jar: Option<crate::cookies::CookieJar>,
    pub ffmpeg: Option<PathBuf>,
    pub force_overwrites: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownloadRange {
    pub start: f64,
    pub end: Option<f64>,
}

impl DownloadRange {
    pub fn duration(&self, total: Option<u64>) -> Option<u64> {
        if let Some(end) = self.end {
            return Some((end - self.start).max(0.0).ceil() as u64);
        }
        total.map(|total| (total as f64 - self.start).max(1.0).ceil() as u64)
    }
}

/// Parse a yt-dlp-style `--download-sections` value.
///
/// Only one contiguous section is supported; multiple sections would produce
/// multiple output files, which is intentionally rejected instead of silently
/// ignoring everything after the first comma.
pub fn parse_range(value: &str) -> Result<Option<DownloadRange>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let mut sections = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let first = sections.next().unwrap_or_default();
    if sections.next().is_some() {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            "multiple download sections are not supported",
        ));
    }
    let first = first.strip_prefix('*').unwrap_or(first);
    let (start, end) = first.split_once('-').ok_or_else(|| {
        YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("invalid download section: {value}"),
        )
    })?;
    let start = if start.trim().is_empty() {
        0.0
    } else {
        parse_time(start.trim()).ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("invalid download section start: {start}"),
            )
        })?
    };
    let end = if end.trim().is_empty() || end.trim().eq_ignore_ascii_case("inf") {
        None
    } else {
        Some(parse_time(end.trim()).ok_or_else(|| {
            YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("invalid download section end: {end}"),
            )
        })?)
    };
    if let Some(end) = end
        && end <= start
    {
        return Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("invalid download section: {value}"),
        ));
    }
    if start == 0.0 && end.is_none() {
        return Ok(None);
    }
    Ok(Some(DownloadRange { start, end }))
}

fn parse_time(value: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0usize;
    for part in value.split(':') {
        count += 1;
        if count > 3 {
            return None;
        }
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let number = part.parse::<f64>().ok()?;
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        total = total * 60.0 + number;
    }
    (count > 0).then_some(total)
}

pub struct Downloader {
    client: reqwest::Client,
    config: DownloadConfig,
    multi: MultiProgress,
    ffmpeg: OnceCell<PathBuf>,
}

impl Downloader {
    pub fn new(config: DownloadConfig) -> Result<Self> {
        let client = crate::http::client_for_config(config.proxy.as_deref(), config.timeout)?;

        let multi = if config.quiet || !std::io::stderr().is_terminal() {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };

        Ok(Self {
            client,
            config,
            multi,
            ffmpeg: OnceCell::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_format(
        &self,
        format: &Format,
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
        range: Option<DownloadRange>,
        progress: Option<&Progress>,
        component: &str,
    ) -> Result<()> {
        if is_file(destination).await && !self.config.force_overwrites {
            return Ok(());
        }
        if range.is_some() || format.is_hls {
            self.download_via_ffmpeg(format, destination, label, duration_secs, range, progress)
                .await
        } else {
            self.download_http(format, destination, label, progress, component)
                .await
        }
    }

    async fn download_http(
        &self,
        format: &Format,
        destination: &Path,
        label: &str,
        progress: Option<&Progress>,
        component: &str,
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let part = format_part_path(destination, format, component);
        let total = format.content_length.filter(|total| *total > 0);
        let bar = self.progress_bar(total, label);
        let last_error;
        let mut attempt = 0u32;
        let mut cold_attempts = 0u32;
        let mut last_was_cold = false;

        loop {
            if attempt > 0 {
                let delay = retry_delay(attempt, cold_attempts, last_was_cold);
                warn!("retrying {label} in {delay:?} (attempt {})", attempt + 1);
                tokio::time::sleep(delay).await;
            }

            match self
                .download_http_once(format, &part, total, &bar, progress, component)
                .await
            {
                Ok(()) => {
                    replace_file(&part, destination).await?;
                    bar.finish_with_message(format!("{label} [done]"));
                    return Ok(());
                }
                Err(error) => {
                    let retryable = is_download_retryable(&error);
                    let cold_start = is_cold_start_forbidden(&error, format);
                    if cold_start {
                        cold_attempts = cold_attempts.saturating_add(1);
                    }
                    last_was_cold = cold_start;
                    if !retryable
                        || !should_retry_download(
                            self.config.retries,
                            attempt,
                            cold_start,
                            cold_attempts,
                        )
                    {
                        last_error = error;
                        break;
                    }
                    bar.set_message(format!("{label} [retrying]"));
                }
            }
            attempt = attempt.saturating_add(1);
        }

        bar.abandon_with_message(format!("{label} [failed]"));
        Err(last_error)
    }

    async fn download_http_once(
        &self,
        format: &Format,
        part: &Path,
        total: Option<u64>,
        bar: &ProgressBar,
        progress: Option<&Progress>,
        component: &str,
    ) -> Result<()> {
        let mut existing = tokio::fs::metadata(part)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if let Some(total) = total {
            if existing == total {
                bar.set_position(total);
                if let Some(progress) = progress {
                    progress.finish_component(component);
                }
                return Ok(());
            }
            if existing > total {
                tokio::fs::remove_file(part).await?;
                existing = 0;
            }
        }
        if let Some(progress) = progress {
            progress.set_done(component, existing);
        }

        let mut request = self
            .client
            .get(&format.url)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, format.user_agent)
            .header(reqwest::header::RANGE, format!("bytes={existing}-"));
        if let Some(cookies) = self.cookies_for(&format.url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        let response = request.send().await?;
        let status = response.status();
        debug!(
            "HTTP {status} for {} (n={:?})",
            crate::http::redact_url(&format.url),
            url::Url::parse(&format.url).ok().and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "n")
                    .map(|(_, value)| value.into_owned())
            })
        );

        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            if total.is_some_and(|total| existing >= total) {
                bar.set_position(existing);
                if let Some(progress) = progress {
                    progress.finish_component(component);
                }
                return Ok(());
            }
            remove_part(part, bar, progress, component, existing).await?;
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: "server rejected resume range; restarting download".to_string(),
            });
        }

        if !status.is_success() {
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: format!("unexpected HTTP status {status}"),
            });
        }

        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        let partial = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if partial {
            match content_range {
                Some((start, _)) if existing > 0 && start == existing => {}
                Some((0, _)) => {
                    remove_part(part, bar, progress, component, existing).await?;
                    existing = 0;
                }
                _ => {
                    remove_part(part, bar, progress, component, existing).await?;
                    return Err(YtrustError::coded(
                        ErrorCode::Network,
                        "server returned an invalid resume range; restarting download",
                    ));
                }
            }
        } else if existing > 0 {
            remove_part(part, bar, progress, component, existing).await?;
            existing = 0;
        }

        let append = partial && existing > 0;
        let expected = content_range
            .and_then(|(_, total)| total)
            .or_else(|| {
                response.content_length().map(|remaining| {
                    if append {
                        existing + remaining
                    } else {
                        remaining
                    }
                })
            })
            .or(total)
            .filter(|expected| *expected > 0);
        if let Some(expected) = expected {
            bar.set_length(expected);
            if let Some(progress) = progress {
                progress.set_total(component, expected);
            }
        }
        bar.set_position(if append { existing } else { 0 });

        let raw_file = if append {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .await?
        } else {
            tokio::fs::File::create(part).await?
        };
        let mut file = tokio::io::BufWriter::with_capacity(512 * 1024, raw_file);
        let mut written = 0u64;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            bar.inc(chunk.len() as u64);
            if let Some(progress) = progress {
                progress.add(component, chunk.len() as u64);
            }
        }
        file.flush().await?;
        let actual = existing + written;
        if let Some(expected) = expected
            && actual != expected
        {
            return Err(YtrustError::coded(
                ErrorCode::Network,
                format!("incomplete download: {actual} of {expected} bytes"),
            ));
        }
        bar.set_position(actual);
        if let Some(progress) = progress {
            progress.finish_component(component);
        }
        Ok(())
    }

    async fn download_via_ffmpeg(
        &self,
        format: &Format,
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
        range: Option<DownloadRange>,
        progress: Option<&Progress>,
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let ffmpeg = self.ffmpeg_path().await?;
        let duration = range
            .and_then(|range| range.duration(duration_secs))
            .or(duration_secs);
        let bar = self.progress_bar(duration, label);
        bar.set_style(duration_style());
        let extension = destination
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("tmp");
        let temp = temp_output_path(destination, "download", extension);

        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");
        apply_ffmpeg_http_options(&mut command, format.is_live);
        if let Some(range) = range
            && range.start > 0.0
        {
            command.arg("-ss").arg(format_seconds(range.start));
        }
        command
            .arg("-user_agent")
            .arg(format.user_agent)
            .arg("-headers")
            .arg(self.ffmpeg_headers(format))
            .arg("-i")
            .arg(&format.url);
        if let Some(range) = range
            && let Some(end) = range.end
        {
            command.arg("-t").arg(format_seconds(end - range.start));
        }
        command.arg("-c").arg("copy");

        if destination
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
            && format.is_hls
        {
            command.arg("-bsf:a").arg("aac_adtstoasc");
        }

        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(&temp);

        run_ffmpeg_to_path(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration,
            progress,
            "download",
            &temp,
            destination,
        )
        .await
    }

    pub async fn merge(
        &self,
        video: &Path,
        audios: &[(PathBuf, Option<String>)],
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
        progress: Option<&Progress>,
    ) -> Result<()> {
        if is_file(destination).await && !self.config.force_overwrites {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let ffmpeg = self.ffmpeg_path().await?;
        let bar = self.progress_bar(duration_secs, label);
        bar.set_style(duration_style());
        let extension = destination
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("mp4");
        let temp = temp_output_path(destination, "merge", extension);

        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(video);
        for (audio, _) in audios {
            command.arg("-i").arg(audio);
        }
        command.arg("-map").arg("0:v:0");
        for index in 0..audios.len() {
            command.arg("-map").arg(format!("{}:a:0", index + 1));
        }
        command.arg("-c").arg("copy");
        for (index, (_, language)) in audios.iter().enumerate() {
            if let Some(language) = language {
                command
                    .arg(format!("-metadata:s:a:{index}"))
                    .arg(format!("language={}", language_metadata(language)));
            }
        }

        if destination
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
        {
            command.arg("-movflags").arg("+faststart");
        }

        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(&temp);

        run_ffmpeg_to_path(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "merge",
            &temp,
            destination,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn convert_audio(
        &self,
        input: &Path,
        destination: &Path,
        audio_format: &str,
        label: &str,
        duration_secs: Option<u64>,
        audio_quality: Option<&str>,
        progress: Option<&Progress>,
    ) -> Result<()> {
        if is_file(destination).await && !self.config.force_overwrites {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let codec = audio_codec(audio_format)?;
        let ffmpeg = self.ffmpeg_path().await?;
        let bar = self.progress_bar(duration_secs, label);
        bar.set_style(duration_style());
        let extension = destination
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(audio_format);
        let temp = temp_output_path(destination, "audio", extension);

        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-vn")
            .arg("-c:a")
            .arg(codec);
        if let Some(quality) = audio_quality.filter(|quality| !quality.trim().is_empty()) {
            apply_audio_quality(&mut command, codec, quality.trim())?;
        }
        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(&temp);

        run_ffmpeg_to_path(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "audio",
            &temp,
            destination,
        )
        .await
    }

    pub async fn embed_subtitles(
        &self,
        media: &Path,
        extras: &[ExtraFile],
        duration_secs: Option<u64>,
        progress: Option<&Progress>,
    ) -> Result<PathBuf> {
        let subtitles: Vec<&ExtraFile> = extras
            .iter()
            .filter(|extra| extra.kind == "subtitle")
            .collect();
        if subtitles.is_empty() {
            return Ok(media.to_path_buf());
        }
        let extension = media
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let subtitle_codec = match extension.as_str() {
            "mp4" | "m4v" | "mov" => "mov_text",
            "mkv" => "srt",
            _ => {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    "--embed-subs requires an mp4/mov or mkv output container",
                ));
            }
        };
        let temp = temp_output_path(media, "embed", &extension);
        let ffmpeg = self.ffmpeg_path().await?;
        let bar = self.progress_bar(duration_secs, "subtitles");
        bar.set_style(duration_style());
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(media);
        for subtitle in &subtitles {
            command.arg("-i").arg(&subtitle.path);
        }
        command.arg("-map").arg("0");
        for index in 0..subtitles.len() {
            command.arg("-map").arg(format!("{}:0", index + 1));
        }
        command
            .arg("-c")
            .arg("copy")
            .arg("-c:s")
            .arg(subtitle_codec);
        for (index, subtitle) in subtitles.iter().enumerate() {
            if let Some(language) = &subtitle.language {
                command
                    .arg(format!("-metadata:s:s:{index}"))
                    .arg(format!("language={}", language_metadata(language)));
            }
        }
        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(&temp);
        let result = run_ffmpeg(
            command,
            bar,
            "subtitles",
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "subtitles",
        )
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error);
        }
        replace_file(&temp, media).await?;
        Ok(media.to_path_buf())
    }

    pub async fn remux(
        &self,
        media: &Path,
        container: &str,
        duration_secs: Option<u64>,
        progress: Option<&Progress>,
    ) -> Result<PathBuf> {
        let container = container
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if !matches!(container.as_str(), "mp4" | "mkv" | "webm") {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("--remux-video supports mp4, mkv or webm, got {container}"),
            ));
        }
        let current = media
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if current == container {
            return Ok(media.to_path_buf());
        }
        let destination = media.with_extension(&container);
        if is_file(&destination).await && !self.config.force_overwrites {
            return Ok(destination);
        }
        let temp = temp_output_path(&destination, "remux", &container);
        let ffmpeg = self.ffmpeg_path().await?;
        let bar = self.progress_bar(duration_secs, "remux");
        bar.set_style(duration_style());
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-i")
            .arg(media)
            .arg("-map")
            .arg("0")
            .arg("-c")
            .arg("copy");
        if container == "mp4" {
            command.arg("-movflags").arg("+faststart");
        }
        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(&temp);
        let result = run_ffmpeg(
            command,
            bar,
            "remux",
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "remux",
        )
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error);
        }
        replace_file(&temp, &destination).await?;
        let _ = tokio::fs::remove_file(media).await;
        Ok(destination)
    }

    pub async fn download_url(&self, url: &str, destination: &Path, label: &str) -> Result<()> {
        if is_file(destination).await && !self.config.force_overwrites {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let bar = self.progress_bar(None, label);
        let part = part_path(destination);
        let mut last_error = None;

        for attempt in 0..=self.config.retries {
            if attempt > 0 {
                let delay = download_retry_delay(attempt);
                warn!(
                    "retrying {label} in {delay:?} ({attempt}/{})",
                    self.config.retries
                );
                tokio::time::sleep(delay).await;
            }
            match self.download_url_once(url, &part, destination, &bar).await {
                Ok(()) => {
                    bar.finish_with_message(format!("{label} [done]"));
                    return Ok(());
                }
                Err(error) => {
                    let retryable = is_download_retryable(&error);
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                    bar.set_message(format!("{label} [retrying]"));
                }
            }
        }

        bar.abandon_with_message(format!("{label} [failed]"));
        let _ = tokio::fs::remove_file(&part).await;
        Err(last_error.unwrap_or_else(|| {
            YtrustError::coded(ErrorCode::Network, format!("failed to download {label}"))
        }))
    }

    async fn download_url_once(
        &self,
        url: &str,
        part: &Path,
        destination: &Path,
        bar: &ProgressBar,
    ) -> Result<()> {
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::ACCEPT, "*/*");
        if let Some(cookies) = self.cookies_for(url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: format!("HTTP {status} for {}", crate::http::redact_url(url)),
            });
        }
        let expected = response.content_length().filter(|length| *length > 0);
        if let Some(expected) = expected {
            bar.set_length(expected);
        }
        let raw_file = tokio::fs::File::create(part).await?;
        let mut file = tokio::io::BufWriter::with_capacity(256 * 1024, raw_file);
        let mut written = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            bar.inc(chunk.len() as u64);
        }
        file.flush().await?;
        if let Some(expected) = expected
            && written != expected
        {
            return Err(YtrustError::coded(
                ErrorCode::Network,
                format!("incomplete download: {written} of {expected} bytes"),
            ));
        }
        replace_file(part, destination).await
    }

    pub async fn stream_to_stdout(
        &self,
        format: &Format,
        range: Option<DownloadRange>,
        label: &str,
    ) -> Result<()> {
        if range.is_some() || format.is_hls {
            return self.stream_via_ffmpeg(format, range, label).await;
        }

        let stream_n = url::Url::parse(&format.url).ok().and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "n")
                .map(|(_, value)| value.into_owned())
        });
        debug!(
            "streaming format {} ({} bytes URL, n={:?}, cipher={})",
            format.itag,
            format.url.len(),
            stream_n,
            format.signature_cipher.is_some() || format.cipher.is_some()
        );

        let bar = self.progress_bar(None, label);
        let last_error;
        let mut attempt = 0u32;
        let mut cold_attempts = 0u32;
        let mut last_was_cold = false;
        loop {
            if attempt > 0 {
                let delay = retry_delay(attempt, cold_attempts, last_was_cold);
                warn!("retrying {label} in {delay:?} (attempt {})", attempt + 1);
                tokio::time::sleep(delay).await;
            }
            let mut written = 0u64;
            match self.stream_to_stdout_once(format, &bar, &mut written).await {
                Ok(()) => {
                    bar.finish_and_clear();
                    return Ok(());
                }
                Err(error) => {
                    let retryable = written == 0 && is_download_retryable(&error);
                    let cold_start = is_cold_start_forbidden(&error, format);
                    if cold_start {
                        cold_attempts = cold_attempts.saturating_add(1);
                    }
                    last_was_cold = cold_start;
                    if !retryable
                        || !should_retry_download(
                            self.config.retries,
                            attempt,
                            cold_start,
                            cold_attempts,
                        )
                    {
                        last_error = error;
                        break;
                    }
                }
            }
            attempt = attempt.saturating_add(1);
        }
        bar.abandon_with_message(format!("{label} [failed]"));
        Err(last_error)
    }

    async fn stream_to_stdout_once(
        &self,
        format: &Format,
        bar: &ProgressBar,
        written: &mut u64,
    ) -> Result<()> {
        let mut request = self
            .client
            .get(&format.url)
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, format.user_agent)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::RANGE, "bytes=0-");
        if let Some(cookies) = self.cookies_for(&format.url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: format!(
                    "unexpected HTTP status {status} for {}",
                    crate::http::redact_url(&format.url)
                ),
            });
        }

        let mut stdout = tokio::io::stdout();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            stdout.write_all(&chunk).await?;
            *written += chunk.len() as u64;
            bar.inc(chunk.len() as u64);
        }
        stdout.flush().await?;
        Ok(())
    }

    async fn stream_via_ffmpeg(
        &self,
        format: &Format,
        range: Option<DownloadRange>,
        label: &str,
    ) -> Result<()> {
        let ffmpeg = self.ffmpeg_path().await?;
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");
        apply_ffmpeg_http_options(&mut command, format.is_live);
        if let Some(range) = range
            && range.start > 0.0
        {
            command.arg("-ss").arg(format_seconds(range.start));
        }
        command
            .arg("-user_agent")
            .arg(format.user_agent)
            .arg("-headers")
            .arg(self.ffmpeg_headers(format))
            .arg("-i")
            .arg(&format.url);
        if let Some(range) = range
            && let Some(end) = range.end
        {
            command.arg("-t").arg(format_seconds(end - range.start));
        }
        command
            .arg("-map")
            .arg("0:v:0?")
            .arg("-map")
            .arg("0:a:0?")
            .arg("-c")
            .arg("copy")
            .arg("-f")
            .arg("matroska")
            .arg("pipe:1");

        if let Some(proxy) = &self.config.proxy {
            command
                .env("http_proxy", proxy)
                .env("https_proxy", proxy)
                .env("HTTP_PROXY", proxy)
                .env("HTTPS_PROXY", proxy);
        }

        let mut child = command
            .kill_on_drop(true)
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| YtrustError::coded(ErrorCode::Ffmpeg, "ffmpeg stderr unavailable"))?;
        let stderr_task = tokio::spawn(async move {
            let mut text = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_bounded(&mut text, &line, MAX_FFMPEG_STDERR_BYTES);
            }
            text
        });
        let status = child.wait().await?;
        let stderr = stderr_task.await.unwrap_or_default();
        if !status.success() {
            if is_broken_pipe_status(&status, &stderr) {
                return Err(YtrustError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "ffmpeg output pipe was closed",
                )));
            }
            return Err(YtrustError::Ffmpeg {
                status: status.to_string(),
                stderr: redact_ffmpeg_stderr(stderr.trim()),
            });
        }
        tracing::debug!("stream finished: {label}");
        Ok(())
    }

    pub async fn fetch_text(&self, url: &str) -> Result<String> {
        let config = crate::http::RequestConfig {
            proxy: self.config.proxy.clone(),
            cookies: self.config.cookies.clone(),
            cookie_jar: self.config.cookie_jar.clone(),
            timeout: self.config.timeout,
            ..crate::http::RequestConfig::default()
        };
        crate::http::fetch_text_limited(
            &self.client,
            url,
            &config,
            WEB_UA,
            Some("https://www.youtube.com/"),
            crate::http::MAX_HTML_BYTES,
        )
        .await
    }

    pub async fn ffmpeg_available(&self) -> bool {
        self.ffmpeg_path().await.is_ok()
    }

    async fn ffmpeg_path(&self) -> Result<PathBuf> {
        if let Some(path) = self.ffmpeg.get() {
            return Ok(path.clone());
        }
        let path = match &self.config.ffmpeg {
            Some(location) if location.is_dir() => location.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            }),
            Some(location) => location.clone(),
            None => PathBuf::from("ffmpeg"),
        };

        let probe = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&path)
                .arg("-version")
                .kill_on_drop(true)
                .output(),
        )
        .await;
        match probe {
            Ok(Ok(output)) if output.status.success() => {
                let _ = self.ffmpeg.set(path.clone());
                Ok(path)
            }
            _ => Err(YtrustError::FfmpegMissing),
        }
    }

    fn ffmpeg_headers(&self, format: &Format) -> String {
        let mut headers = String::from("Referer: https://www.youtube.com/\r\n");
        if let Some(cookies) = self.cookies_for(&format.url) {
            headers.push_str("Cookie: ");
            headers.push_str(&cookies);
            headers.push_str("\r\n");
        }
        headers
    }

    fn cookies_for(&self, url: &str) -> Option<String> {
        if let Some(jar) = &self.config.cookie_jar {
            return jar.header_for_url(url);
        }
        self.config
            .cookies
            .as_deref()
            .filter(|_| is_youtube_host(url))
            .map(str::to_string)
    }

    fn progress_bar(&self, total: Option<u64>, label: &str) -> ProgressBar {
        let bar = if let Some(total) = total.filter(|total| *total > 0) {
            ProgressBar::new(total)
        } else {
            ProgressBar::new_spinner()
        };
        bar.set_style(bytes_style());
        bar.set_message(label.to_string());
        self.multi.add(bar)
    }
}

pub fn audio_codec(audio_format: &str) -> Result<&'static str> {
    match audio_format.to_ascii_lowercase().as_str() {
        "mp3" => Ok("libmp3lame"),
        "m4a" | "aac" => Ok("aac"),
        "opus" => Ok("libopus"),
        "ogg" | "vorbis" => Ok("libvorbis"),
        "flac" => Ok("flac"),
        "wav" => Ok("pcm_s16le"),
        other => Err(YtrustError::coded(
            ErrorCode::InvalidInput,
            format!("unsupported audio format: {other}"),
        )),
    }
}

fn format_seconds(value: f64) -> String {
    format!("{value:.3}")
}

fn apply_ffmpeg_http_options(command: &mut Command, is_live: bool) {
    command
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-reconnect_on_http_error")
        .arg("403,408,410,425,429,500,502,503,504");
    if is_live {
        command.arg("-reconnect_at_eof").arg("1");
    }
}

fn download_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.min(3))
}

fn cold_start_retry_delay(cold_attempts: u32) -> Duration {
    Duration::from_millis(COLD_START_BASE_DELAY_MS << cold_attempts.saturating_sub(1).min(3))
}

fn retry_delay(attempt: u32, cold_attempts: u32, last_was_cold: bool) -> Duration {
    if last_was_cold {
        cold_start_retry_delay(cold_attempts)
    } else {
        download_retry_delay(attempt)
    }
}

fn should_retry_download(
    user_retries: u32,
    attempt: u32,
    cold_start: bool,
    cold_attempts: u32,
) -> bool {
    attempt < user_retries || (cold_start && cold_attempts < COLD_START_MIN_ATTEMPTS)
}

fn is_cold_start_forbidden(error: &YtrustError, format: &Format) -> bool {
    matches!(error, YtrustError::HttpStatus { status: 403, .. }) && is_googlevideo_url(&format.url)
}

fn is_googlevideo_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    host == "googlevideo.com" || host.ends_with(".googlevideo.com")
}

fn is_download_retryable(error: &YtrustError) -> bool {
    error.is_retryable()
        || matches!(
            error,
            YtrustError::HttpStatus {
                status: 403 | 408 | 410 | 425,
                ..
            }
        )
}

async fn replace_file(part: &Path, destination: &Path) -> Result<()> {
    match tokio::fs::rename(part, destination).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if !is_file(destination).await {
                return Err(error.into());
            }
            // Windows refuses to rename over an existing file. Move the old
            // file aside first so a failed second rename does not destroy it.
            let backup = temp_output_path(destination, "backup", "tmp");
            match tokio::fs::rename(destination, &backup).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            match tokio::fs::rename(part, destination).await {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(&backup).await;
                    Ok(())
                }
                Err(rename_error) => {
                    let _ = tokio::fs::rename(&backup, destination).await;
                    Err(rename_error.into())
                }
            }
        }
    }
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn part_path(destination: &Path) -> PathBuf {
    let mut path = destination.as_os_str().to_os_string();
    path.push(format!(".{}.part", std::process::id()));
    PathBuf::from(path)
}

fn temp_output_path(destination: &Path, kind: &str, extension: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let pid = std::process::id();
    destination.with_file_name(format!(".{name}.{kind}.{pid}.{extension}"))
}

fn format_part_path(destination: &Path, format: &Format, component: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let length = format.content_length.unwrap_or(0);
    let component = sanitize_identity(component);
    destination.with_file_name(format!(
        ".{name}.{itag}.{length}.{component}.part",
        itag = format.itag
    ))
}

fn sanitize_identity(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if sanitized.is_empty() {
        "format".to_string()
    } else {
        sanitized
    }
}

fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse::<u64>().ok()?)
    };
    Some((start, total))
}

async fn remove_part(
    part: &Path,
    bar: &ProgressBar,
    progress: Option<&Progress>,
    component: &str,
    existing: u64,
) -> Result<()> {
    if existing > 0 {
        match tokio::fs::remove_file(part).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    bar.set_position(0);
    if let Some(progress) = progress {
        progress.set_done(component, 0);
    }
    Ok(())
}

fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg} {wide_bar:.cyan/blue} {bytes}/{total_bytes} {bytes_per_sec} {eta}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("━╸─")
}

fn duration_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg} {wide_bar:.cyan/blue} {pos}/{len} [{elapsed_precise}]",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("━╸─")
}

#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg_to_path(
    command: Command,
    bar: ProgressBar,
    label: &str,
    proxy: Option<&str>,
    duration_secs: Option<u64>,
    progress: Option<&Progress>,
    processor: &str,
    temp: &Path,
    destination: &Path,
) -> Result<()> {
    let result = run_ffmpeg(
        command,
        bar,
        label,
        proxy,
        duration_secs,
        progress,
        processor,
    )
    .await;
    match result {
        Ok(()) => match replace_file(temp, destination).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_file(temp).await;
                Err(error)
            }
        },
        Err(error) => {
            let _ = tokio::fs::remove_file(temp).await;
            Err(error)
        }
    }
}

async fn run_ffmpeg(
    mut command: Command,
    bar: ProgressBar,
    label: &str,
    proxy: Option<&str>,
    duration_secs: Option<u64>,
    progress: Option<&Progress>,
    processor: &str,
) -> Result<()> {
    if let Some(proxy) = proxy {
        command
            .env("http_proxy", proxy)
            .env("https_proxy", proxy)
            .env("HTTP_PROXY", proxy)
            .env("HTTPS_PROXY", proxy);
    }

    let mut child = command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| YtrustError::coded(ErrorCode::Ffmpeg, "ffmpeg stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| YtrustError::coded(ErrorCode::Ffmpeg, "ffmpeg stderr unavailable"))?;

    if let Some(progress) = progress {
        progress.phase_started(processor);
    }
    let progress_handle = progress.cloned();
    let progress_bar = bar.clone();
    let progress_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut last_us = 0u64;
        while let Ok(Some(line)) = lines.next_line().await {
            let timestamp = line
                .strip_prefix("out_time_us=")
                .and_then(|value| value.parse::<u64>().ok())
                .or_else(|| {
                    line.strip_prefix("out_time_ms=")
                        .and_then(|value| value.parse::<u64>().ok())
                });
            if let Some(us) = timestamp
                && us >= last_us
            {
                last_us = us;
                let seconds = us / 1_000_000;
                progress_bar.set_position(seconds);
                if let (Some(progress), Some(duration)) = (&progress_handle, duration_secs)
                    && duration > 0
                {
                    progress.phase_progress(seconds as f64 / duration as f64 * 100.0);
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            push_bounded(&mut text, &line, MAX_FFMPEG_STDERR_BYTES);
        }
        text
    });

    let status = child.wait().await?;
    let _ = progress_task.await;
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        if let Some(progress) = progress {
            progress.phase_failed(processor);
        }
        bar.abandon_with_message(format!("{label} [failed]"));
        return Err(YtrustError::Ffmpeg {
            status: status.to_string(),
            stderr: redact_ffmpeg_stderr(stderr.trim()),
        });
    }

    if let Some(progress) = progress {
        progress.phase_finished(processor);
    }
    bar.finish_with_message(format!("{label} [done]"));
    Ok(())
}

fn redact_ffmpeg_stderr(stderr: &str) -> String {
    let mut redacted = String::new();
    for line in stderr.lines() {
        let lower = line.to_ascii_lowercase();
        let line = if lower.contains("cookie:") || lower.contains("authorization:") {
            let prefix_len = lower
                .find("cookie:")
                .or_else(|| lower.find("authorization:"))
                .unwrap_or(0);
            let (prefix, rest) = line.split_at(prefix_len);
            let key_end = rest.find(':').map(|index| index + 1).unwrap_or(rest.len());
            format!("{} {} <redacted>", prefix.trim_end(), &rest[..key_end])
        } else {
            line.split_whitespace()
                .map(|word| {
                    let Some(start) = word.find("http://").or_else(|| word.find("https://")) else {
                        return word.to_string();
                    };
                    let prefix = &word[..start];
                    let candidate = &word[start..];
                    let end = candidate
                        .find(|character: char| {
                            !character.is_ascii_graphic()
                                || matches!(character, '"' | '\'' | ')' | ']' | ',' | ';')
                        })
                        .unwrap_or(candidate.len());
                    format!(
                        "{prefix}{}{}",
                        crate::http::redact_url(&candidate[..end]),
                        &candidate[end..]
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        push_bounded(&mut redacted, &line, MAX_FFMPEG_STDERR_BYTES);
    }
    redacted
}

fn push_bounded(text: &mut String, line: &str, max: usize) {
    if text.len() >= max {
        return;
    }
    let remaining = max - text.len();
    if line.len() < remaining {
        text.push_str(line);
        text.push('\n');
    } else {
        let mut end = remaining.saturating_sub(1).min(line.len());
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&line[..end]);
        text.push_str("…\n");
    }
}

fn is_broken_pipe_status(status: &std::process::ExitStatus, stderr: &str) -> bool {
    if stderr.to_ascii_lowercase().contains("broken pipe") {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(13) {
            return true;
        }
    }
    let _ = status;
    false
}

fn language_metadata(language: &str) -> String {
    let language = language.trim().split('-').next().unwrap_or(language);
    match language.to_ascii_lowercase().as_str() {
        "en" => "eng".to_string(),
        "de" => "ger".to_string(),
        "fr" => "fre".to_string(),
        "es" => "spa".to_string(),
        "it" => "ita".to_string(),
        "pt" => "por".to_string(),
        "ru" => "rus".to_string(),
        "uk" => "ukr".to_string(),
        "pl" => "pol".to_string(),
        "tr" => "tur".to_string(),
        "ja" => "jpn".to_string(),
        "ko" => "kor".to_string(),
        "zh" => "chi".to_string(),
        "ar" => "ara".to_string(),
        "hi" => "hin".to_string(),
        "nl" => "dut".to_string(),
        "sv" => "swe".to_string(),
        "da" => "dan".to_string(),
        "no" => "nor".to_string(),
        "fi" => "fin".to_string(),
        "cs" => "cze".to_string(),
        "el" => "gre".to_string(),
        "he" => "heb".to_string(),
        "id" => "ind".to_string(),
        "vi" => "vie".to_string(),
        "th" => "tha".to_string(),
        _ => language.to_string(),
    }
}

fn apply_audio_quality(command: &mut Command, codec: &str, quality: &str) -> Result<()> {
    if let Some(bitrate) = quality
        .strip_suffix('k')
        .or_else(|| quality.strip_suffix('K'))
        .or_else(|| quality.strip_suffix('m'))
        .or_else(|| quality.strip_suffix('M'))
    {
        if bitrate.is_empty() || bitrate.parse::<u32>().is_err() {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                format!("invalid audio bitrate: {quality}"),
            ));
        }
        let suffix = &quality[quality.len() - 1..];
        command.arg("-b:a").arg(format!("{bitrate}{suffix}"));
        return Ok(());
    }
    if let Ok(value) = quality.parse::<u8>() {
        if value > 10 {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "--audio-quality VBR value must be between 0 and 10",
            ));
        }
        if matches!(codec, "libmp3lame" | "libvorbis") {
            command.arg("-q:a").arg(quality);
            return Ok(());
        }
        let bitrate = match value {
            0 => 320,
            1 => 256,
            2 => 224,
            3 => 192,
            4 => 160,
            5 => 128,
            6 => 112,
            7 => 96,
            8 => 80,
            9 => 64,
            _ => 48,
        };
        command.arg("-b:a").arg(format!("{bitrate}k"));
        return Ok(());
    }
    Err(YtrustError::coded(
        ErrorCode::InvalidInput,
        format!("invalid audio quality: {quality}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_ranges() {
        let range = parse_range("*120-inf").unwrap().unwrap();
        assert_eq!(range.start, 120.0);
        assert_eq!(range.end, None);

        let range = parse_range("*1:00-2:30").unwrap().unwrap();
        assert_eq!(range.start, 60.0);
        assert_eq!(range.end, Some(150.0));

        let range = parse_range("10-20").unwrap().unwrap();
        assert_eq!(range.start, 10.0);
        assert_eq!(range.end, Some(20.0));

        assert!(parse_range("*0-inf").unwrap().is_none());
        assert!(parse_range("").unwrap().is_none());
        assert!(parse_range("1-2,3-4").is_err());
        assert!(parse_range("10-5").is_err());
    }

    #[test]
    fn computes_range_duration() {
        let range = DownloadRange {
            start: 10.0,
            end: Some(20.0),
        };
        assert_eq!(range.duration(Some(100)), Some(10));
        let range = DownloadRange {
            start: 90.0,
            end: None,
        };
        assert_eq!(range.duration(Some(100)), Some(10));
    }

    #[test]
    fn maps_audio_formats() {
        assert_eq!(audio_codec("mp3").unwrap(), "libmp3lame");
        assert_eq!(audio_codec("M4A").unwrap(), "aac");
        assert!(audio_codec("aiff").is_err());
    }

    #[test]
    fn maps_language_metadata() {
        assert_eq!(language_metadata("en-US"), "eng");
        assert_eq!(language_metadata("de"), "ger");
        assert_eq!(language_metadata("xx"), "xx");
    }

    #[test]
    fn redacts_urls_in_ffmpeg_errors() {
        let redacted = redact_ffmpeg_stderr(
            "Error opening input file https://rr1.googlevideo.com/videoplayback?sig=secret&expire=1",
        );
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("expire=1"));
    }

    #[test]
    fn parses_content_ranges() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, Some(200)))
        );
        assert_eq!(parse_content_range("bytes 0-9/*"), Some((0, None)));
        assert_eq!(parse_content_range("items 1-2/3"), None);
        assert_eq!(parse_content_range("bytes nope-1/2"), None);
    }

    #[test]
    fn part_paths_include_format_and_component_identity() {
        let format = Format {
            itag: 251,
            url: "https://example.com".to_string(),
            signature_cipher: None,
            cipher: None,
            container: "webm".to_string(),
            video_codec: None,
            audio_codec: Some("opus".to_string()),
            has_video: false,
            has_audio: true,
            width: None,
            height: None,
            fps: None,
            bitrate: 128_000,
            audio_bitrate: Some(128_000),
            content_length: Some(1_000),
            quality_label: None,
            is_hls: false,
            is_live: false,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: false,
            language: None,
            user_agent: "test",
        };
        let first = format_part_path(Path::new("video.mkv"), &format, "audio");
        let second = format_part_path(Path::new("video.mkv"), &format, "audio2");
        assert_ne!(first, second);
        assert!(first.to_string_lossy().contains("251"));
    }

    #[test]
    fn redacts_cookie_headers_in_ffmpeg_errors() {
        let redacted = redact_ffmpeg_stderr("Cookie: SID=secret; SAPISID=hidden");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("hidden"));
        assert!(redacted.contains("Cookie:"));
    }

    #[test]
    fn detects_cold_start_forbidden_responses() {
        let format = Format {
            itag: 18,
            url: "https://rr1---sn-test.googlevideo.com/videoplayback?itag=18".to_string(),
            signature_cipher: None,
            cipher: None,
            container: "mp4".to_string(),
            video_codec: Some("avc1".to_string()),
            audio_codec: Some("mp4a".to_string()),
            has_video: true,
            has_audio: true,
            width: Some(640),
            height: Some(360),
            fps: Some(30),
            bitrate: 500_000,
            audio_bitrate: None,
            content_length: Some(1_000),
            quality_label: None,
            is_hls: false,
            is_live: false,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: false,
            language: None,
            user_agent: "test",
        };
        let forbidden = YtrustError::HttpStatus {
            status: 403,
            message: "cold edge".to_string(),
        };
        assert!(is_cold_start_forbidden(&forbidden, &format));
        assert!(is_googlevideo_url(&format.url));
        assert!(!is_googlevideo_url("https://example.com/videoplayback"));

        let mut other_host = format.clone();
        other_host.url = "https://example.com/video".to_string();
        assert!(!is_cold_start_forbidden(&forbidden, &other_host));

        let not_forbidden = YtrustError::HttpStatus {
            status: 404,
            message: "missing".to_string(),
        };
        assert!(!is_cold_start_forbidden(&not_forbidden, &format));
    }

    #[test]
    fn retries_cold_start_forbidden_without_user_retries() {
        assert!(should_retry_download(0, 0, true, 1));
        assert!(should_retry_download(0, 1, true, 2));
        assert!(should_retry_download(0, 2, true, 3));
        assert!(should_retry_download(0, 3, true, 4));
        assert!(!should_retry_download(0, 4, true, 5));
        assert!(!should_retry_download(0, 0, false, 0));
        assert!(should_retry_download(2, 0, false, 0));
        assert!(should_retry_download(2, 1, false, 0));
        assert!(!should_retry_download(2, 2, false, 0));
    }

    #[test]
    fn uses_short_cold_start_backoff() {
        assert_eq!(
            retry_delay(1, 1, true),
            Duration::from_millis(COLD_START_BASE_DELAY_MS)
        );
        assert_eq!(retry_delay(2, 2, true), Duration::from_millis(1_000));
        assert_eq!(retry_delay(3, 3, true), Duration::from_millis(2_000));
        assert_eq!(retry_delay(4, 4, true), Duration::from_millis(4_000));
        assert_eq!(retry_delay(1, 0, false), download_retry_delay(1));
    }

    #[tokio::test]
    async fn failed_ffmpeg_postprocess_leaves_no_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("final.mp4");
        let temp = temp_output_path(&destination, "test", "mp4");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 7");
        let result = run_ffmpeg_to_path(
            command,
            ProgressBar::hidden(),
            "test",
            None,
            None,
            None,
            "test",
            &temp,
            &destination,
        )
        .await;
        assert!(result.is_err());
        assert!(!destination.exists());
        assert!(!temp.exists());
    }
}
