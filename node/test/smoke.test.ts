import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";
import { errorCode, resolve, resolveAudio, version, YtrustError } from "../index.js";

const integrationOptions = process.env.YTRUST_COOKIES ? { cookiesPath: process.env.YTRUST_COOKIES } : {};

test("version", () => {
  assert.match(version(), /^\d+\.\d+\.\d+/);
});

test("errorCode classifies common failures", () => {
  assert.equal(errorCode("YouTube: Sign in to confirm you're not a bot"), "BOTCHECK");
  assert.equal(errorCode("YouTube: Sign in to confirm your age"), "AGE_RESTRICTED");
  assert.equal(errorCode("failed to load cookies file"), "COOKIES");
  assert.equal(errorCode("not a YouTube URL: https://example.com/x"), "UNSUPPORTED_URL");
  assert.equal(errorCode("channel URLs are not supported yet; use a video or playlist URL"), "UNSUPPORTED_URL");
  assert.equal(errorCode("playlist URL has no list parameter"), "INVALID_INPUT");
  assert.equal(errorCode("invalid URL: "), "INVALID_INPUT");
  assert.equal(errorCode("YouTube: This video is unavailable"), "UNAVAILABLE");
  assert.equal(errorCode("YouTube request timed out"), "TIMEOUT");
  assert.equal(errorCode("format not available: 137"), "FORMAT_NOT_FOUND");
  assert.equal(errorCode("ffmpeg was not found"), "FFMPEG");
  assert.equal(errorCode("I/O: No such file or directory"), "IO");
  assert.equal(errorCode("[IO] I/O: Connection reset by peer"), "IO");
  assert.equal(errorCode("[BOTCHECK] Sign in to confirm you're not a bot"), "BOTCHECK");
  assert.equal(errorCode("challenge solving failed"), "CHALLENGE_FAILED");
  assert.equal(errorCode("JSON: expected value"), "INVALID_RESPONSE");
  assert.equal(errorCode("native addon is unavailable: missing"), "NATIVE_LOAD");
  assert.equal(errorCode("something else"), "UNKNOWN");
});

test("CommonJS require() exposes the same API", () => {
  const require = createRequire(import.meta.url);
  const loaded = require("../index.js");
  assert.equal(loaded.version(), version());
  assert.equal(typeof loaded.resolve, "function");
  assert.equal(typeof loaded.resolveAudio, "function");
});

test("resolve wraps native errors", async () => {
  await assert.rejects(
    () => resolve("https://example.com/x"),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "UNSUPPORTED_URL");
      assert.match(error.message, /not a YouTube URL/);
      return true;
    },
  );
});

test("resolve maps missing cookies files to COOKIES", async () => {
  await assert.rejects(
    () =>
      resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", {
        cookiesPath: "/definitely/not/a/cookies.txt",
      }),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "COOKIES");
      assert.match(error.message, /failed to load cookies file/);
      assert.doesNotMatch(error.message, /^\[/);
      return true;
    },
  );
});

test("resolveAudio maps missing cookies files to COOKIES", async () => {
  await assert.rejects(
    () =>
      resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ", {
        cookiesPath: "/definitely/not/a/cookies.txt",
      }),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "COOKIES");
      return true;
    },
  );
});

test("resolve maps missing playlist list to INVALID_INPUT", async () => {
  await assert.rejects(
    () => resolve("https://www.youtube.com/playlist"),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
});

test("resolve validates options before calling native code", async () => {
  await assert.rejects(
    () => resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", { timeout: -1 }),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
  await assert.rejects(
    // @ts-expect-error testing unknown option validation
    () => resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", { cookiePath: "typo.txt" }),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
  await assert.rejects(
    // @ts-expect-error testing runtime validation
    () => resolve(null),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
  await assert.rejects(
    // @ts-expect-error testing ensurePlayable validation
    () => resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ", { ensurePlayable: "yes" }),
    (error) => {
      assert.ok(error instanceof YtrustError);
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
});

test("resolve single video", { skip: !process.env.YTRUST_INTEGRATION }, async () => {
  const result = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", integrationOptions);
  assert.equal(result._type, "video");
  assert.equal(result.id, "dQw4w9WgXcQ");
  assert.ok(Array.isArray(result.formats));
  assert.ok(result.best_audio);
  assert.equal(result.best_audio.playable, true);
  assert.equal(typeof result.best_audio.http_headers["User-Agent"], "string");
  assert.equal(result.best_audio.http_headers.Referer, "https://www.youtube.com/");
  assert.equal(typeof result.best_audio.expires_at, "number");
});

test("resolveAudio single video", { skip: !process.env.YTRUST_INTEGRATION }, async () => {
  const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ", integrationOptions);
  assert.equal(audio.id, "dQw4w9WgXcQ");
  assert.equal(audio.playable, true);
  assert.match(audio.url, /^https:\/\/.+googlevideo\.com\//);
  assert.match(audio.acodec, /^opus/);
  assert.equal(typeof audio.http_headers["User-Agent"], "string");
  assert.equal(typeof audio.expires_at, "number");
});

test("resolveAudio solves signatureCipher formats", {
  skip: !process.env.YTRUST_INTEGRATION || !process.env.YTRUST_COOKIES,
}, async () => {
  const audio = await resolveAudio("https://www.youtube.com/watch?v=0t9dU9iNyYo", {
    ...(process.env.YTRUST_COOKIES ? { cookiesPath: process.env.YTRUST_COOKIES } : {}),
  });
  assert.match(audio.url, /^https:\/\/.+googlevideo\.com\//);
  assert.ok(audio.url.includes("sig="));
  assert.ok(new URL(audio.url).searchParams.has("n"));
});

test("resolveAudio ensurePlayable warms the URL", { skip: !process.env.YTRUST_INTEGRATION }, async () => {
  const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ", {
    ...integrationOptions,
    ensurePlayable: true,
  });
  const response = await fetch(audio.url, {
    headers: { ...audio.http_headers, Range: "bytes=0-0" },
  });
  assert.ok(response.ok);
  await response.arrayBuffer();
});

test("resolveAudio ignores an attached playlist", { skip: !process.env.YTRUST_INTEGRATION }, async () => {
  const audio = await resolveAudio(
    "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL63F0C78739B09958",
    integrationOptions,
  );
  assert.equal(audio.id, "dQw4w9WgXcQ");
  assert.match(audio.acodec, /^opus/);
});
