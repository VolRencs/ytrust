import * as native from "./native.js";

export function probe() {
  return native.probe();
}

export function version() {
  return native.version();
}

export async function resolve(input, options = {}) {
  const json = await native.resolve(String(input), options);
  return JSON.parse(json);
}
