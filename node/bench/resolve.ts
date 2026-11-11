import { performance } from "node:perf_hooks";
import { resolve } from "../index.js";

const VIDEO = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
const PLAYLIST = "https://www.youtube.com/playlist?list=PL63F0C78739B09958";
const RUNS = Number(process.env.RUNS ?? 5);

async function measure(label: string, input: string, options: Record<string, unknown> = {}) {
  const times: number[] = [];
  for (let i = 0; i < RUNS; i++) {
    const start = performance.now();
    await resolve(input, options);
    times.push(Math.round(performance.now() - start));
  }
  times.sort((a, b) => a - b);
  const median = times[Math.floor(times.length / 2)];
  console.log(`${label.padEnd(34)} median ${String(median).padStart(5)} ms  runs ${times.join(" ")}`);
}

await resolve(VIDEO);
await measure("napi resolve single video", VIDEO);

await resolve(PLAYLIST, { playlistItems: "1-5", playlistLimit: 5 });
await measure("napi resolve playlist 1-5", PLAYLIST, {
  playlistItems: "1-5",
  playlistLimit: 5,
});
