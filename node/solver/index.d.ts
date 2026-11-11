export type SolverRequest = {
  type: "n" | "sig";
  challenges: string[];
};

export type SolverResponse =
  | { type: "result"; data?: Record<string, string> }
  | { type: "error"; error?: string };

export type SolverOutput =
  | { type: "result"; responses: SolverResponse[]; preprocessed_player?: string }
  | { type: "error"; error?: string; preprocessed_player?: string };

export function solve(player: string, requests: SolverRequest[]): SolverOutput;

export function solveWithContext(player: string, requests: SolverRequest[], context: object): SolverOutput;
