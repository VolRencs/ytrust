let native;
let nativeLoadError;
try {
  native = await import("./native.js");
} catch (error) {
  nativeLoadError = error;
}

function requireNative() {
  if (!native) {
    const detail = nativeLoadError instanceof Error ? nativeLoadError.message : String(nativeLoadError ?? "unknown error");
    throw new YtrustError("NATIVE_LOAD", `native addon is unavailable: ${detail}`, { cause: nativeLoadError });
  }
  return native;
}

const ERROR_CODES = [
  ["AGE_RESTRICTED", /age.?restricted|confirm your age|inappropriate for some users/i],
  ["BOTCHECK", /confirm you.?re not a bot|sign in to confirm you.?re not a bot/i],
  ["PRIVATE", /private video|video is private/i],
  ["UNAVAILABLE", /unavailable|does not exist|removed|terminated/i],
  ["COOKIES", /cookie/i],
  ["UNSUPPORTED_URL", /not a YouTube URL|unsupported YouTube URL|only YouTube is supported|channel URLs are not supported/i],
  ["INVALID_INPUT", /invalid (url|playlist|search|download section)|search query is empty|playlist items start at 1|playlist URL has no list parameter/i],
  ["TIMEOUT", /timed out|timeout/i],
  ["NETWORK", /request failed|connection|dns|tls|network|unreachable/i],
  ["HTTP", /HTTP \d{3}|status \d{3}/i],
];

export class YtrustError extends Error {
  constructor(code, message, options) {
    super(message, options);
    this.name = "YtrustError";
    this.code = code;
  }
}

export function errorCode(message) {
  const text = String(message ?? "");
  for (const [code, pattern] of ERROR_CODES) {
    if (pattern.test(text)) return code;
  }
  return "UNKNOWN";
}

function toYtrustError(error) {
  if (error instanceof YtrustError) return error;
  const message = error instanceof Error ? error.message : String(error);
  return new YtrustError(errorCode(message), message, { cause: error });
}

export function probe() {
  return requireNative().probe();
}

export function version() {
  return requireNative().version();
}

export async function resolve(input, options = {}) {
  let raw;
  try {
    raw = await requireNative().resolve(String(input), options);
  } catch (error) {
    throw toYtrustError(error);
  }
  try {
    return JSON.parse(raw);
  } catch (error) {
    throw new YtrustError("INVALID_RESPONSE", "ytrust returned invalid JSON", { cause: error });
  }
}

/**
 * Resolve a single video and return its best playable audio stream together
 * with the HTTP headers required to fetch it.
 */
export async function resolveAudio(input, options = {}) {
  const result = await resolve(input, { ...options, noPlaylist: true });
  const audio = result?.best_audio;
  if (result?._type !== "video" || !audio) {
    throw new YtrustError("NO_AUDIO", `no audio format found for ${input}`);
  }
  return {
    ...audio,
    id: result.id,
    title: result.title,
    duration: result.duration,
    thumbnail: result.thumbnail,
    webpage_url: result.webpage_url,
  };
}
