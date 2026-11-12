import { readFileSync } from "node:fs";
import { solve } from "./index.js";

try {
  const input = JSON.parse(readFileSync(0, "utf8"));
  const output = solve(input.player ?? "", input.requests ?? [], {
    preprocessed: input.preprocessed,
  });
  process.stdout.write(JSON.stringify(output));
} catch (error) {
  const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
  process.stdout.write(JSON.stringify({ type: "error", error: message }));
}
