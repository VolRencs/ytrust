import assert from "node:assert/strict";
import test from "node:test";
import { probe, resolve, version } from "../index.js";

test("probe and version", () => {
  assert.equal(probe(), version());
  assert.match(version(), /^\d+\.\d+\.\d+/);
});

test(
  "resolve single video",
  { skip: !process.env.YTRUST_INTEGRATION },
  async () => {
    const result = await resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert.equal(result._type, "video");
    assert.equal(result.id, "dQw4w9WgXcQ");
    assert.ok(Array.isArray(result.formats));
  },
);
