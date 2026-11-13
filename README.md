# ytrust

Fast, modern YouTube downloader and resolver written in Rust. It supports
videos, Shorts, live streams, regular and inline/radio playlists, search,
separate audio/video streams, cookies and proxies.

`ytrust` is used as both a CLI and an in-process Node.js module (napi-rs), so
applications can resolve YouTube metadata and media URLs without spawning an
external binary.

## Requirements

- Rust 1.99+ to build from source
- Node.js 26+ for the native module (prebuilt addon targets `linux-x64-gnu`, glibc 2.35+)
- Node.js 20+ for `signatureCipher`/`n` solving in the CLI (optional; needed only
  when YouTube exposes ciphered formats)
- ffmpeg with the codecs you need when converting or merging media

## Build

```bash
cargo build --release

cd node
pnpm install
pnpm build        # produces ytrust.linux-x64-gnu.node and native.cjs
```

The Node addon ships as a prebuilt `.node` binary; consumers do not need a Rust
toolchain. It bundles yt-dlp's EJS challenge solver so `n`-challenge URLs are
returned without unresolved signatures. Fresh googlevideo URLs can still hit a
short CDN cold-start `403`; external downloaders should retry that response
(for ffmpeg, add `-reconnect_on_http_error 403`) or use
`resolveAudio(..., { ensurePlayable: true })`. Player JS URLs are cached for 30
minutes, and the preprocessed player is reused across videos to keep subsequent
resolves fast.

## CLI

```bash
# Best video + audio, merged with ffmpeg
ytrust "https://www.youtube.com/watch?v=dQw4w9WgXcQ"

# Audio only, as Opus
ytrust -x --audio-format opus "https://youtu.be/dQw4w9WgXcQ"

# Metadata only
ytrust -J "https://www.youtube.com/watch?v=dQw4w9WgXcQ"

# Playlist items 1-10
ytrust --playlist-items 1-10 "https://www.youtube.com/playlist?list=..."

# Cookies for age-restricted or bot-checked videos
ytrust --cookies cookies.txt "https://www.youtube.com/watch?v=..."

# Multiple dubbed audio tracks in one container
ytrust -f 'bestvideo+bestaudio[language=en]+bestaudio[language=de]' \
  --audio-multistreams "https://www.youtube.com/watch?v=..."

# Machine-readable progress events for a UI
ytrust --progress-json -P ~/Downloads "https://www.youtube.com/watch?v=..."

# Explicit JavaScript runtime for ciphered-only videos
ytrust --js-runtimes node:/usr/bin/node "https://www.youtube.com/watch?v=..."
```

Run `ytrust --help` for the full list of options. The CLI bundles the same EJS
challenge solver as the Node module and runs it through a detected Node.js
runtime; pass `--js-runtimes none` to disable it, or `node:PATH` to select a
specific binary. `--download-sections` accepts one range such as `*120-inf` or
`*1:00-2:30`. `-j` prints one JSON object per video, while `-J` prints one object
per URL (a playlist object for playlists). `-j --flat-playlist` emits playlist
entries without fetching every video. `--ignore-errors` continues after a
per-item failure and exits with status 1 when anything failed. `--skip-download`
skips media but still writes requested thumbnails and subtitles; `--simulate`
writes nothing.

Fresh googlevideo URLs can return a transient `403` for a few seconds while
YouTube's CDN edge fetches the stream. ytrust retries the same URL with a short
backoff (even when `--retries 0` is used) before treating the stream as
rejected. If a `403`/`410` persists after those cold-start retries, ytrust
retries once with a muxed format and reports a `fallback` progress event. Pass
`--no-fallback` to disable the muxed retry.

### Progress JSON

`--progress-json` writes newline-delimited JSON to stdout. Human status and
progress bars stay on stderr. Events are versioned with `"v": 1` and include
`start`, `progress`, `postprocess`, `file`, `done`, `error`, `fallback` and
`playlist`.
Every `done` event contains an absolute `filepath` for the final media file
when a file was written; simulate/skip events may report `null`. Bytes, speed
and ETA are aggregated across separate video/audio downloads; the
`components` array exposes per-stream progress, and playlist events include
`playlist_index`/`playlist_count`. `--progress-json` conflicts with `-j`, `-J`,
`-F`, `--list-audio-tracks` and `-o -`.

