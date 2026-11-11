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
});
