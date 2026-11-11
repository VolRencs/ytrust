# ytrust for Node.js

Native napi-rs bindings for [ytrust](https://github.com/VolRencs/ytrust). The
package ships a prebuilt Linux x64 glibc addon (glibc 2.35+), so no Rust
toolchain is required at install time. Node.js 24+ is supported; both ESM and
CommonJS consumers are supported.

```js
import { resolve, resolveAudio } from "ytrust";

const meta = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
```

```js
const { resolveAudio } = require("ytrust");
```

## API

- `resolve(input, options?)` — metadata and formats, `yt-dlp -J` compatible.
- `resolveAudio(input, options?)` — best playable audio format plus video
  metadata. Throws `CHALLENGE_FAILED` if every audio format remains unsolvable.
- `probe()` / `version()` — addon version string (`probe` is a deprecated alias).
- `YtrustError` — error class with a stable `code`.
- `errorCode(message)` — classify a raw error message.

Every format includes `http_headers`, `expires_at` and `is_hls`. Ciphered
formats are solved with yt-dlp's EJS bundle in a bounded worker pool, and
player JS is fetched through the same proxy/timeout/cookies configuration as
the API requests. This keeps challenge solving off the Node event loop.

## Browser cookies

`cookiesFromBrowser` supports Firefox and Chromium-based browsers on Linux and
Windows. Chromium cookies encrypted with platform keyrings (macOS Keychain,
newer Windows app-bound encryption) are skipped with a warning; export a
`cookies.txt` or use Firefox in that case.

## Build

```bash
pnpm install
pnpm build
pnpm test
pnpm test:integration   # network tests against YouTube
pnpm lint               # Biome
```

The release build uses the `napi` Cargo profile, which unwinds panics so napi-rs
can reject the JavaScript promise instead of aborting the host process.

## License

GPL-3.0 — see [LICENSE](https://github.com/VolRencs/ytrust/blob/main/LICENSE).
