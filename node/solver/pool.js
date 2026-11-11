import { availableParallelism } from "node:os";
import { Worker } from "node:worker_threads";

const TASK_TIMEOUT_MS = 20_000;
const configured = Number.parseInt(process.env.YTRUST_SOLVER_THREADS ?? "", 10);
const MAX_WORKERS = Math.max(
  1,
  Math.min(4, Number.isFinite(configured) && configured > 0 ? configured : availableParallelism()),
);

const slots = [];
const queue = [];
let nextId = 1;

function createSlot() {
  const worker = new Worker(new URL("./worker.js", import.meta.url), {
    resourceLimits: {
      maxOldGenerationSizeMb: 256,
      stackSizeMb: 4,
    },
  });
  const slot = { worker, task: null, timer: null, dead: false };
  worker.unref();
  worker.on("message", (message) => {
    if (slot.dead) return;
    const task = slot.task;
    if (!task || message?.id !== task.id) return;
    finishTask(slot);
    if (message.ok) task.resolve(message.output);
    else task.reject(new Error(String(message.error ?? "challenge solver failed")));
    dispatch();
  });
  worker.on("error", (error) => failSlot(slot, error));
  worker.on("exit", (code) => {
    if (!slot.dead) failSlot(slot, new Error(`solver worker exited with code ${code}`));
  });
  slots.push(slot);
  return slot;
}

function finishTask(slot) {
  if (slot.timer) clearTimeout(slot.timer);
  slot.timer = null;
  slot.task = null;
  slot.worker.unref();
}

function failSlot(slot, error) {
  if (slot.dead) return;
  slot.dead = true;
  const task = slot.task;
  finishTask(slot);
  const index = slots.indexOf(slot);
  if (index >= 0) slots.splice(index, 1);
  slot.worker.terminate().catch(() => {});
  if (task) {
    task.reject(error);
  } else if (slots.length === 0 && queue.length > 0) {
    for (const queued of queue.splice(0)) queued.reject(error);
  }
  dispatch();
}

function dispatch() {
  while (queue.length > 0) {
    let slot = slots.find((candidate) => !candidate.dead && !candidate.task);
    if (!slot && slots.length < MAX_WORKERS) {
      try {
        slot = createSlot();
      } catch (error) {
        queue.shift().reject(error);
        continue;
      }
    }
    if (!slot) return;
    const task = queue.shift();
    slot.task = task;
    slot.worker.ref();
    slot.timer = setTimeout(() => {
      failSlot(slot, new Error(`challenge solver timed out after ${TASK_TIMEOUT_MS} ms`));
    }, TASK_TIMEOUT_MS);
    try {
      slot.worker.postMessage({
        id: task.id,
        player: task.player,
        requests: task.requests,
      });
    } catch (error) {
      failSlot(slot, error);
    }
  }
}

export function solve(player, requests) {
  return new Promise((resolveTask, rejectTask) => {
    queue.push({
      id: nextId++,
      player,
      requests,
      resolve: resolveTask,
      reject: rejectTask,
    });
    dispatch();
  });
}
