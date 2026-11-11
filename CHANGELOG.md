# Changelog

## 0.2.0

- `watch?v=...&list=...` URLs are now resolved as inline playlists (including
  YouTube radio/mix `RD...` playlists), with `next` pagination matching
  yt-dlp. This fixes radio links returning an empty playlist.
- Added cookie authentication headers (`SAPISIDHASH`, `SAPISID1PHASH`,
  `SAPISID3PHASH`) using the account user-session ID from ytcfg.
- Added a `web_embedded` player fallback with visitor data, player JS URL and
  encrypted host flags for videos that reject the regular clients.
- Video JSON includes `player_js_url`.
- The Node module now bundles yt-dlp's EJS challenge solver and rewrites `n`
  challenge URLs before returning formats or `resolveAudio`.
- Cookie headers now include the full YouTube cookie set instead of only the
  essential names, matching yt-dlp.

## 0.1.3

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
