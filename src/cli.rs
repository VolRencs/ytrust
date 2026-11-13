use std::path::PathBuf;

use clap::Parser;

use ytrust::constants::{
    DEFAULT_OUTPUT, DEFAULT_PLAYLIST_ITEMS, DEFAULT_PLAYLIST_LIMIT, DEFAULT_RETRIES,
    DEFAULT_SUB_FORMAT, DEFAULT_SUB_LANGS, DEFAULT_TIMEOUT_SECS,
};
use ytrust::error::{ErrorCode, Result, YtrustError};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "ytrust",
    version,
    about = "Fast, modern YouTube downloader",
    long_about = "Fast, modern YouTube downloader.\n\nSupports videos, Shorts, live streams, playlists and separate audio/video streams.",
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// YouTube video, Shorts, live or playlist URLs
    #[arg(required = true, value_name = "URL")]
    pub urls: Vec<String>,

    /// Format selector (e.g. best, bestvideo+bestaudio, 137+140, mp4)
    #[arg(short = 'f', long, env = "YTRUST_FORMAT")]
    pub format: Option<String>,

    /// Do not retry with a muxed format when streams are still rejected after cold-start retries (HTTP 403/410)
    #[arg(long)]
    pub no_fallback: bool,

    /// Output filename template. Supports {title}, {id}, {ext}, {height}, {uploader}, {playlist}, {playlist_index}
    #[arg(short = 'o', long, default_value = DEFAULT_OUTPUT, env = "YTRUST_OUTPUT")]
    pub output: String,

    /// Directory for output files
    #[arg(short = 'P', long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,

    /// List available formats and exit
    #[arg(short = 'F', long, conflicts_with_all = ["dump_json", "dump_single_json", "list_audio_tracks", "progress_json"])]
    pub list_formats: bool,

    /// Extract audio only
    #[arg(short = 'x', long)]
    pub extract_audio: bool,

    /// Convert extracted audio to this format (mp3, m4a, opus, ogg, flac, wav)
    #[arg(long, value_name = "FORMAT")]
    pub audio_format: Option<String>,

    /// Print one JSON object per video and do not download
    #[arg(
        short = 'j',
        long = "dump-json",
        alias = "print-json",
        conflicts_with = "dump_single_json"
    )]
    pub dump_json: bool,

    /// Print one JSON object for the whole input and do not download
    #[arg(short = 'J', long = "dump-single-json")]
    pub dump_single_json: bool,

    /// Skip the media download but still write requested thumbnails/subtitles
    #[arg(long)]
    pub skip_download: bool,

    /// Resolve playlists without fetching every video (flat metadata)
    #[arg(long, requires = "dump_json")]
    pub flat_playlist: bool,

    /// Print machine-readable newline-delimited JSON progress events
    #[arg(
        long,
        env = "YTRUST_PROGRESS_JSON",
        conflicts_with_all = ["dump_json", "dump_single_json", "list_formats", "list_audio_tracks"]
    )]
    pub progress_json: bool,

    /// Continue after per-item errors and report them as JSON events
    #[arg(long)]
    pub ignore_errors: bool,

    /// JavaScript runtime for signatureCipher/n-challenge solving (node, node:PATH, PATH, none)
    #[arg(
        long,
        alias = "js-runtime",
        value_name = "RUNTIMES",
        env = "YTRUST_JS_RUNTIMES"
    )]
    pub js_runtimes: Option<String>,

    /// Merge multiple selected audio tracks into the output container
    #[arg(long)]
    pub audio_multistreams: bool,

    /// Select an audio track by id/name/language; may be repeated
    #[arg(
        long = "audio-track",
        value_name = "TRACK",
        value_delimiter = ',',
        action = clap::ArgAction::Append
    )]
    pub audio_tracks: Vec<String>,

    /// Audio encoder quality: 0-10 (VBR) or a bitrate such as 192K
    #[arg(long, value_name = "QUALITY")]
    pub audio_quality: Option<String>,

    /// Embed downloaded subtitles into the final media file
    #[arg(long)]
    pub embed_subs: bool,

    /// Remux the final media into this container (mp4, mkv or webm)
    #[arg(long, value_name = "FORMAT")]
    pub remux_video: Option<String>,

    /// yt-dlp compatibility: output encoding (UTF-8 is always used)
    #[arg(long, value_name = "ENCODING")]
    pub encoding: Option<String>,

    /// yt-dlp compatibility: sanitize names for Windows (already the default)
    #[arg(long)]
    pub windows_filenames: bool,

    /// yt-dlp compatibility: use .part files and resume (already the default)
    #[arg(long = "continue", hide = true)]
    pub continue_: bool,

    /// yt-dlp compatibility: use .part files (already the default)
    #[arg(long, hide = true)]
    pub part: bool,

    /// yt-dlp compatibility: convert downloaded subtitles to this format
    #[arg(long, value_name = "FORMAT")]
    pub convert_subs: Option<String>,

    /// yt-dlp compatibility: accepted for compatibility; ytrust uses its own retry schedule
    #[arg(long, value_name = "EXPR", hide = true)]
    pub retry_sleep: Option<String>,

    /// yt-dlp compatibility: accepted for compatibility; ytrust streams sequentially
    #[arg(long, value_name = "N", hide = true)]
    pub concurrent_fragments: Option<u32>,

    /// yt-dlp compatibility: accepted for compatibility
    #[arg(long, hide = true)]
    pub abort_on_unavailable_fragments: bool,

    /// yt-dlp compatibility: accepted for compatibility
    #[arg(long, hide = true)]
    pub force_keyframes_at_cuts: bool,

    /// yt-dlp compatibility: suppress warnings
    #[arg(long)]
    pub no_warnings: bool,

    /// yt-dlp compatibility alias for --timeout
    #[arg(long, value_name = "SECONDS")]
    pub socket_timeout: Option<u64>,

    /// yt-dlp compatibility alias for --retries
    #[arg(long, value_name = "N")]
    pub fragment_retries: Option<u32>,

    /// Download only one time range, e.g. *120-inf or *1:00-2:30
    #[arg(long, value_name = "RANGE")]
    pub download_sections: Option<String>,

    /// Download subtitles
    #[arg(long)]
    pub write_subs: bool,

    /// Download automatic subtitles
    #[arg(long)]
    pub write_auto_subs: bool,

    /// Subtitle languages: all or comma-separated codes
    #[arg(long, value_name = "LANGS", default_value = DEFAULT_SUB_LANGS)]
    pub sub_langs: String,

    /// Subtitle format: vtt or srt
    #[arg(long, value_name = "FORMAT", default_value = DEFAULT_SUB_FORMAT)]
    pub sub_format: String,

    /// Download the best thumbnail
    #[arg(long)]
    pub write_thumbnail: bool,

    /// Download all available thumbnails
    #[arg(long)]
    pub write_all_thumbnails: bool,

    /// List available audio tracks and exit
    #[arg(long)]
    pub list_audio_tracks: bool,

    /// Do not download anything
    #[arg(long)]
    pub simulate: bool,

    /// Download only the video from a URL that also contains a playlist
    #[arg(long)]
    pub no_playlist: bool,

    /// Playlist item ranges, e.g. 1-3,7,10-
    #[arg(long, value_name = "RANGES", default_value = DEFAULT_PLAYLIST_ITEMS)]
    pub playlist_items: String,

    /// Maximum number of playlist entries to inspect
    #[arg(long, default_value_t = DEFAULT_PLAYLIST_LIMIT)]
    pub playlist_limit: u64,

    /// Retries per download; cold googlevideo 403s always get a few extra attempts
    #[arg(long, default_value_t = DEFAULT_RETRIES)]
    pub retries: u32,

    /// HTTP timeout in seconds
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,

    /// Proxy URL (http, https, socks5)
    #[arg(long, env = "YTRUST_PROXY")]
    pub proxy: Option<String>,

    /// Netscape cookies.txt file
    #[arg(long, value_name = "FILE", env = "YTRUST_COOKIES")]
    pub cookies: Option<PathBuf>,

    /// Load cookies from a browser, e.g. firefox or firefox:/path/to/profile
    #[arg(
        long,
        value_name = "BROWSER[:PROFILE]",
        env = "YTRUST_COOKIES_FROM_BROWSER"
    )]
    pub cookies_from_browser: Option<String>,

    /// ffmpeg executable or directory containing it
    #[arg(long, value_name = "PATH", env = "YTRUST_FFMPEG")]
    pub ffmpeg_location: Option<PathBuf>,

    /// Container used when merging video and audio (mp4, mkv, webm)
    #[arg(long, value_name = "FORMAT")]
    pub merge_output_format: Option<String>,

    /// Overwrite existing files
    #[arg(long)]
    pub force_overwrites: bool,

    /// Make filenames safe for all platforms
    #[arg(long)]
    pub restrict_filenames: bool,

    /// Suppress progress and informational output
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Increase log verbosity (-v, -vv)
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Cli {
    /// Validate mode combinations before cookies, network access or runtime
    /// creation so invalid commands fail fast.
    pub fn validate(&self) -> Result<()> {
        if self.timeout == 0 {
            return Err(invalid("--timeout must be at least 1 second"));
        }
        if self.playlist_limit == 0 {
            return Err(invalid("--playlist-limit must be at least 1"));
        }
        ytrust::youtube::max_playlist_item(&self.playlist_items)?;
        if self.audio_quality.is_some() && !self.extract_audio && self.audio_format.is_none() {
            return Err(invalid(
                "--audio-quality requires -x/--extract-audio or --audio-format",
            ));
        }
        if let Some(quality) = &self.audio_quality {
            validate_audio_quality(quality)?;
        }
        let subtitle_format = self
            .convert_subs
            .as_deref()
            .unwrap_or(&self.sub_format)
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if subtitle_format != "vtt" && subtitle_format != "srt" {
            return Err(invalid("--sub-format/--convert-subs supports vtt or srt"));
        }
        let media_mode = !self.dump_json
            && !self.dump_single_json
            && !self.list_formats
            && !self.list_audio_tracks
            && !self.simulate;
        if self.output == "-" {
            if self.urls.len() > 1 {
                return Err(invalid(
                    "stdout output supports exactly one input URL; use a file template instead",
                ));
            }
            for url in &self.urls {
                if media_mode
                    && matches!(
                        ytrust::youtube::classify(url, self.no_playlist)?,
                        ytrust::youtube::Target::Playlist { .. }
                            | ytrust::youtube::Target::Search { .. }
                    )
                {
                    return Err(invalid(
                        "stdout output requires a single video URL, not a playlist or search",
                    ));
                }
            }
            if self.progress_json {
                return Err(invalid(
                    "--progress-json cannot be combined with streaming media to stdout",
                ));
            }
            if media_mode
                && (self.extract_audio
                    || self.audio_format.is_some()
                    || self.write_thumbnail
                    || self.write_all_thumbnails
                    || self.write_subs
                    || self.write_auto_subs
                    || self.embed_subs
                    || self.remux_video.is_some())
            {
                return Err(invalid(
                    "stdout output does not support audio extraction, subtitles, thumbnails, embedding or remuxing",
                ));
            }
        }
        if self.embed_subs && (self.extract_audio || self.audio_format.is_some()) {
            return Err(invalid("--embed-subs is not supported in audio-only mode"));
        }
        if self.remux_video.is_some() && (self.extract_audio || self.audio_format.is_some()) {
            return Err(invalid("--remux-video is not supported in audio-only mode"));
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> YtrustError {
    YtrustError::coded(ErrorCode::InvalidInput, message)
}

fn validate_audio_quality(quality: &str) -> Result<()> {
    let quality = quality.trim();
    if quality.is_empty() {
        return Err(invalid("--audio-quality cannot be empty"));
    }
    if let Some(value) = quality
        .strip_suffix('k')
        .or_else(|| quality.strip_suffix('K'))
        .or_else(|| quality.strip_suffix('m'))
        .or_else(|| quality.strip_suffix('M'))
        .filter(|value| !value.is_empty())
    {
        if value
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .is_none()
        {
            return Err(invalid(format!("invalid audio bitrate: {quality}")));
        }
        return Ok(());
    }
    match quality.parse::<u8>() {
        Ok(value) if value <= 10 => Ok(()),
        _ => Err(invalid(
            "--audio-quality must be 0-10 (VBR) or a bitrate such as 192K",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_fallback_flag() {
        let cli = Cli::try_parse_from([
            "ytrust",
            "--no-fallback",
            "https://www.youtube.com/watch?v=x",
        ])
        .unwrap();
        assert!(cli.no_fallback);
    }
}
