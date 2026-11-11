# ytrust for Node.js

Native napi-rs bindings for [ytrust](../README.md). The package ships a
prebuilt Linux x64 glibc addon, so no Rust toolchain is required at install
time.

```js
import { resolve, resolveAudio } from "ytrust";

const meta = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
```

## API

- `resolve(input, options?)` — metadata and formats, `yt-dlp -J` compatible.
- `resolveAudio(input, options?)` — best audio format plus video metadata.
- `probe()` / `version()` — addon version string.
- `YtrustError` — error class with a stable `code`.
- `errorCode(message)` — classify an error message.

Every format includes `http_headers` and `expires_at`. The module bundles
yt-dlp's EJS solver and transparently rewrites `n`-challenge URLs before
returning results. See the root README for the full option table.

## Build

```bash
pnpm install
pnpm build
pnpm test
pnpm test:integration   # network tests against YouTube
```

The release build uses the `napi` Cargo profile, which unwinds panics so napi-rs
can reject the JavaScript promise instead of aborting the host process.

## License

GPL-3.0 — see [LICENSE](LICENSE).
