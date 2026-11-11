use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::youtube::Format;
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::error::{Result, YtrustError};

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub retries: u32,
    pub timeout: u64,
    pub proxy: Option<String>,
    pub cookies: Option<String>,
    pub ffmpeg: Option<PathBuf>,
    pub force_overwrites: bool,
}

pub struct Downloader {
    client: reqwest::Client,
    config: DownloadConfig,
    multi: MultiProgress,
}

impl Downloader {
    pub fn new(config: DownloadConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.timeout.max(5)))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/87.0.4280.101 Safari/537.36",
            );

        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|err| YtrustError::msg(format!("invalid proxy: {err}")))?,
            );
        }

        if let Some(cookies) = &config.cookies {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::COOKIE,
                reqwest::header::HeaderValue::from_str(cookies)
                    .map_err(|err| YtrustError::msg(format!("invalid cookie header: {err}")))?,
            );
            builder = builder.default_headers(headers);
        }

        Ok(Self {
            client: builder.build()?,
            config,
            multi: MultiProgress::new(),
        })
    }

    pub async fn download_format(
        &self,
        format: &Format,
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
    ) -> Result<()> {
        if format.is_hls {
            self.download_manifest(format, destination, label, duration_secs)
                .await
        } else {
            self.download_http(format, destination, label).await
        }
    }

    async fn download_http(&self, format: &Format, destination: &Path, label: &str) -> Result<()> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let part = part_path(destination);
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

            match self.download_http_once(format, &part, total, &bar).await {
                Ok(()) => {
                    if destination.exists() && self.config.force_overwrites {
                        tokio::fs::remove_file(destination).await?;
                    }
                    tokio::fs::rename(&part, destination).await?;
                    bar.finish_with_message(format!("{label} [done]"));
                    return Ok(());
                }
                Err(err) => {
                    last_error = Some(err);
                    bar.set_message(format!("{label} [retrying]"));
                }
            }
        }

        bar.abandon_with_message(format!("{label} [failed]"));
        Err(last_error.unwrap_or_else(|| YtrustError::msg(format!("failed to download {label}"))))
    }

    async fn download_http_once(
        &self,
        format: &Format,
        part: &Path,
        total: Option<u64>,
        bar: &ProgressBar,
    ) -> Result<()> {
        let existing = tokio::fs::metadata(part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if let Some(total) = total
            && existing >= total
        {
            bar.set_position(total);
            return Ok(());
        }

        let mut request = self
            .client
            .get(&format.url)
            .header(reqwest::header::RANGE, format!("bytes={existing}-"));
        request = request
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, &format.user_agent);

        let response = request.send().await?;
        let status = response.status();
        debug!("HTTP {status} for {}", format.url);

        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            if total.is_some_and(|total| existing >= total) {
                return Ok(());
            }
            return Err(YtrustError::msg("server rejected resume range"));
        }

        if !status.is_success() {
            return Err(YtrustError::msg(format!(
                "unexpected HTTP status {status} for {}",
                format.url
            )));
        }

        let append = status == reqwest::StatusCode::PARTIAL_CONTENT && existing > 0;
        let response_total = response.content_length().or(total);
        if let Some(response_total) = response_total {
            bar.set_length(response_total);
        }
        if append {
            bar.set_position(existing);
        } else {
            bar.set_position(0);
        }

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
        }
        file.flush().await?;
        bar.set_position(bar.position().max(bar.length().unwrap_or(0)));
        Ok(())
    }

    async fn download_manifest(
        &self,
        format: &Format,
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
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
            .arg("-y")
            .arg("-i")
            .arg(&format.url)
            .arg("-c")
            .arg("copy");

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

        run_ffmpeg(command, bar, label, self.config.proxy.as_deref()).await
    }

    pub async fn merge(
        &self,
        video: &Path,
        audio: &Path,
        destination: &Path,
        label: &str,
        duration_secs: Option<u64>,
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
            .arg("-y")
            .arg("-i")
            .arg(video)
            .arg("-i")
            .arg(audio)
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("1:a:0")
            .arg("-c")
            .arg("copy");

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

        run_ffmpeg(command, bar, label, self.config.proxy.as_deref()).await
    }

    pub async fn convert_audio(
        &self,
        input: &Path,
        destination: &Path,
        audio_format: &str,
        label: &str,
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let codec = match audio_format.to_ascii_lowercase().as_str() {
            "mp3" => "libmp3lame",
            "m4a" | "aac" => "aac",
            "opus" => "libopus",
            "flac" => "flac",
            "wav" => "pcm_s16le",
            other => {
                return Err(YtrustError::msg(format!(
                    "unsupported audio format: {other}"
                )));
            }
        };

        let bar = self.progress_bar(None, label);
        let ffmpeg = self.ffmpeg_path().await?;
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-vn")
            .arg("-c:a")
            .arg(codec)
            .arg("-progress")
            .arg("pipe:1")
            .arg("-nostats")
            .arg(destination);

        run_ffmpeg(command, bar, label, self.config.proxy.as_deref()).await
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
                tokio::time::sleep(Duration::from_secs(1 << attempt.min(5))).await;
            }

            let request = self
                .client
                .get(url)
                .header(reqwest::header::REFERER, "https://www.youtube.com/");
            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    let mut file = tokio::fs::File::create(&part).await?;
                    let mut stream = response.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        file.write_all(&chunk).await?;
                        bar.inc(chunk.len() as u64);
                    }
                    file.flush().await?;
                    tokio::fs::rename(&part, destination).await?;
                    bar.finish_with_message(format!("{label} [done]"));
                    return Ok(());
                }
                Ok(response) => {
                    last_error = Some(YtrustError::msg(format!(
                        "HTTP {} for {url}",
                        response.status()
                    )));
                }
                Err(err) => last_error = Some(YtrustError::from(err)),
            }
        }

        bar.abandon_with_message(format!("{label} [failed]"));
        Err(last_error.unwrap_or_else(|| YtrustError::msg(format!("failed to download {label}"))))
    }

    pub async fn stream_to_stdout(
        &self,
        format: &Format,
        offset_secs: Option<u64>,
        label: &str,
    ) -> Result<()> {
        if offset_secs.is_some() || format.is_hls {
            return self.stream_via_ffmpeg(format, offset_secs, label).await;
        }

        let bar = self.progress_bar(None, label);
        let response = self
            .client
            .get(&format.url)
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .header(reqwest::header::USER_AGENT, &format.user_agent)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(YtrustError::msg(format!(
                "unexpected HTTP status {status} for {}",
                format.url
            )));
        }

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
        offset_secs: Option<u64>,
        label: &str,
    ) -> Result<()> {
        let ffmpeg = self.ffmpeg_path().await?;
        let mut command = Command::new(ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin");
        if let Some(offset) = offset_secs.filter(|offset| *offset > 0) {
            command.arg("-ss").arg(offset.to_string());
        }
        command
            .arg("-i")
            .arg(&format.url)
            .arg("-map")
            .arg("0:a:0")
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
            .ok_or_else(|| YtrustError::msg("ffmpeg stderr unavailable"))?;
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
                stderr: stderr.trim().to_string(),
            });
        }
        tracing::debug!("stream finished: {label}");
        Ok(())
    }

    pub async fn fetch_text(&self, url: &str) -> Result<String> {
        let response = self.client.get(url).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(YtrustError::msg(format!(
                "HTTP {status} for {url}: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        Ok(text)
    }

    pub async fn ffmpeg_available(&self) -> bool {
        self.ffmpeg_path().await.is_ok()
    }

    async fn ffmpeg_path(&self) -> Result<PathBuf> {
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
            Ok(output) if output.status.success() => Ok(path),
            _ => Err(YtrustError::FfmpegMissing),
        }
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

fn part_path(destination: &Path) -> PathBuf {
    let mut path = destination.as_os_str().to_os_string();
    path.push(".part");
    PathBuf::from(path)
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
        .ok_or_else(|| YtrustError::msg("ffmpeg stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| YtrustError::msg("ffmpeg stderr unavailable"))?;

    let progress_bar = bar.clone();
    let progress_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut last_us = 0u64;
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(value) = line.strip_prefix("out_time_us=")
                && let Ok(us) = value.parse::<u64>()
                && us >= last_us
            {
                last_us = us;
                progress_bar.set_position(us / 1_000_000);
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
        bar.abandon_with_message(format!("{label} [failed]"));
        return Err(YtrustError::Ffmpeg {
            status: status.to_string(),
            stderr: stderr.trim().to_string(),
        });
    }

    bar.finish_with_message(format!("{label} [done]"));
    Ok(())
}
