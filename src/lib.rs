pub mod api;
pub mod challenge;
pub mod constants;
pub mod cookies;
pub mod download;
pub mod error;
pub mod format;
pub mod http;
pub mod json;
pub mod output;
pub mod progress;
pub mod util;
pub mod youtube;

pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
