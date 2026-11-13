import { solve } from "./index.js";

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    handle(line);
  }
});

function handle(line) {
  let request;
  try {
    request = JSON.parse(line);
  } catch (error) {
    write({ type: "error", error: `invalid solver request: ${error.message}` });
    return;
  }
  try {
    const output = solve(request.player ?? "", request.requests ?? [], {
      preprocessed: request.preprocessed,
    });
    write(output);
  } catch (error) {
    write({ type: "error", error: String(error?.message ?? error) });
  }
}

function write(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