### Audio tracks and formats

Every format in `-J` output includes `language`, and `audio_track` includes
`id`, `display_name`, `audio_is_default` and `language`. Use format filters:

```bash
-f 'bestaudio[language=en]'
-f 'bestaudio[language^=en]'
-f 'bestaudio[language!=de]'
-f 'bestvideo+bestaudio[language=en]+bestaudio[language=de]'
```

Multiple audio tracks require `--audio-multistreams` in video mode and are
merged with language metadata. In audio-only mode (`-x`) each selected language
is written to its own file instead. `--embed-subs` downloads the selected
subtitle tracks automatically, `--remux-video` selects the final container, and
`--audio-quality` accepts `0`-`10` (VBR for mp3/vorbis, mapped to an equivalent
bitrate for other codecs) or a bitrate such as `192K`.

## Node.js module

The package is shipped as a release tarball (`ytrust-<version>.tgz`); install it
with `npm install ./ytrust-<version>.tgz`.

```js
import { resolve, resolveAudio } from "ytrust";

const meta = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
console.log(meta.title, meta.duration, meta.best_audio?.format_id);

const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
console.log(audio.url, audio.http_headers["User-Agent"], audio.expires_at);
```

CommonJS is supported through Node's `require(esm)`:

```js
const { resolveAudio } = require("ytrust");
```

Challenge solving runs in a bounded worker pool, so resolving many videos does
not block the Node event loop.

### `resolve(input, options?)`

Returns the same shape as `yt-dlp -J`: `_type: "video"` for a single video or
`_type: "playlist"` for playlists and searches. Video results include a
`best_audio` field preselected for playback, and every format includes:

- `http_headers` — headers required to fetch `url` (`User-Agent`, `Referer`);
- `expires_at` — Unix timestamp parsed from the googlevideo `expire` parameter;
- `is_hls` — true for HLS manifest entries;
- `language` — audio track language when YouTube reports one;
- `filesize_approx` — estimated size when the exact size is unknown;
- `playable` — false when a signature/`n` challenge is still unresolved.

### `resolveAudio(input, options?)`

Resolves a single video and returns its best playable audio format with the
video metadata attached. Prefers the default audio track, then audio-only over
muxed streams, then Opus, then bitrate. Throws `CHALLENGE_FAILED` when every
audio URL remains unsolvable. Always resolves the video, ignoring any playlist
attached to a watch URL.

### Options

| Option | Default | Description |
|---|---|---|
| `cookiesPath` | — | Netscape `cookies.txt` file |
| `cookiesFromBrowser` | — | Browser profile, e.g. `firefox`, `firefox:profile` or `firefox:/path` |
| `proxy` | — | `http`, `https` or `socks5` proxy URL |
| `timeout` | `30` | HTTP timeout in seconds (minimum `1`) |
| `playlistLimit` | `500` | Maximum playlist entries to fetch (minimum `1`) |
| `playlistItems` | `"1-"` | Playlist ranges, e.g. `1-10,15` |
| `noPlaylist` | `false` | Ignore a playlist attached to a watch URL |
| `ensurePlayable` | `false` | `resolveAudio` only: retry transient googlevideo `403`s and return the URL only after it responds successfully |

### Errors

Errors thrown by the module are `YtrustError` instances with a stable `code`:
`AGE_RESTRICTED`, `BOTCHECK`, `PRIVATE`, `UNAVAILABLE`, `COOKIES`,
`UNSUPPORTED_URL`, `INVALID_INPUT`, `TIMEOUT`, `NETWORK`, `HTTP`, `NO_AUDIO`,
`INVALID_RESPONSE`, `NATIVE_LOAD`, `FORMAT_NOT_FOUND`, `FFMPEG`, `IO`,
`CHALLENGE_FAILED`, `UNKNOWN`.

## License

GPL-3.0 — see [LICENSE](LICENSE).
