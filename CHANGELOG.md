# Changelog

## 0.4.3

- Cached the player JS URL for 30 minutes in the native addon, avoiding a
  watch-page request for every video.
- Cached the EJS-preprocessed player source in the Node module, cutting
  challenge-solving overhead from roughly 0.8 s to 0.1 s on subsequent videos.
- `resolveAudio()` now solves challenges for the preferred audio formats first
  and only falls back to a full solve when none of them becomes playable.
- Added solver tests for preprocessed player input.

## 0.4.2

- Fixed the Node challenge solver passing a JSON string instead of an object to
  the EJS `jsc` entrypoint, which made `resolve()` and `resolveAudio()` fail
  with `CHALLENGE_FAILED` for ciphered formats.
- Added a regression test for the solver input contract.

## 0.4.1

- Node.js 26 is now the only supported runtime.
- The Node addon CI/release build uses an isolated Rust cache key on Ubuntu
  22.04, so cached artifacts built on newer glibc cannot leak into the shipped
  addon and break loading on glibc 2.35.
- Added a CI smoke-load step for `native.cjs` to fail fast if the addon cannot
  be loaded.

## 0.4.0

### Added

- `--download-sections` now supports one time range for file downloads and
  stdout, including `HH:MM:SS`, `inf` and `*` prefixes.
- Stable structured error codes shared by Rust and the Node module:
  `FORMAT_NOT_FOUND`, `FFMPEG`, `IO`, `CHALLENGE_FAILED` and the existing codes.
- Node challenge solving runs in a bounded worker pool and player JS is fetched
  through the native proxy/timeout/cookies configuration.
- `--dump-json` now prints one object per playlist/search entry; `-J` still
  prints one playlist object per URL.

### Changed

- Shared resolve pipeline between the CLI and Node addon.
- Shared HTTP client cache, typed Innertube responses and request read timeouts.
- Search pagination now honours `playlistLimit` and fetches continuations.
- `-x` honours `-f`, falls back to muxed audio and `--audio-format` implies
  audio extraction.
- HLS and section downloads pass User-Agent/Referer/Cookie headers to ffmpeg.
- YouTube cookies are no longer sent to googlevideo/i.ytimg hosts.
- Node.js 26+ and both ESM/CommonJS are supported; the addon is built against
  glibc 2.35 for modern Linux distributions.
- `best_audio` prefers the default audio track and playable URLs.
- CLI `--quiet` hides progress and informational status; `--simulate` no longer
  writes thumbnails or subtitles.
- Filenames are truncated on UTF-8 boundaries; Windows reserved names are
  prefixed and `--restrict-filenames` replaces spaces.
- Download retries/resume validate sizes, work on Windows and do not silently
  accept stale partial files.

### Fixed

- Ciphered formats from the regular player clients now get `player_js_url`, so
  the Node solver can rewrite `sig`/`n` challenges for the main resolve path.
- `resolveAudio` never returns an empty URL; it falls back to a playable audio
  format or throws `CHALLENGE_FAILED`.
- Open-ended playlist ranges beyond the playlist length no longer fail.
- `parse_item_ranges` and playlist/search item filtering are consistent between
  CLI downloads and JSON output.
- `original_url` now contains the caller's URL for videos.
- Missing cookies files are reported as `COOKIES` instead of `UNKNOWN`.
- `--write-thumbnail` cannot overwrite the media file; manual and automatic
  subtitles no longer overwrite each other.
- `--socket-timeout`/`--timeout` are applied to response bodies as well.
- Redacted signed URLs in logs.
- `--merge-output-format` and `--audio-format` are validated before downloading.
- `--fragment-retries` is honoured as the download retry count.

## 0.3.0

- Ciphered formats (`signatureCipher`/`cipher`) are no longer discarded.
  `url` remains empty for them and the raw cipher is included in JSON.
- The Node module now solves both `sig` and `n` challenges with the bundled
  EJS solver and rebuilds ciphered URLs before returning results.
- `best_audio` falls back to HLS for live streams when no progressive audio
  format exists.
- Unified cookie header construction; the full YouTube cookie set is used for
  API/player requests.
- Added JSON and solver tests for ciphered formats.

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
