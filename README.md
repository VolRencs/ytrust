# ytrust

Fast, modern YouTube downloader and resolver written in Rust. It supports
videos, Shorts, live streams, playlists, search, separate audio/video streams,
cookies and proxies.

`ytrust` is used as both a CLI and an in-process Node.js module (napi-rs), so
applications can resolve YouTube metadata and media URLs without spawning an
external binary.

## Requirements

- Rust 1.89+ to build from source
- Node.js 26+ for the native module (prebuilt addon targets `linux-x64-gnu`)
- ffmpeg with the codecs you need when converting or merging media

## Build

```bash
cargo build --release

cd node
pnpm install
pnpm build        # produces ytrust.linux-x64-gnu.node
```

The Node addon ships as a prebuilt `.node` binary; consumers do not need a Rust
toolchain.

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

Run `ytrust --help` for the full list of options.

## Node.js module

```js
import { resolve, resolveAudio } from "ytrust";

const meta = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
console.log(meta.title, meta.duration, meta.best_audio?.format_id);

const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
console.log(audio.url, audio.http_headers["User-Agent"], audio.expires_at);
```

### `resolve(input, options?)`

Returns the same shape as `yt-dlp -J`: `_type: "video"` for a single video or
`_type: "playlist"` for playlists and searches. Video results include a
`best_audio` field preselected for playback, and every format includes:

- `http_headers` — headers required to fetch `url` (`User-Agent`, `Referer`);
- `expires_at` — Unix timestamp parsed from the googlevideo `expire` parameter.

### `resolveAudio(input, options?)`

Resolves a single video and returns its best playable audio format with the
video metadata attached. Prefers Opus audio-only streams, then any audio-only
stream, then the loudest muxed stream. Always resolves the video, ignoring any
playlist attached to a watch URL.

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
`INVALID_RESPONSE`, `NATIVE_LOAD`, `UNKNOWN`.

## License

Unlicense — public domain. See [LICENSE](LICENSE).
