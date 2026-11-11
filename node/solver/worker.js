import { parentPort } from "node:worker_threads";
import { solve } from "./index.js";

if (!parentPort) {
  throw new Error("ytrust solver worker must run inside a worker thread");
}

parentPort.on("message", ({ id, player, requests }) => {
  try {
    const output = solve(player, requests);
    parentPort.postMessage({ id, ok: true, output });
  } catch (error) {
    const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    parentPort.postMessage({ id, ok: false, error: message });
  }
});
