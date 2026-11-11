pub mod cookies;
pub mod download;
pub mod error;
pub mod format;
pub mod json;
pub mod output;
pub mod youtube;

pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
