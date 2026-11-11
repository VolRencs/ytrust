import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const lib = readFileSync(join(here, "lib.min.js"), "utf8");
const core = readFileSync(join(here, "core.min.js"), "utf8");

let context;

function solverContext() {
  if (context) return context;
  context = vm.createContext({ console });
  vm.runInContext(lib, context, { filename: "yt.solver.lib.js" });
  vm.runInContext("Object.assign(globalThis, lib);", context);
  vm.runInContext(core, context, { filename: "yt.solver.core.js" });
  return context;
}

/**
 * Solve YouTube `n`/`sig` challenges with yt-dlp's EJS bundle.
 * `requests` is [{ type: "n" | "sig", challenges: string[] }].
 */
export function solve(player, requests) {
  const input = JSON.stringify({
    type: "player",
    player,
    requests,
    output_preprocessed: false,
  });
  const output = vm.runInContext(`jsc(${input})`, solverContext(), { timeout: 60_000 });
  if (output.type === "error") throw new Error(output.error);
  return output;
}
