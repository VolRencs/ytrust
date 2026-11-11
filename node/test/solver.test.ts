import assert from "node:assert/strict";
import test from "node:test";
import vm from "node:vm";
import { solveWithContext } from "../solver/index.js";

test("challenge solver passes a structured input object to jsc", () => {
  const ctx = vm.createContext({});
  vm.runInContext(
    `
    globalThis.jsc = (input) => {
      if (input === null || typeof input !== "object") {
        throw new TypeError("jsc expected an object");
      }
      if (typeof input.player !== "string") throw new Error("missing player");
      if (!Array.isArray(input.requests)) throw new Error("missing requests");
      return {
        type: "result",
        responses: input.requests.map((request) => ({
          type: "result",
          data: Object.fromEntries(request.challenges.map((challenge) => [challenge, challenge])),
        })),
        preprocessed_player: "preprocessed-source",
      };
    };
  `,
    ctx,
  );

  const output = solveWithContext("player source", [{ type: "n", challenges: ["abc"] }], ctx);

  assert.equal(output.type, "result");
  assert.equal(output.responses?.length, 1);
  assert.equal(output.responses?.[0]?.type, "result");
  assert.equal(output.responses?.[0]?.data?.abc, "abc");
  assert.equal(output.preprocessed_player, "preprocessed-source");
});

test("challenge solver accepts a preprocessed player", () => {
  const seen: { type?: string; preprocessed_player?: string }[] = [];
  const ctx = vm.createContext({
    record: (input: { type?: string; preprocessed_player?: string }) => seen.push(input),
  });
  vm.runInContext(
    `
    globalThis.jsc = (input) => {
      globalThis.record(input);
      if (!Array.isArray(input.requests)) throw new Error("missing requests");
      return {
        type: "result",
        responses: input.requests.map(() => ({ type: "result", data: {} })),
      };
    };
  `,
    ctx,
  );

  solveWithContext("player source", [{ type: "n", challenges: ["abc"] }], ctx);
  solveWithContext("player source", [{ type: "sig", challenges: ["def"] }], ctx, {
    preprocessed: "preprocessed-source",
  });

  assert.equal(seen.length, 2);
  assert.equal(seen[0]?.type, "player");
  assert.equal(seen[1]?.type, "preprocessed");
  assert.equal(seen[1]?.preprocessed_player, "preprocessed-source");
});
