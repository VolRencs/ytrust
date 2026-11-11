//! Shared defaults, client versions and user agents.
//!
//! Keeping these in one place avoids the client/version drift that used to
//! exist between the player, playlist, search and download code paths.

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_PLAYLIST_LIMIT: u64 = 500;
pub const DEFAULT_PLAYLIST_ITEMS: &str = "1-";
pub const DEFAULT_FORMAT: &str = "bestvideo+bestaudio/best";
pub const DEFAULT_OUTPUT: &str = "{title} [{id}].{ext}";
pub const DEFAULT_SUB_LANGS: &str = "all";
pub const DEFAULT_SUB_FORMAT: &str = "vtt";
pub const DEFAULT_RETRIES: u32 = 5;

pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";
pub const WEB_EMBEDDED_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
pub const VISIONOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
pub const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)";

pub const WEB_CLIENT_VERSION: &str = "2.20260707.01.00";
pub const WEB_EMBEDDED_CLIENT_VERSION: &str = "2.20261110.01.00";
pub const VISIONOS_CLIENT_VERSION: &str = "1.02";
pub const TV_CLIENT_VERSION: &str = "7.20260707.07.00";

pub const VISIONOS_CLIENT_NAME: &str = "101";
pub const TV_CLIENT_NAME: &str = "7";
pub const WEB_CLIENT_NAME: &str = "1";
pub const WEB_EMBEDDED_CLIENT_NAME: &str = "56";
