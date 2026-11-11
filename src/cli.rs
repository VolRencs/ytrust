use std::path::PathBuf;

use clap::Parser;

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
    #[arg(
        short = 'f',
        long,
        default_value = "bestvideo+bestaudio/best",
        env = "YTRUST_FORMAT"
    )]
    pub format: String,

    /// Output filename template. Supports {title}, {id}, {ext}, {height}, {uploader}, {playlist}, {playlist_index}
    #[arg(
        short = 'o',
        long,
        default_value = "{title} [{id}].{ext}",
        env = "YTRUST_OUTPUT"
    )]
    pub output: String,

    /// Directory for output files
    #[arg(short = 'P', long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,

    /// List available formats and exit
    #[arg(short = 'F', long)]
    pub list_formats: bool,

    /// Extract audio only
    #[arg(short = 'x', long)]
    pub extract_audio: bool,

    /// Convert extracted audio to this format (mp3, m4a, opus, flac, wav)
    #[arg(long, value_name = "FORMAT")]
    pub audio_format: Option<String>,

    /// Print metadata as JSON and do not download
    #[arg(short = 'j', long = "dump-json", alias = "print-json")]
    pub dump_json: bool,

    /// Print one JSON object for the whole input and do not download
    #[arg(short = 'J', long = "dump-single-json")]
    pub dump_single_json: bool,

    /// yt-dlp compatibility: skip media download
    #[arg(long)]
    pub skip_download: bool,

    /// yt-dlp compatibility: list playlist entries without full extraction
    #[arg(long)]
    pub flat_playlist: bool,

    /// yt-dlp compatibility: suppress warnings
    #[arg(long)]
    pub no_warnings: bool,

    /// yt-dlp compatibility alias for --timeout
    #[arg(long, value_name = "SECONDS")]
    pub socket_timeout: Option<u64>,

    /// yt-dlp compatibility: fragment retry count
    #[arg(long, value_name = "N")]
    pub fragment_retries: Option<u32>,

    /// Download only a time range, e.g. *120-inf
    #[arg(long, value_name = "RANGE")]
    pub download_sections: Option<String>,

    /// Download subtitles
    #[arg(long)]
    pub write_subs: bool,

    /// Download automatic subtitles
    #[arg(long)]
    pub write_auto_subs: bool,

    /// Subtitle languages: all or comma-separated codes
    #[arg(long, value_name = "LANGS", default_value = "all")]
    pub sub_langs: String,

    /// Subtitle format: vtt or srt
    #[arg(long, value_name = "FORMAT", default_value = "vtt")]
    pub sub_format: String,

    /// Download the best thumbnail
    #[arg(long)]
    pub write_thumbnail: bool,

    /// Download all available thumbnails
    #[arg(long)]
    pub write_all_thumbnails: bool,

    /// Select a YouTube audio track by id or display name, or "default"
    #[arg(long, value_name = "TRACK")]
    pub audio_track: Option<String>,

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
    #[arg(long, value_name = "RANGES", default_value = "1-")]
    pub playlist_items: String,

    /// Maximum number of playlist entries to inspect
    #[arg(long, default_value_t = 500)]
    pub playlist_limit: u64,

    /// Retries per download
    #[arg(long, default_value_t = 5)]
    pub retries: u32,

    /// HTTP timeout in seconds
    #[arg(long, default_value_t = 30)]
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
