import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const lib = readFileSync(join(here, "lib.min.js"), "utf8");
const core = readFileSync(join(here, "core.min.js"), "utf8");

const SOLVE_TIMEOUT_MS = 15_000;

let context;

function solverContext() {
  if (context) return context;
  context = vm.createContext({});
  vm.runInContext(lib, context, { filename: "yt.solver.lib.js" });
  vm.runInContext("Object.assign(globalThis, lib);", context);
  vm.runInContext(core, context, { filename: "yt.solver.core.js" });
  return context;
}

/**
 * Solve YouTube `n`/`sig` challenges with yt-dlp's EJS bundle.
 * `requests` is [{ type: "n" | "sig", challenges: string[] }].
 *
 * The player source is passed as a VM global instead of being interpolated
 * into a script string, so remote player code cannot break out of a template
 * literal and the VM timeout still applies.
 */
export function solve(player, requests, options = {}) {
  return solveWithContext(player, requests, solverContext(), options);
}

export function solveWithContext(player, requests, ctx, options = {}) {
  const preprocessed = typeof options.preprocessed === "string" ? options.preprocessed : "";
  const input = preprocessed
    ? {
        type: "preprocessed",
        preprocessed_player: preprocessed,
        requests,
        output_preprocessed: false,
      }
    : {
        type: "player",
        player,
        requests,
        output_preprocessed: true,
      };
  ctx.__ytrust_input = JSON.stringify(input);
  let output;
  try {
    output = vm.runInContext("jsc(JSON.parse(__ytrust_input))", ctx, {
      filename: "yt.solver.run.js",
      timeout: SOLVE_TIMEOUT_MS,
    });
  } finally {
    delete ctx.__ytrust_input;
  }
  if (!output || typeof output !== "object") {
    throw new Error("challenge solver returned an invalid result");
  }
  if (output.type === "error") {
    throw new Error(String(output.error ?? "challenge solver failed"));
  }
  return output;
}
