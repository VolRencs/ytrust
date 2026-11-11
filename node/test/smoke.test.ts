import assert from "node:assert/strict";
import test from "node:test";
import { probe, resolve, resolveAudio, version, errorCode, YtrustError } from "../index.js";

test("probe and version", () => {
  assert.equal(probe(), version());
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
  assert.equal(errorCode("something else"), "UNKNOWN");
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

test(
  "resolve single video",
  { skip: !process.env.YTRUST_INTEGRATION },
  async () => {
    const result = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert.equal(result._type, "video");
    assert.equal(result.id, "dQw4w9WgXcQ");
    assert.ok(Array.isArray(result.formats));
    assert.ok(result.best_audio);
    assert.equal(typeof result.best_audio.http_headers["User-Agent"], "string");
    assert.equal(result.best_audio.http_headers.Referer, "https://www.youtube.com/");
    assert.equal(typeof result.best_audio.expires_at, "number");
  },
);

test(
  "resolveAudio single video",
  { skip: !process.env.YTRUST_INTEGRATION },
  async () => {
    const audio = await resolveAudio("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert.equal(audio.id, "dQw4w9WgXcQ");
    assert.match(audio.url, /^https:\/\/.+googlevideo\.com\//);
    assert.match(audio.acodec, /^opus/);
    assert.equal(typeof audio.http_headers["User-Agent"], "string");
    assert.equal(typeof audio.expires_at, "number");
  },
);

test(
  "resolveAudio ignores an attached playlist",
  { skip: !process.env.YTRUST_INTEGRATION },
  async () => {
    const audio = await resolveAudio(
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL63F0C78739B09958",
    );
    assert.equal(audio.id, "dQw4w9WgXcQ");
    assert.match(audio.acodec, /^opus/);
  },
);
