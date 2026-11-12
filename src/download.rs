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

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub retries: u32,
    pub timeout: u64,
    pub proxy: Option<String>,
    pub cookies: Option<String>,
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
        let timeout = Duration::from_secs(config.timeout.max(5));
        let mut builder = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .user_agent(WEB_UA);

        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| {
                YtrustError::coded(ErrorCode::InvalidInput, format!("invalid proxy: {error}"))
            })?);
        }

        let multi = if config.quiet {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };

        Ok(Self {
            client: builder.build()?,
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

        let part = format_part_path(destination, format);
        let total = format.content_length;
        let bar = self.progress_bar(total, label);
        let mut last_error = None;

        for attempt in 0..=self.config.retries {
            if attempt > 0 {
                let delay = Duration::from_secs(1u64 << attempt.min(5));
                warn!(
                    "retrying {label} in {delay:?} ({attempt}/{})",
                    self.config.retries
                );
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
                    let retryable = error.is_retryable();
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                    bar.set_message(format!("{label} [retrying]"));
                }
            }
        }

        bar.abandon_with_message(format!("{label} [failed]"));
        Err(last_error.unwrap_or_else(|| {
            YtrustError::coded(ErrorCode::Network, format!("failed to download {label}"))
        }))
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
            .header(reqwest::header::RANGE, format!("bytes={existing}-"));
        request = request
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, &format.user_agent);
        if let Some(cookies) = self.cookies_for(&format.url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }

        let response = request.send().await?;
        let status = response.status();
        debug!("HTTP {status} for {}", crate::http::redact_url(&format.url));

        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            if total.is_some_and(|total| existing >= total) {
                return Ok(());
            }
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: "server rejected resume range".to_string(),
            });
        }

        if !status.is_success() {
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: format!("unexpected HTTP status {status}"),
            });
        }

        let append = status == reqwest::StatusCode::PARTIAL_CONTENT && existing > 0;
        let expected = match (status, response.content_length(), total) {
            (reqwest::StatusCode::PARTIAL_CONTENT, Some(remaining), _) if existing > 0 => {
                Some(existing + remaining)
            }
            (_, Some(length), _) => Some(length),
            (_, None, total) => total,
        };
        if let Some(expected) = expected {
            bar.set_length(expected);
            if let Some(progress) = progress {
                progress.set_total(component, expected);
            }
        }
        bar.set_position(if append { existing } else { 0 });

        let mut file = if append {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .await?
        } else {
            tokio::fs::File::create(part).await?
        };

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            bar.inc(chunk.len() as u64);
            if let Some(progress) = progress {
                progress.add(component, chunk.len() as u64);
            }
        }
        file.flush().await?;
        let actual = file.metadata().await?.len();
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

        let duration = range
            .and_then(|range| range.duration(duration_secs))
            .or(duration_secs);
        let bar = self.progress_bar(duration, label);
        bar.set_style(duration_style());

        let ffmpeg = self.ffmpeg_path().await?;
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y");
        if let Some(range) = range
            && range.start > 0.0
        {
            command.arg("-ss").arg(format_seconds(range.start));
        }
        command
            .arg("-user_agent")
            .arg(&format.user_agent)
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
            .arg(destination);

        run_ffmpeg(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration,
            progress,
            "download",
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
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let bar = self.progress_bar(duration_secs, label);
        bar.set_style(duration_style());

        let ffmpeg = self.ffmpeg_path().await?;
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
            .arg(destination);

        run_ffmpeg(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "merge",
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
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let codec = audio_codec(audio_format)?;
        let bar = self.progress_bar(duration_secs, label);
        bar.set_style(duration_style());
        let ffmpeg = self.ffmpeg_path().await?;
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
            apply_audio_quality(&mut command, codec, quality.trim());
        }
        command
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(destination);

        run_ffmpeg(
            command,
            bar,
            label,
            self.config.proxy.as_deref(),
            duration_secs,
            progress,
            "audio",
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
        if destination.exists() && !self.config.force_overwrites {
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
                let delay = Duration::from_secs(1u64 << attempt.min(5));
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
                    let retryable = error.is_retryable();
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                    bar.set_message(format!("{label} [retrying]"));
                }
            }
        }

        bar.abandon_with_message(format!("{label} [failed]"));
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
        let expected = response.content_length();
        if let Some(expected) = expected {
            bar.set_length(expected);
        }
        let mut file = tokio::fs::File::create(part).await?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            bar.inc(chunk.len() as u64);
        }
        file.flush().await?;
        let actual = file.metadata().await?.len();
        if let Some(expected) = expected
            && actual != expected
        {
            return Err(YtrustError::coded(
                ErrorCode::Network,
                format!("incomplete download: {actual} of {expected} bytes"),
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

        let mut request = self
            .client
            .get(&format.url)
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, &format.user_agent)
            .header(reqwest::header::ACCEPT, "*/*");
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

        let bar = self.progress_bar(None, label);
        let mut stdout = tokio::io::stdout();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            stdout.write_all(&chunk).await?;
            bar.inc(chunk.len() as u64);
        }
        stdout.flush().await?;
        bar.finish_and_clear();
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
        if let Some(range) = range
            && range.start > 0.0
        {
            command.arg("-ss").arg(format_seconds(range.start));
        }
        command
            .arg("-user_agent")
            .arg(&format.user_agent)
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
                text.push_str(&line);
                text.push('\n');
            }
            text
        });
        let status = child.wait().await?;
        let stderr = stderr_task.await.unwrap_or_default();
        if !status.success() {
            return Err(YtrustError::Ffmpeg {
                status: status.to_string(),
                stderr: redact_ffmpeg_stderr(stderr.trim()),
            });
        }
        tracing::debug!("stream finished: {label}");
        Ok(())
    }

    pub async fn fetch_text(&self, url: &str) -> Result<String> {
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, WEB_UA)
            .header(reqwest::header::REFERER, "https://www.youtube.com/");
        if let Some(cookies) = self.cookies_for(url) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(YtrustError::HttpStatus {
                status: status.as_u16(),
                message: format!(
                    "HTTP {status} for {}: {}",
                    crate::http::redact_url(url),
                    text.chars().take(200).collect::<String>()
                ),
            });
        }
        Ok(text)
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

        match Command::new(&path).arg("-version").output().await {
            Ok(output) if output.status.success() => {
                let _ = self.ffmpeg.set(path.clone());
                Ok(path)
            }
            _ => Err(YtrustError::FfmpegMissing),
        }
    }

    fn ffmpeg_headers(&self, format: &Format) -> String {
        let mut headers = String::from("Referer: https://www.youtube.com/\r\n");
        if is_youtube_host(&format.url)
            && let Some(cookies) = &self.config.cookies
        {
            headers.push_str("Cookie: ");
            headers.push_str(cookies);
            headers.push_str("\r\n");
        }
        headers
    }

    fn cookies_for(&self, url: &str) -> Option<&str> {
        self.config
            .cookies
            .as_deref()
            .filter(|_| is_youtube_host(url))
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

async fn replace_file(part: &Path, destination: &Path) -> Result<()> {
    match tokio::fs::rename(part, destination).await {
        Ok(()) => Ok(()),
        Err(_) if destination.exists() => {
            // Windows refuses to rename over an existing file.
            match tokio::fs::remove_file(destination).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tokio::fs::rename(part, destination).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn part_path(destination: &Path) -> PathBuf {
    let mut path = destination.as_os_str().to_os_string();
    path.push(".part");
    PathBuf::from(path)
}

fn temp_output_path(destination: &Path, kind: &str, extension: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    destination.with_file_name(format!(".{name}.{kind}.{extension}"))
}

fn format_part_path(destination: &Path, format: &Format) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let length = format.content_length.unwrap_or(0);
    destination.with_file_name(format!(".{name}.{}.{length}.part", format.itag))
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
            text.push_str(&line);
            text.push('\n');
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
    stderr
        .lines()
        .map(|line| {
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
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn apply_audio_quality(command: &mut Command, codec: &str, quality: &str) {
    if let Some(bitrate) = quality
        .strip_suffix('k')
        .or_else(|| quality.strip_suffix('K'))
        .or_else(|| quality.strip_suffix('m'))
        .or_else(|| quality.strip_suffix('M'))
    {
        let suffix = &quality[quality.len() - 1..];
        command.arg("-b:a").arg(format!("{bitrate}{suffix}"));
        return;
    }
    if quality.parse::<u8>().is_ok() && matches!(codec, "libmp3lame" | "libvorbis") {
        command.arg("-q:a").arg(quality);
    }
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
}
