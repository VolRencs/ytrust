# ytrust

Fast, modern YouTube downloader and resolver written in Rust. It supports
videos, Shorts, live streams, regular and inline/radio playlists, search,
separate audio/video streams, cookies and proxies.

`ytrust` is used as both a CLI and an in-process Node.js module (napi-rs), so
applications can resolve YouTube metadata and media URLs without spawning an
external binary.

## Requirements

- Rust 1.99+ to build from source
- Node.js 24+ for the native module (prebuilt addon targets `linux-x64-gnu`, glibc 2.35+)
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
returned ready for ffmpeg or another downloader.

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
```

Run `ytrust --help` for the full list of options. Ciphered-only formats are
included in JSON but the CLI does not run the JavaScript challenge solver; use
the Node module when a video exposes only `signatureCipher` formats. `--download-sections`
accepts one range such as `*120-inf` or `*1:00-2:30`. `-j` prints one JSON
object per video, while `-J` prints one object per URL (a playlist object for
playlists).

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
- `is_hls` — true for HLS manifest entries.

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
| `cookiesFromBrowser` | — | Browser profile, e.g. `firefox` or `firefox:/path` |
| `proxy` | — | `http`, `https` or `socks5` proxy URL |
| `timeout` | `30` | HTTP timeout in seconds |
| `playlistLimit` | `500` | Maximum playlist entries to fetch |
| `playlistItems` | `"1-"` | Playlist ranges, e.g. `1-10,15` |
| `noPlaylist` | `false` | Ignore a playlist attached to a watch URL |

### Errors

Errors thrown by the module are `YtrustError` instances with a stable `code`:
`AGE_RESTRICTED`, `BOTCHECK`, `PRIVATE`, `UNAVAILABLE`, `COOKIES`,
`UNSUPPORTED_URL`, `INVALID_INPUT`, `TIMEOUT`, `NETWORK`, `HTTP`, `NO_AUDIO`,
`INVALID_RESPONSE`, `NATIVE_LOAD`, `FORMAT_NOT_FOUND`, `FFMPEG`, `IO`,
`CHALLENGE_FAILED`, `UNKNOWN`.

## License

GPL-3.0 — see [LICENSE](LICENSE).
