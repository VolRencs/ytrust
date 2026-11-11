# Changelog

## 0.1.3

- Fixed the Node package `LICENSE` file to contain the GPL-3.0 text.

## 0.1.2

- Relicensed under GPL-3.0.

## 0.1.1

- Added `http_headers` (`User-Agent`, `Referer`) and `expires_at` to every
  format in JSON output.
- Added `best_audio` to video JSON with the preferred playable audio stream.
- Added `resolveAudio()` to the Node.js module.
- Added `YtrustError` with stable error codes and `errorCode()` to the Node.js
  module.
- The Node addon now builds with the `napi` Cargo profile (`panic = "unwind"`),
  so Rust panics become rejected promises instead of aborting the host process.
- Added README docs for the CLI and Node.js API.

## 0.1.0

- Initial release: YouTube videos, Shorts, live streams, playlists, search,
  separate audio/video formats, cookies and proxies.
- CLI and napi-rs Node.js bindings.
